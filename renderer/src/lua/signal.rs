//! The `Signal` reactive primitive (`oblisk-idl-api-specs.md` § 1.2): `signal:get()`,
//! `signal:map(fn)`, `signal:set(value)`, and the globals `computed(dependencies, fn)` and
//! `state(name, initial)` (ADR-0044 decision 5).
//!
//! Lua never constructs a bare `Signal` itself -- § 1.2 exposes it as Rust-owned userdata handed
//! *to* Lua, never built *by* Lua from a raw value.
//!
//! ponytail: `computed`/`map` recompute their function fresh on every `:get()` call -- no
//! memoization, no dependency-invalidation graph. Deciding *when* a cached computed value goes
//! stale is the Watcher's job (`CONTEXT.md`, Watcher), not this loader's. Recomputing on every
//! read is the correct, simple baseline until something needs the graph.
//!
//! `computed(dependencies, fn)` calls `fn` with each dependency's *current value* as a positional
//! argument, not the `Signal` handles themselves, so a `computed` body doesn't have to
//! redundantly call `:get()` on each of its own dependencies.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use crate::lua::marshal;

/// § 1.2: "CPU runtime is capped at 5ms per evaluation."
const CPU_CAP: Duration = Duration::from_millis(5);

/// One live evaluation's allowance, on both clocks.
///
/// `cpu` is the one § 1.2 specifies and the only one that decides. `wall` is a pre-filter and
/// nothing else: a thread's CPU time advances at most as fast as the clock, so an unexpired wall
/// deadline *proves* an unexpired CPU one and [`Deadline::expired`] can answer "keep going"
/// without a syscall. Measured on an Intel laptop, `Instant::now()` is 23.6ns against
/// `CLOCK_THREAD_CPUTIME_ID`'s 170.4ns, and the hook fires every
/// [`CHECK_EVERY_N_INSTRUCTIONS`] instructions, so the common path pays the cheap clock.
///
/// This was wall clock alone, which made the constant's name and § 1.2 both wrong: a descheduled
/// evaluation was charged for time it did not run. On a 12-thread machine running the whole test
/// suite that fired on configs a quiet machine evaluates in microseconds, 5 times in 53 full
/// runs, a different test each time.
///
/// Nothing is lost by not counting the wait. A thread parked in a syscall executes no
/// instructions, so the hook never fired during one anyway; ADR-0048 says so and answers it the
/// only way that works, by removing every call that can block: `io` is absent from
/// `lua::config_stdlib`, and `lua::restrict_os` keeps only four `os` calls, all of which return
/// without waiting.
#[derive(Clone, Copy)]
struct Deadline {
    wall: Instant,
    /// `None` when the clock could not be read, which leaves `wall` authoritative on its own.
    cpu: Option<Duration>,
}

impl Deadline {
    /// [`CPU_CAP`] from now, on both clocks.
    fn starting_now() -> Self {
        Self { wall: Instant::now() + CPU_CAP, cpu: thread_cpu_time().map(|used| used + CPU_CAP) }
    }

    fn expired(&self) -> bool {
        if Instant::now() <= self.wall {
            return false;
        }
        // Past the wall pre-filter, so the CPU clock decides. An unreadable clock expires: a cap
        // that cannot measure must fire rather than quietly stop existing, and falling back to
        // the wall deadline is exactly the behaviour this replaced.
        self.cpu.is_none_or(|deadline| thread_cpu_time().is_none_or(|used| used > deadline))
    }
}

/// How much CPU the calling thread has burned, which is what § 1.2's cap is written against.
///
/// Per *thread*, not per process: one evaluation runs start to finish on the thread that entered
/// it, and the Lua VM is single-threaded by construction (docs/adr/0039 puts it on the Wayland
/// thread). A process-wide clock would charge a config for the shaping worker.
fn thread_cpu_time() -> Option<Duration> {
    let spent = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    Some(Duration::new(spent.tv_sec().try_into().ok()?, spent.tv_nsec().try_into().ok()?))
}

/// How many VM instructions run between budget checks. `mlua`'s `HookTriggers` docs warn a low
/// value "can incur a very high overhead"; 1000 keeps the check cheap while still catching a
/// runaway closure within roughly one instruction-batch of the 5ms mark, not after it's spun for
/// seconds.
const CHECK_EVERY_N_INSTRUCTIONS: u32 = 1000;

/// The recursion bound for [`Signal::get_value`]'s `Computed` arm: how many of those calls may be
/// live on the stack at once before the next one returns an `mlua::Error` instead of recursing
/// further -- the same "at most N levels" boundary `layout::scene`'s `MAX_TREE_DEPTH` uses.
///
/// It bounds *both* shapes of `get_value` recursion, because [`CpuBudget::enter`] runs before
/// dependency resolution rather than around the closure call alone:
/// - body nesting: a `computed`/`map` body calling `:get()` on another signal, directly or
///   through a self- or mutually-referential cycle; and
/// - dependency chains: `s:map(f):map(g):...`, which resolve each dependency recursively and
///   nest no Lua call at all. An earlier version pushed the deadline only around the closure
///   call, so a chain nested Rust frames while the deadline stack never grew past 1 -- a 200-link
///   chain reached `get_value` depth 200 with a measured max stack depth of 1, and a 5000-link
///   chain aborted the process with `fatal runtime error: stack overflow`.
///
/// The 5ms CPU cap alone cannot be this bound: a dependency chain does no Lua work between
/// levels, so the instruction hook may never fire before the guard page does.
///
/// A legitimate dependency chain is shallow, so 32 is generous headroom. See
/// `layout::scene::MAX_TREE_DEPTH` for the measured compounded stack cost of this constant and
/// that one together, which is what actually sets both.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// The one message both cap gates raise, so a caller matching on it does not have to know which
/// gate fired (the hook mid-call, or [`CpuBudget::check_not_exceeded`] at the Rust boundary).
const CPU_CAP_EXCEEDED: &str = "computed/map exceeded its 5ms CPU budget";

#[derive(Clone)]
enum SignalKind {
    // ponytail: only `Signal::try_new_direct` constructs this variant, and that constructor has
    // no production caller yet either -- exercised by tests only.
    #[allow(dead_code)]
    Direct(Value),
    Computed { deps: Vec<Signal>, func: Function },
    /// A value Rust can overwrite after construction (`Signal::new_live`/`LiveSignalHandle`).
    /// `Rc<RefCell<_>>`, not `Arc<Mutex<_>>`: the `Loader` this lives on stays confined to one
    /// dedicated OS thread (the Wayland dispatch thread, docs/adr/0039).
    Live(Rc<RefCell<Value>>),
    /// The engine's own reactive state: a boolean `crate::wayland`'s pointer handler writes and a
    /// config only reads, built by the `hover(name)` global (docs/adr/0062).
    ///
    /// Structurally identical to [`SignalKind::Live`] and deliberately not it. The two differ in
    /// who may write them, which is the whole point of the split: `Signal::hover_handle` hands out
    /// a write end for this variant only, so a config binding `hover = oblisk.network` gets no
    /// writer rather than a pointer that overwrites a capability snapshot.
    ///
    /// `paired_rect` is the other half of one hover slot: the boolean `hover(name)` returns carries
    /// a reference to the rect cell `hover_rect(name)` reads, so the pointer handler can write both
    /// from the one handle a node's `hover` property gave it. `None` on the rect signal itself,
    /// which is a hover signal in every other respect and is not a trigger for anything.
    Hover { cell: Rc<RefCell<Value>>, paired_rect: Option<Rc<RefCell<Value>>>, dirty: DirtyFlag },
    /// Lua-authored state (ADR-0044 decision 5): the one signal kind `Signal::set` accepts, built
    /// by the `state(name, initial)` global and written from a config's own `on_click`.
    ///
    /// A fourth variant rather than a reuse of `Live`, even though the storage is identical.
    /// ADR-0044 makes capability signals read-only to Lua, and a `set` that accepted `Live` would
    /// let a config overwrite the network SSID the Supervisor just pushed, with every reader
    /// downstream believing it. Keeping the writable kind distinct makes that rule a fact the
    /// type system holds rather than a comment `set` has to remember, and makes it testable.
    ///
    /// Carries its own `DirtyFlag` clone rather than reaching for one at write time: `set` is a
    /// `UserData` method with no `RendererClient` in reach, and every signal in a generation
    /// shares the one flag decision 2 specifies anyway.
    State { cell: Rc<RefCell<Value>>, dirty: DirtyFlag },
}

impl SignalKind {
    /// What a refused [`Signal::set`] calls this kind when it explains itself to a config author.
    fn describe(&self) -> &'static str {
        match self {
            SignalKind::Direct(_) => "a direct",
            SignalKind::Computed { .. } => "a computed",
            SignalKind::Live(_) => "a capability",
            SignalKind::Hover { .. } => "a hover",
            SignalKind::State { .. } => "a state",
        }
    }
}

/// The marshalling boundary (`marshal.rs`, § 1.1) applied to one Lua-authored value: the types
/// § 1.1 constrains (`Number`/`Integer`/`String`) are checked, every other shape passes through
/// untouched.
///
/// Shared by [`Signal::try_new_direct`], [`Signal::new_state`] and `set`, which all guard a value
/// hand-authored in Lua crossing into Rust. `Signal::new_live` deliberately does not, since its
/// value came out of a Rust struct.
fn check_lua_authored(value: &Value) -> Result<(), marshal::MarshalError> {
    match value {
        Value::Number(n) => {
            marshal::check_number(*n)?;
        }
        Value::Integer(i) => {
            marshal::check_integer(*i)?;
        }
        Value::String(s) => {
            marshal::check_string(&s.to_string_lossy())?;
        }
        _ => {}
    }
    Ok(())
}

/// A read-only reactive value. Wraps either a plain value (`Direct`, Rust-pushed) or a Lua
/// closure recomputed against its dependencies' current values on every `get()` (`Computed`).
#[derive(Clone)]
pub struct Signal(SignalKind);

impl Signal {
    /// Wraps `value` as a `Direct` signal, enforcing the marshalling boundary (`marshal.rs`) on
    /// the types it constrains (`Number`/`Integer`/`String`); every other shape passes through
    /// untouched.
    ///
    /// ponytail: no production caller yet -- every live value pushed today goes through
    /// `Signal::new_live` instead (Rust-pushed, not Lua-authored, so this boundary doesn't apply
    /// the same way). A real Lua-constructed `Direct` signal is still a future phase's job.
    /// Exercised by this module's tests only.
    #[allow(dead_code)]
    pub fn try_new_direct(value: Value) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&value)?;
        Ok(Signal(SignalKind::Direct(value)))
    }

    /// The signal behind `state(name, initial)` (ADR-0044 decision 5): writable from Lua through
    /// `signal:set(value)`, which marks `dirty` so the next poll turn re-resolves.
    ///
    /// Marshal-checked, unlike [`Self::new_live`]: `initial` is written in `shell.lua`, so it is
    /// a hand-authored value crossing into Rust, the boundary `marshal.rs` exists for.
    pub fn new_state(initial: Value, dirty: DirtyFlag) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&initial)?;
        Ok(Signal(SignalKind::State { cell: Rc::new(RefCell::new(initial)), dirty }))
    }

    /// A signal Rust can push new values into after construction via the paired
    /// [`LiveSignalHandle`] -- `try_new_direct`'s marshalling checks don't apply here: the value
    /// arrives already `serde_json`-serialized from a Rust struct, which can't produce a
    /// NaN/Inf/oversized string the way hand-authored Lua can.
    ///
    /// `dirty` is the scene-dirty flag (ADR-0044 decision 2) the paired [`LiveSignalHandle`]
    /// marks on every `set`. Every live signal in one generation shares the same `DirtyFlag`
    /// (`renderer/src/socket.rs`'s `RendererClient` holds the clone that reads and clears it):
    /// decision 2's "one flag for the whole scene" means there is exactly one to share.
    pub fn new_live(initial: Value, dirty: DirtyFlag) -> (Self, LiveSignalHandle) {
        let cell = Rc::new(RefCell::new(initial));
        (Signal(SignalKind::Live(Rc::clone(&cell))), LiveSignalHandle(cell, dirty))
    }

    /// The signal `hover(name)` builds: a boolean the *engine* writes from `wl_pointer`, read-only
    /// to Lua (docs/adr/0062 decision 2).
    ///
    /// Its own kind rather than a second [`Self::new_live`] caller, and the variant earns its place
    /// at both ends. `signal:set()` refuses it by name, so a config is told it is holding a hover
    /// slot rather than a capability; and [`Self::hover_handle`] answers `None` for every other
    /// kind, so a config writing `hover = oblisk.network` cannot get the pointer to overwrite a
    /// capability's snapshot.
    ///
    /// Starts `false`, not nil: a config binds this straight to `visible`, and a signal resolving
    /// to nil means the property is *absent* (ADR-0044 decision 1's amendment), which is not what
    /// "the pointer is not here" should mean.
    ///
    /// `initial_rect` is that same rule applied to the other half, and it is not optional. A
    /// tooltip binds `anchor_rect` to the rect signal, § 6.3 makes `anchor_rect` required and
    /// non-zero, and the popup resolves from the first frame -- long before any pointer has been
    /// near it. A nil here reads as "no `anchor_rect` at all" and the popup is refused on every
    /// re-resolve until something hovers, which is what a live run reported before this argument
    /// existed. The caller passes a real 1x1 rect; this constructor takes it rather than building
    /// it because a `Value::Table` needs a `Lua` and this does not have one.
    pub fn new_hover(dirty: DirtyFlag, initial_rect: Value) -> (Self, Self) {
        let over = Rc::new(RefCell::new(Value::Boolean(false)));
        let rect = Rc::new(RefCell::new(initial_rect));
        (
            Signal(SignalKind::Hover { cell: Rc::clone(&over), paired_rect: Some(Rc::clone(&rect)), dirty: dirty.clone() }),
            Signal(SignalKind::Hover { cell: rect, paired_rect: None, dirty }),
        )
    }

    /// The write end of a hover signal, for `crate::wayland`'s pointer handler. `None` for every
    /// other kind -- see [`Self::new_hover`] for why that refusal is the point rather than a
    /// missing case.
    pub(crate) fn hover_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Hover { cell, dirty, .. } => Some(LiveSignalHandle(Rc::clone(cell), dirty.clone())),
            _ => None,
        }
    }

    /// The write end of the rect half of this hover slot: where the node carrying it last was, in
    /// its surface's logical coordinates, which is what a tooltip `popup` binds `anchor_rect` to.
    ///
    /// `None` for every kind but the boolean half of a hover slot -- including the rect half, which
    /// no node's `hover` property should be naming.
    pub(crate) fn hover_rect_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Hover { paired_rect: Some(rect), dirty, .. } => Some(LiveSignalHandle(Rc::clone(rect), dirty.clone())),
            _ => None,
        }
    }

    /// The signal `map(f)` builds: a `Computed` with this one as its only dependency, recomputed
    /// on every read like any other (ADR-0044 decision 3, no memoization).
    ///
    /// Lifted out of the Lua-facing `map` method so a Rust caller can build the same thing --
    /// `lua::capability::Capability` delegates its own `map` here, so `oblisk.lock` reads exactly
    /// like the bare capability globals beside it instead of a second, drift-prone construction.
    pub(crate) fn mapped(&self, func: Function) -> Signal {
        Signal(SignalKind::Computed { deps: vec![self.clone()], func })
    }

    /// Reads this signal's current value (ADR-0044 decision 1). `pub(crate)`, not private:
    /// `layout::node`'s property parsers call this directly to resolve a `Signal` userdata found
    /// in a property slot instead of rejecting it. `&Lua` is threaded in rather than recovered
    /// from `self`, because a `Computed` signal's closure runs under a [`CpuBudget`], which needs
    /// a real `Lua` to install its instruction-count hook on -- mlua 0.12 exposes no way to
    /// recover a `Lua` from an `AnyUserData`/`Value`.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        match &self.0 {
            SignalKind::Direct(value) => Ok(value.clone()),
            SignalKind::Live(cell) => Ok(cell.borrow().clone()),
            SignalKind::Hover { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::State { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::Computed { deps, func } => {
                // Entered *before* dependency resolution, not around `func.call` alone: this
                // makes the deadline-stack depth equal the true `get_value` nesting depth, so
                // MAX_SIGNAL_NESTING_DEPTH bounds dependency chains as well as body nesting, and
                // keeps the outermost deadline live across the whole dependency graph rather than
                // restarting the 5ms clock at every leaf.
                let budget = CpuBudget::enter(lua)?;

                // ponytail: resolving every dependency on every read, with no memoization
                // (ADR-0044 decision 3), makes evaluation exponential in graph depth for a
                // diamond-shaped graph -- `computed({s, s}, f)` stacked N deep evaluates `s` 2^N
                // times, measured at 1,048,575 closure calls for N = 20. The 5ms budget entered
                // just above is what makes that survivable: shared by the whole graph rather than
                // restarting per leaf, so the evaluation is cut off with an error. Memoizing a
                // dependency's resolved value for one outermost `get_value` is the upgrade path
                // if a real config ever grows a diamond deep enough to notice; not built now.
                let mut args = Vec::with_capacity(deps.len());
                for dep in deps {
                    args.push(dep.get_value(lua)?);
                }
                let value = func.call::<Value>(MultiValue::from_vec(args))?;
                budget.check_not_exceeded()?;
                Ok(value)
            }
        }
    }
}

/// The Rust-side handle to a [`Signal::new_live`] signal's storage: lets Rust push a new value in
/// after construction, e.g. on every received `StateSnapshot`. The paired `Signal` (Lua-side)
/// always reads whatever was last set here -- no memoization, matching every other `Signal` kind
/// in this file.
#[derive(Clone)]
pub struct LiveSignalHandle(Rc<RefCell<Value>>, DirtyFlag);

impl LiveSignalHandle {
    /// Writes `value` and marks the shared scene dirty (ADR-0044 decision 2): every push has to
    /// make the next poll turn re-resolve the whole scene, since decision 3 rejects a per-signal
    /// dependency graph that could narrow that down.
    pub fn set(&self, value: Value) {
        *self.0.borrow_mut() = value;
        self.1.mark();
    }

    /// [`Self::set`], except that storing the value already there does nothing at all: no write,
    /// no dirty mark, and it answers `false`.
    ///
    /// docs/adr/0062 decision 4. The pointer handler calls this on every `wl_pointer` motion
    /// event, which arrives at device rate, and one mark re-resolves every surface in the
    /// generation (ADR-0044 decision 2). Comparing first turns that into one re-resolve per
    /// hover boundary crossed rather than one per motion event.
    pub fn set_changed(&self, value: Value) -> bool {
        let unchanged = *self.0.borrow() == value;
        if unchanged {
            return false;
        }
        *self.0.borrow_mut() = value;
        self.1.mark();
        true
    }
}

/// The one scene-dirty flag ADR-0044 decision 2 specifies: a single `bool`, shared by every
/// [`LiveSignalHandle`] in a generation and by the `RendererClient` that reads and clears it, not
/// a per-signal or per-surface set. `Rc<Cell<bool>>`, not `Arc<AtomicBool>`: this lives entirely
/// on the Wayland dispatch thread (docs/adr/0039).
///
/// `ponytail:` one flag for the whole scene means any push re-resolves every surface, including
/// one that reads nothing from the capability that changed. The upgrade path is a per-surface
/// flag keyed on which signals a surface actually reads, which needs read-tracking that doesn't
/// exist yet.
#[derive(Clone)]
pub struct DirtyFlag(Rc<Cell<bool>>);

impl DirtyFlag {
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    /// `pub(crate)`: a `configure` carrying a new size for one surface instance means exactly
    /// what a capability push means -- the resolved geometry no longer matches its inputs -- so
    /// `crate::socket::RendererClient::set_instance_size` marks this same flag rather than adding
    /// a second mechanism beside it (ADR-0044 decision 2).
    pub(crate) fn mark(&self) {
        self.0.set(true);
    }

    /// Reads and clears the flag in one step -- the "drain first, then re-resolve once" rule
    /// (ADR-0044 decision 2): `wayland::run`'s poll loop drains every pending inbound frame
    /// before calling this once, coalescing a burst of `StateSnapshot` pushes into a single
    /// re-resolve instead of one per push.
    pub fn take(&self) -> bool {
        self.0.replace(false)
    }
}

impl Default for DirtyFlag {
    fn default() -> Self {
        Self::new()
    }
}

impl UserData for Signal {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| this.get_value(lua));
        methods.add_method("map", |_, this, f: Function| Ok(this.mapped(f)));
        // ADR-0044 decision 5's write path, and the only one Lua has. Every other kind is refused
        // by name rather than by a type error, so `network:set(...)` says *why* it refuses.
        methods.add_method("set", |_, this, value: Value| {
            let SignalKind::State { cell, dirty } = &this.0 else {
                return Err(mlua::Error::runtime(format!(
                    "signal:set() is only valid on a state(name, initial) signal, and this is {} signal: every other signal kind is read-only to Lua (docs/adr/0044 decision 5)",
                    this.0.describe()
                )));
            };
            // Checked before the write, so a refused value leaves the stored one alone and marks
            // nothing -- the same Lua-authored boundary `Signal::new_state` puts on `initial`.
            check_lua_authored(&value).map_err(|err| {
                mlua::Error::runtime(format!("signal:set() refused its value at the marshalling boundary: {err}"))
            })?;
            *cell.borrow_mut() = value;
            dirty.mark();
            Ok(())
        });
    }
}

/// Whether this VM's config ever called `hover(name)`.
///
/// `crate::wayland`'s pointer handler asks before doing any hover work at all, so a config with no
/// tooltip and no expand-on-hover pays nothing for the feature existing: no tree clone, no walk,
/// no signal writes, on an event that arrives at pointer-report rate.
pub fn any_hover_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<HoverRegistry>().is_some_and(|registry| !registry.0.is_empty())
}

/// The `name -> Signal` map ADR-0044 decision 5 hangs `state` off: the *name* is the identity, so
/// re-running the config on an in-place reload finds the signal it built last time still holding
/// whatever the user's last click left in it, and an open dropdown stays open across a config edit.
///
/// Lives in `Lua::set_app_data`, the same per-`Lua` storage [`CpuBudget::enter`] keeps its
/// deadline stack in, so it needs no explicit lifetime management: decision 4 keeps the VM alive
/// across an in-place reload, and a generation swap is a new process with a new VM, which is
/// decision 5's "named state dies on a generation swap" falling out for free.
#[derive(Default)]
struct StateRegistry(HashMap<String, Signal>);

/// `state`'s registry, for `hover(name)` (docs/adr/0062 decision 2). Separate map, same rule and
/// the same lifetime: the name is the identity, so an in-place reload finds the signal it built
/// last time and a tooltip open across a `config/theme.lua` edit stays open.
///
/// Not shared with [`StateRegistry`]: one name space would let `state("volume", 0)` and
/// `hover("volume")` collide, and the collision would surface as `signal:set()` refusing a name
/// the config thought it owned.
#[derive(Default)]
struct HoverRegistry(HashMap<String, (Signal, Signal)>);

/// An RAII claim on the 5ms evaluation budget, held for one `Computed` [`Signal::get_value`] --
/// dependency resolution *and* the closure call, not the closure call alone. Entering pushes a
/// deadline onto a per-`Lua` stack kept in `app_data`; dropping pops it.
///
/// A stack, rather than mlua's single hook slot, because this is reentrant three ways: a
/// dependency is itself a `Computed` signal, a `computed`/`map` body reads a *second* `Signal`
/// (an upvalue or a global, not just its declared `dependencies`), and a `computed` can be
/// self- or mutually-referential. `Lua::set_hook`/`set_global_hook` keep one unstacked callback,
/// so installing and removing per call meant an inner call's removal silently stripped the outer
/// call's still-active cap. The hook is therefore installed only on the 0->1 transition and
/// removed only on the 1->0 transition: a finished inner call leaves the outer deadline in the
/// stack, and enforcement hands back to the outer call instead of vanishing.
///
/// The governing deadline is `stack[0]`, the outermost live call's -- not the innermost
/// (`stack.last()`, this code's original design). Reading the innermost meant a monotonically
/// deepening recursion always saw the *freshest* deadline, recomputed from `Instant::now()` at
/// every new level's own push, so the check could never observe an expired budget no matter how
/// long the whole chain ran. `stack[0]` makes "5ms" mean what § 1.2 says: a budget for the
/// evaluation as a whole, not a per-level allowance that resets on every recursive `:get()`.
///
/// `stack[0]` is also, specifically, the *minimum*, so this is `first()` and not `iter().min()`:
/// every entry is [`Deadline::starting_now`], and entries are pushed and popped strictly LIFO, so
/// the vector is non-decreasing and its first element is its minimum. Both clocks are monotonic,
/// so that holds on each of the pair independently and the argument survived them becoming two.
/// It matters because the hook runs every [`CHECK_EVERY_N_INSTRUCTIONS`] instructions: `first()`
/// is O(1) where `min()` walked up to [`MAX_SIGNAL_NESTING_DEPTH`] entries per fire. Do not "fix"
/// it to `last()` either -- that is the per-level reset this design exists to avoid.
struct CpuBudget<'lua> {
    lua: &'lua Lua,
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`].
    ///
    /// The hook is installed *before* the deadline is pushed, so no early return can leave the
    /// stack unbalanced: an install failure returns with nothing pushed and nothing to pop, where
    /// pushing first would have stranded an entry that the depth-1 branch never revisits, silently
    /// disabling the cap for the rest of the VM's life. No Lua runs between the install and the
    /// push, and the hook tolerates an empty stack.
    fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        if lua.app_data_ref::<Vec<Deadline>>().is_none() {
            lua.set_app_data(Vec::<Deadline>::new());
        }
        let depth = lua.app_data_ref::<Vec<Deadline>>().expect("just ensured the deadline stack exists").len();
        if depth >= MAX_SIGNAL_NESTING_DEPTH {
            return Err(mlua::Error::runtime(format!(
                "signal nesting exceeded its maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels -- a computed/map chain recursing into itself, or a dependency chain that long?"
            )));
        }
        if depth == 0 {
            // `set_global_hook`, not `set_hook`: mlua's per-thread hook is keyed by Lua thread, so
            // a coroutine created by a `computed` body inherited the hook C function from
            // `lua_newthread` but found no callback for itself and *disabled* the hook on that
            // thread -- measured 5.75s of uninterrupted Lua inside `coroutine.create`/`resume`,
            // returning `Ok`. `HookKind::Global` stores one callback on the `Lua` itself, read
            // whichever thread it fires on, so the inherited hook resolves and covers coroutines.
            lua.set_global_hook(
                mlua::HookTriggers {
                    every_nth_instruction: Some(CHECK_EVERY_N_INSTRUCTIONS),
                    ..mlua::HookTriggers::new()
                },
                |lua, _| {
                    if governing_deadline_expired(lua) {
                        Err(mlua::Error::runtime(CPU_CAP_EXCEEDED))
                    } else {
                        Ok(mlua::VmState::Continue)
                    }
                },
            )?;
        }
        lua.app_data_mut::<Vec<Deadline>>().expect("just ensured the deadline stack exists").push(Deadline::starting_now());
        Ok(Self { lua })
    }

    /// The second gate on the 5ms budget, at the Rust boundary rather than inside the VM. The
    /// hook raises an ordinary Lua error inside the running function, so a `pcall` in a
    /// `computed` body catches it and carries on -- measured, a body looping over
    /// `pcall(function() ... end)` ran 7.5x the cap and returned a partially computed `Ok` value.
    /// Checking the governing deadline again once the call has returned makes a
    /// caught-and-ignored hook error an `Err` anyway: config Lua can catch the hook, but it
    /// cannot catch this.
    ///
    /// ponytail: this gate only runs when the call *returns*. A body that both swallows the hook
    /// error and never returns (`while true do pcall(f) end`) still spins until a hook fire
    /// happens to land on an instruction outside the `pcall`, eventually but not promptly.
    /// Bounding that properly needs preemption this VM cannot offer from inside itself; the
    /// upgrade path is the generation-swap process boundary (docs/adr/0039), which can kill a
    /// wedged renderer outright.
    fn check_not_exceeded(&self) -> mlua::Result<()> {
        if governing_deadline_expired(self.lua) {
            return Err(mlua::Error::runtime(CPU_CAP_EXCEEDED));
        }
        Ok(())
    }
}

impl Drop for CpuBudget<'_> {
    fn drop(&mut self) {
        let remaining = {
            let mut stack =
                self.lua.app_data_mut::<Vec<Deadline>>().expect("CpuBudget::enter always runs before its Drop");
            stack.pop();
            stack.len()
        };
        if remaining == 0 {
            // Both, in this order: `remove_global_hook` clears the callback so an inheriting
            // coroutine stops calling back, and `remove_hook` clears the mask on *this* thread
            // now, so unrelated Lua after a finished evaluation isn't paying for a hook that
            // would otherwise just no-op.
            self.lua.remove_global_hook();
            self.lua.remove_hook();
        }
    }
}

/// Whether the outermost live [`CpuBudget`]'s deadline has passed. `first()`, not `min()` or
/// `last()`: see [`CpuBudget`]'s doc comment for why. An empty stack (no evaluation in flight) is
/// never expired, which is what lets [`CpuBudget::enter`] install the hook before its first push.
fn governing_deadline_expired(lua: &Lua) -> bool {
    lua.app_data_ref::<Vec<Deadline>>().and_then(|stack| stack.first().copied()).is_some_and(|deadline| deadline.expired())
}

/// The one answer to "does this Lua userdata resolve like a signal?", and the `Signal` to
/// resolve it through.
///
/// Two userdata types answer yes. [`Signal`] is the obvious one. `capability::Capability` is the
/// other, and it is why this function exists: every § 2 capability sits behind a `Capability` so
/// the same object can be read and commanded, which means `computed({oblisk.audio}, f)` and
/// `content = oblisk.mpris` hand the engine a `Capability` rather than a bare `Signal`. Without
/// one shared answer here, every live capability binding in every config would be a value the
/// resolver skipped, and a skipped property is a *literal*, so every bar would freeze at its
/// first frame with nothing logged.
pub fn from_userdata(ud: &mlua::AnyUserData) -> Option<Signal> {
    if let Ok(signal) = ud.borrow::<Signal>() {
        return Some(signal.clone());
    }
    Some(ud.borrow::<crate::lua::capability::Capability>().ok()?.signal())
}

/// [`from_userdata`] without the clone, for callers that only need the question answered. The
/// two must agree on which types are signals, which `from_userdata_and_is_signal_agree` asserts.
pub fn is_signal(ud: &mlua::AnyUserData) -> bool {
    ud.is::<Signal>() || ud.is::<crate::lua::capability::Capability>()
}

/// Registers the `computed(dependencies, fn)` global (§ 1.2) and the `state(name, initial)`
/// global (ADR-0044 decision 5). `dependencies` must be an array of `Signal` userdata handles.
///
/// `dirty` is decision 2's one scene-dirty flag, taken explicitly rather than fished out of
/// `app_data` at call time: a hidden coupling that fails inside a config author's own `state()`
/// call is worse than threading one argument through. It must be the same flag `Signal::new_live`
/// hands out and `RendererClient` drains, or a `:set()` would mark a flag nothing reads.
pub fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    let hover_dirty = dirty.clone();
    let rect_dirty = dirty.clone();
    lua.globals().set(
        "computed",
        lua.create_function(|_, (deps, func): (Table, Function)| {
            let mut collected = Vec::new();
            for dep in deps.sequence_values::<mlua::AnyUserData>() {
                let dep = dep?;
                // Named rather than left to `borrow`'s own type error, which reports the mlua
                // type name of whatever was passed and never says what was expected.
                let signal = from_userdata(&dep).ok_or_else(|| {
                    mlua::Error::runtime("computed() dependencies must be Signals or `oblisk` capabilities, § 1.2")
                })?;
                collected.push(signal);
            }
            Ok(Signal(SignalKind::Computed { deps: collected, func }))
        })?,
    )?;
    lua.globals().set(
        "state",
        lua.create_function(move |lua, (name, initial): (String, Value)| {
            if lua.app_data_ref::<StateRegistry>().is_none() {
                lua.set_app_data(StateRegistry::default());
            }
            let existing =
                lua.app_data_ref::<StateRegistry>().expect("just ensured the state registry exists").0.get(&name).cloned();
            if let Some(signal) = existing {
                // Decision 5: a name already in the map wins and `initial` is ignored, which is
                // what makes an in-place reload keep the value instead of resetting it.
                return Ok(signal);
            }
            let signal = Signal::new_state(initial, dirty.clone()).map_err(|err| {
                mlua::Error::runtime(format!("state(\"{name}\", ...) refused its initial value at the marshalling boundary: {err}"))
            })?;
            lua.app_data_mut::<StateRegistry>()
                .expect("just ensured the state registry exists")
                .0
                .insert(name, signal.clone());
            Ok(signal)
        })?,
    )?;
    lua.globals().set(
        "hover",
        lua.create_function(move |lua, name: String| {
            if lua.app_data_ref::<HoverRegistry>().is_none() {
                lua.set_app_data(HoverRegistry::default());
            }
            Ok(hover_slot(lua, &hover_dirty, name)?.0)
        })?,
    )?;
    lua.globals().set(
        "hover_rect",
        lua.create_function(move |lua, name: String| Ok(hover_slot(lua, &rect_dirty, name)?.1))?,
    )
}

/// One hover slot by name, built on first ask: the boolean `hover(name)` returns and the rect
/// `hover_rect(name)` returns, in that order (docs/adr/0062 decision 2).
///
/// The name is the identity, exactly as `state(name, initial)` does it (ADR-0044 decision 5), which
/// is what carries a hover across an in-place reload and what lets two files reach one slot. Both
/// globals build the pair, so naming the rect first is not a different slot from naming the boolean
/// first.
///
/// No marshalling check on either initial: the only values these ever hold are the booleans and the
/// rect table the pointer handler writes, none of them hand-authored in Lua.
fn hover_slot(lua: &Lua, dirty: &DirtyFlag, name: String) -> mlua::Result<(Signal, Signal)> {
    if lua.app_data_ref::<HoverRegistry>().is_none() {
        lua.set_app_data(HoverRegistry::default());
    }
    let existing = lua.app_data_ref::<HoverRegistry>().expect("just ensured the hover registry exists").0.get(&name).cloned();
    if let Some(slot) = existing {
        return Ok(slot);
    }
    let slot = Signal::new_hover(dirty.clone(), Value::Table(unhovered_rect(lua)?));
    lua.app_data_mut::<HoverRegistry>().expect("just ensured the hover registry exists").0.insert(name, slot.clone());
    Ok(slot)
}

/// What `hover_rect(name)` reads before the pointer has ever been on its node: a 1x1 rect at the
/// origin.
///
/// Non-zero on both axes because § 6.3 refuses a zero `anchor_rect`, and a real table rather than
/// nil because a nil property is an *absent* one. A tooltip bound to this resolves from the first
/// frame and simply sits at the origin, which nothing ever sees: it is `visible = hover(name)`, and
/// that is false until the same event that replaces this with the node's real rect.
fn unhovered_rect(lua: &Lua) -> mlua::Result<mlua::Table> {
    let rect = lua.create_table()?;
    rect.set("x", 0.0)?;
    rect.set("y", 0.0)?;
    rect.set("width", 1.0)?;
    rect.set("height", 1.0)?;
    Ok(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_signal(name: &str, value: Value) -> Lua {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let signal = Signal::try_new_direct(value).unwrap();
        lua.globals().set(name, signal).unwrap();
        lua
    }

    /// A VM whose `state` global marks the returned flag, so a test can assert on the same flag
    /// `RendererClient` would be draining.
    fn lua_with_state() -> (Lua, DirtyFlag) {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        (lua, dirty)
    }

    #[test]
    fn hover_returns_a_read_only_boolean_signal_that_starts_false() {
        // docs/adr/0062 decision 2: the engine writes this one, so a config that reads it before
        // the pointer has ever been over the node must get `false`, not nil -- `visible` binds to
        // it directly and a nil there would mean "absent" (ADR-0044 decision 1's amendment).
        let (lua, _dirty) = lua_with_state();
        let started: bool = lua.load(r#"return hover("volume"):get()"#).eval().unwrap();
        assert!(!started);
    }

    #[test]
    fn hover_hands_the_same_name_the_same_signal_so_an_in_place_reload_keeps_it_open() {
        // The `state(name, initial)` rule of ADR-0044 decision 5, applied to hover by
        // docs/adr/0062 decision 2: the name is the identity, so re-running the config finds the
        // signal it built last time rather than a fresh false.
        //
        // Asserted through the storage rather than with `==`, which on two userdata handles is
        // object identity and answers false for `state(...)` too. What has to hold is that a write
        // to the slot one call named is read by the other.
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"first = hover("volume") second = hover("volume") other = hover("battery")"#).exec().unwrap();

        let first: mlua::AnyUserData = lua.globals().get("first").unwrap();
        from_userdata(&first).unwrap().hover_handle().unwrap().set(Value::Boolean(true));

        assert!(lua.load("return second:get()").eval::<bool>().unwrap(), "one name is one slot");
        assert!(!lua.load("return other:get()").eval::<bool>().unwrap(), "a different name is a different slot");
    }

    #[test]
    fn hover_rect_reads_a_real_non_zero_rect_before_anything_has_been_hovered() {
        // Found on a live session, not here: the rect half started nil, a signal resolving to nil
        // means the property is *absent* (ADR-0044 decision 1's amendment), and a tooltip's
        // `anchor_rect` is required and non-zero (§ 6.3). So every re-resolve refused the popup
        // until something hovered -- once per capability push, from the first frame.
        let (lua, _dirty) = lua_with_state();
        let rect: mlua::Table = lua.load(r#"return hover_rect("volume"):get()"#).eval().unwrap();

        assert!(rect.get::<f32>("width").unwrap() > 0.0, "a zero-width anchor_rect is refused by the protocol");
        assert!(rect.get::<f32>("height").unwrap() > 0.0, "and so is a zero-height one");
        assert_eq!(rect.get::<f32>("x").unwrap(), 0.0);
        assert_eq!(rect.get::<f32>("y").unwrap(), 0.0);
    }

    #[test]
    fn a_config_cannot_write_a_hover_signal_and_the_refusal_names_it_a_hover() {
        // Its own `SignalKind`, not the capability one, so this message does not tell a config it
        // is holding a capability (docs/adr/0062 decision 2).
        let (lua, dirty) = lua_with_state();
        let err = lua.load(r#"hover("volume"):set(true)"#).exec().unwrap_err().to_string();
        assert!(err.contains("state(name, initial)"), "the refusal points at the one writable kind: {err}");
        assert!(err.contains("a hover signal"), "the refusal has to name what it is holding: {err}");
        assert!(!dirty.take(), "a refused write marks nothing");
    }

    #[test]
    fn the_engine_writes_a_hover_signal_through_its_handle_and_only_a_hover_signal() {
        // The other half of decision 2: the writer takes a hover signal and nothing else, so a
        // config binding `hover = oblisk.network` cannot get the pointer to overwrite a
        // capability's snapshot.
        let dirty = DirtyFlag::new();
        let (hovered, hovered_rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let (capability, _capability_handle) = Signal::new_live(Value::Boolean(false), dirty.clone());

        assert!(hovered.hover_handle().is_some(), "a hover signal has a write end");
        assert!(hovered.hover_rect_handle().is_some(), "and a write end for where the node was");
        assert!(capability.hover_handle().is_none(), "a capability signal must not be writable as a hover");
        assert!(
            hovered_rect.hover_rect_handle().is_none(),
            "the rect half is not itself a trigger, so it hands out no second rect"
        );
    }

    #[test]
    fn writing_the_value_already_stored_marks_nothing() {
        // docs/adr/0062 decision 4. The pointer pushes this on every `wl_pointer` motion event and
        // one mark re-resolves every surface in the generation (ADR-0044 decision 2), so a pointer
        // sitting still inside one button must cost no re-resolves at all.
        let dirty = DirtyFlag::new();
        let (hovered, _rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let handle = hovered.hover_handle().unwrap();

        assert!(handle.set_changed(Value::Boolean(true)), "the first crossing is a real change");
        assert!(dirty.take());

        assert!(!handle.set_changed(Value::Boolean(true)), "the same value again is not a change");
        assert!(!dirty.take(), "an unchanged hover must not re-resolve the scene");

        assert!(handle.set_changed(Value::Boolean(false)), "leaving is a change again");
        assert!(dirty.take());
    }

    #[test]
    fn set_changed_cannot_dedupe_a_table_because_table_equality_is_identity() {
        // Not a wish, a warning. `crate::wayland::input`'s hover writer builds a fresh rect table
        // per event, and `PartialEq` on two `mlua` tables compares identity rather than contents,
        // so this can never answer "unchanged" for one. That is why the rect is written on the
        // entry edge only and not on every motion event (docs/adr/0062 decision 4) -- a caller
        // that leans on `set_changed` to dedupe a table marks the scene dirty every time.
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        let (_over, rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let handle = rect.hover_handle().unwrap();

        let build = || {
            let table = lua.create_table().unwrap();
            table.set("x", 1.0).unwrap();
            table.set("y", 2.0).unwrap();
            Value::Table(table)
        };
        assert!(handle.set_changed(build()));
        assert!(dirty.take());
        assert!(handle.set_changed(build()), "an identical table is still a different table");
        assert!(dirty.take(), "which is exactly the per-motion dirty mark the writer must avoid");
    }

    #[test]
    fn state_returns_a_signal_reading_back_the_initial_value_it_was_given() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua.load(r#"return state("count", 7):get()"#).eval().unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn set_replaces_what_a_later_get_returns() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                local s = state("count", 7)
                s:set(41)
                return s:get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 41, "a state signal must read back what Lua last wrote, not its initial value");
    }

    #[test]
    fn set_marks_the_scene_dirty_flag_and_a_plain_get_does_not() {
        // Reading must not mark dirty, or every layout-time resolve would re-dirty the scene it
        // was resolving.
        let (lua, dirty) = lua_with_state();
        lua.load(r#"s = state("count", 0)"#).exec().unwrap();
        assert!(!dirty.take(), "constructing a state signal changes nothing that is painted");

        let _: i64 = lua.load("return s:get()").eval().unwrap();
        assert!(!dirty.take(), "reading a state signal must not mark the scene dirty");

        lua.load("s:set(1)").exec().unwrap();
        assert!(dirty.take(), "writing a state signal must mark the shared scene-dirty flag");
    }

    #[test]
    fn the_same_state_name_returns_the_same_signal_and_ignores_the_second_initial() {
        // ADR-0044 decision 5's whole point: an in-place reload must hand back the signal holding
        // the user's last click, not reset it to what the config literal says.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "a re-declared name must keep its current value and ignore the new initial");
    }

    #[test]
    fn two_state_names_are_two_independent_signals() {
        let (lua, _dirty) = lua_with_state();
        let (a, b): (i64, i64) = lua
            .load(
                r#"
                state("a", 1):set(10)
                return state("a", 0):get(), state("b", 2):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!((a, b), (10, 2), "the map is keyed by name, so a write to one name must not reach another");
    }

    #[test]
    fn set_on_a_live_capability_signal_is_refused_because_capability_values_are_read_only_to_lua() {
        // The security-relevant one: if `:set()` accepted a `Live` signal, a config could
        // overwrite the network SSID the Supervisor just pushed, and every reader downstream
        // would believe it.
        let (lua, dirty) = lua_with_state();
        let (signal, _handle) = Signal::new_live(Value::Integer(1), dirty.clone());
        lua.globals().set("network", signal).unwrap();

        let err = lua.load(r#"network:set(2)"#).exec().unwrap_err();
        assert!(err.to_string().contains("read-only"), "the refusal must name the read-only rule: {err}");
        assert!(!dirty.take(), "a refused write must not mark the scene dirty either");

        let unchanged: i64 = lua.load("return network:get()").eval().unwrap();
        assert_eq!(unchanged, 1, "the pushed value must survive the attempt");
    }

    #[test]
    fn set_on_a_computed_signal_is_refused() {
        let (lua, _dirty) = lua_with_state();
        let err = lua
            .load(
                r#"
                local s = state("count", 1)
                s:map(function(v) return v end):set(9)
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "the refusal must name the read-only rule: {err}");
    }

    #[test]
    fn state_refuses_an_initial_value_that_fails_the_marshalling_boundary() {
        // `state`'s initial is Lua-authored, the boundary `marshal.rs` guards.
        let (lua, _dirty) = lua_with_state();
        let err = lua.load(r#"return state("bad", 0/0)"#).eval::<Value>().unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN initial must be refused by name: {err}");
    }

    #[test]
    fn set_refuses_a_value_that_fails_the_marshalling_boundary() {
        let (lua, dirty) = lua_with_state();
        let err = lua
            .load(
                r#"
                s = state("count", 0)
                s:set(0/0)
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN write must be refused by name: {err}");
        assert!(!dirty.take(), "a refused write must not mark the scene dirty");

        let unchanged: i64 = lua.load("return s:get()").eval().unwrap();
        assert_eq!(unchanged, 0, "a refused write must leave the stored value alone");
    }

    #[test]
    fn get_returns_the_wrapped_direct_value() {
        let lua = lua_with_signal("s", Value::Number(0.75));
        let result: f64 = lua.load("return s:get()").eval().unwrap();
        assert_eq!(result, 0.75);
    }

    #[test]
    fn map_recomputes_against_the_parents_current_value() {
        let lua = lua_with_signal("s", Value::Integer(10));
        let result: i64 = lua.load("return s:map(function(v) return v * 2 end):get()").eval().unwrap();
        assert_eq!(result, 20);
    }

    #[test]
    fn computed_combines_multiple_dependencies_current_values() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(3)).unwrap()).unwrap();
        lua.globals().set("b", Signal::try_new_direct(Value::Integer(4)).unwrap()).unwrap();

        let result: i64 = lua
            .load("return computed({a, b}, function(x, y) return x + y end):get()")
            .eval()
            .unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn computed_reflects_a_later_direct_signal_reconstruction_not_a_stale_cache() {
        // No memoization: rebuilding the dependency signal under the same name must be observed
        // by a later :get(), not a cached first read.
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        lua.load("doubled = computed({a}, function(x) return x * 2 end)").exec().unwrap();

        let first: i64 = lua.load("return doubled:get()").eval().unwrap();
        assert_eq!(first, 2);

        lua.globals().set("a", Signal::try_new_direct(Value::Integer(5)).unwrap()).unwrap();
        lua.load("doubled = computed({a}, function(x) return x * 2 end)").exec().unwrap();
        let second: i64 = lua.load("return doubled:get()").eval().unwrap();
        assert_eq!(second, 10);
    }

    #[test]
    fn computed_aborts_a_runaway_closure_instead_of_hanging_or_returning_a_wrong_value() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load("return computed({a}, function(x) while true do end end):get()")
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a busy-loop computed must error, not return a value");
        assert!(elapsed < Duration::from_secs(1), "the 5ms cap must abort well under a second, took {elapsed:?}");
    }

    #[test]
    fn a_nested_get_call_inside_a_computed_body_does_not_strip_the_outer_calls_cap() {
        // A computed body reading a *second* Signal re-enters the budget before the outer call
        // returns. The inner call must hand enforcement back to the outer one, not erase it.
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        lua.globals().set("other", Signal::try_new_direct(Value::Integer(2)).unwrap()).unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load("return computed({a}, function(x) local y = other:get(); while true do end end):get()")
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "the outer computed must still abort even though its body read a second Signal");
        assert!(elapsed < Duration::from_secs(1), "the cap must still fire near 5ms, took {elapsed:?}");
    }

    #[test]
    fn a_live_signal_reflects_a_value_pushed_after_construction_not_a_frozen_snapshot() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let (signal, handle) = Signal::new_live(Value::Integer(1), DirtyFlag::new());
        lua.globals().set("live", signal).unwrap();

        let first: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(first, 1, "must read the value passed to new_live before any push");

        handle.set(Value::Integer(42));
        let second: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(second, 42, "must reflect the pushed value without re-registering the global");
    }

    #[test]
    fn a_self_referential_computed_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        // `loop = computed({}, function() return loop:get() end)` recurses through
        // Signal::get_value with no bound the CPU cap can stop -- before this cap existed, this
        // exact test recursed the process into a `fatal runtime error: stack overflow`.
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                local loop
                loop = computed({}, function() return loop:get() end)
                return loop:get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a self-referential computed must error, not abort the process");
        assert!(elapsed < Duration::from_secs(1), "the depth cap must trip well under a second, took {elapsed:?}");
    }

    #[test]
    fn a_mutually_recursive_computed_pair_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                local a, b
                a = computed({}, function() return b:get() end)
                b = computed({}, function() return a:get() end)
                return a:get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a mutually recursive computed pair must error, not abort the process");
        assert!(elapsed < Duration::from_secs(1), "the depth cap must trip well under a second, took {elapsed:?}");
    }

    /// `s:map(f):map(f):...` `links` deep, on top of a `Direct` signal named `a`.
    fn map_chain_source(links: usize) -> String {
        format!(
            r#"
            local s = a
            for _ = 1, {links} do s = s:map(function(v) return v end) end
            return s:get()
            "#
        )
    }

    #[test]
    fn a_long_map_dependency_chain_is_rejected_by_the_nesting_cap_not_a_stack_overflow() {
        // A dependency *chain* nests `Signal::get_value` frames without nesting any Lua call, so
        // it is the shape the nesting cap has to bound. Before the deadline push moved to wrap
        // dependency resolution, a chain this long aborted the process with `fatal runtime error:
        // stack overflow`; the cap never saw depth above 1.
        let lua = lua_with_signal("a", Value::Integer(1));
        let err = lua.load(map_chain_source(200)).eval::<i64>().unwrap_err();
        assert!(
            err.to_string().contains("signal nesting exceeded"),
            "a 200-link map chain must trip the nesting cap: {err}"
        );
    }

    #[test]
    fn a_map_chain_at_the_nesting_cap_is_accepted_and_one_link_past_it_is_rejected() {
        // Both caps mean the same thing: at most N levels are admitted, the N+1th is rejected.
        //
        // The assertions are about which *gate* fired, not about the value. The budget counts
        // this thread's CPU time now (see `Deadline`), so a descheduled chain is no longer
        // charged for the wait, but a chain this long can still genuinely spend 5ms of CPU on a
        // busy machine. A CPU-cap error at the limit is the budget doing its job; what must never
        // happen is the *nesting* cap refusing a depth it claims to admit.
        let lua = lua_with_signal("a", Value::Integer(7));
        match lua.load(map_chain_source(MAX_SIGNAL_NESTING_DEPTH)).eval::<i64>() {
            Ok(value) => assert_eq!(value, 7, "exactly MAX_SIGNAL_NESTING_DEPTH nested levels must be admitted"),
            Err(err) => assert!(
                err.to_string().contains(CPU_CAP_EXCEEDED),
                "at the limit only the CPU budget may fire, never the nesting cap: {err}"
            ),
        }

        let err = lua.load(map_chain_source(MAX_SIGNAL_NESTING_DEPTH + 1)).eval::<i64>().unwrap_err();
        assert!(
            err.to_string().contains(&format!("maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels")),
            "one level past the cap must be rejected, naming the limit actually enforced: {err}"
        );
    }

    #[test]
    fn a_diamond_dependency_graph_is_cut_off_by_the_shared_cpu_budget() {
        // ADR-0044 decision 3's "no memoization" makes a diamond graph exponential in depth: 20
        // levels is 2^20 - 1 closure calls, which ran to completion in 1.9s returning Ok(1048576)
        // before the deadline covered dependency resolution. The one deadline the outermost
        // `get_value` pushes is what bounds it now: instrumented, the cap fires after roughly
        // 3,200 closure calls.
        //
        // The graph is built in a separate `exec` so `start` times only the evaluation: building
        // it is itself exponential and dominates the wall clock (`Signal` is `Clone` by value, so
        // `computed({s, s}, f)` deep-copies `s` twice, making a 2^20-node tree, not a shared DAG).
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.load("for _ = 1, 20 do a = computed({a, a}, function(x, y) return x + y end) end").exec().unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> = lua.load("return a:get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "an exponential diamond graph must be cut off, not returned: {result:?}");
        assert!(elapsed < Duration::from_millis(100), "the shared 5ms budget must cut it off early, took {elapsed:?}");
    }

    #[test]
    fn a_computed_descheduled_past_its_deadline_is_not_charged_for_time_it_did_not_run() {
        // The suite's own flakiness is what this is for. Measured before the fix: 5 failures in 53
        // full renderer-suite runs, a different test each time, every one of them
        // `computed/map exceeded its 5ms CPU budget` raised by a config a quiet machine evaluates
        // in microseconds. 630 tests across 12 threads deschedule one, and a wall-clock deadline
        // charges it for the wait.
        //
        // `park` burns no CPU, so a cap meaning what § 1.2 says -- "CPU runtime is capped at 5ms"
        // -- must not fire here. The loop after it is what makes this cover both gates rather than
        // one: it runs enough instructions for the hook to fire mid-call, and returning then puts
        // `CpuBudget::check_not_exceeded` past the wall deadline too.
        let lua = lua_with_signal("a", Value::Integer(7));
        let park = lua
            .create_function(|_, ()| {
                std::thread::sleep(Duration::from_millis(40));
                Ok(())
            })
            .unwrap();
        lua.globals().set("park", park).unwrap();

        let value: i64 = lua
            .load("return a:map(function(v) park() local n = 0 for i = 1, 5000 do n = n + i end return v end):get()")
            .eval()
            .unwrap();

        assert_eq!(value, 7);
    }

    #[test]
    fn a_pcall_swallowing_the_hook_error_still_fails_at_the_rust_boundary() {
        // The hook raises an ordinary Lua error, so a `pcall` in the body catches it and carries
        // on with a partially computed value. Bounded iteration, not `while true`, so a
        // regression here is a slow test rather than a hung runner.
        let lua = lua_with_signal("a", Value::Integer(1));
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                return computed({a}, function(x)
                    local n = 0
                    for _ = 1, 400 do
                        pcall(function() for _ = 1, 20000 do n = n + 1 end end)
                    end
                    return n
                end):get()
                "#,
            )
            .eval();

        assert!(
            result.is_err(),
            "a body that swallows the hook error must not yield a partially computed value: {result:?}"
        );
    }

    #[test]
    fn a_coroutine_body_is_covered_by_the_cpu_cap() {
        // `Lua::set_hook` installs per Lua thread, so work done inside `coroutine.create`/`resume`
        // used to run entirely unhooked: measured 5.75s returning `Ok`. The elapsed assertion is
        // what catches the coroutine escaping the hook, since the Rust-boundary gate would error
        // either way.
        let lua = lua_with_signal("a", Value::Integer(1));
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                return computed({a}, function(x)
                    local step = coroutine.wrap(function()
                        local n = 0
                        for _ = 1, 500000000 do n = n + 1 end
                        return n
                    end)
                    return step()
                end):get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "work inside a coroutine must still hit the CPU cap: {result:?}");
        assert!(elapsed < Duration::from_secs(1), "the hook must reach the coroutine, took {elapsed:?}");
    }

    #[test]
    fn a_cap_abort_does_not_leave_the_hook_installed_for_later_unrelated_evaluation() {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        lua.globals().set("a", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        let _: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) while true do end end):get()").eval();

        // An unrelated, slower-than-5ms-but-legitimate top-level script must not be clipped by a
        // hook left over from the aborted computed above.
        let result: i64 = lua
            .load(
                r#"
                local sum = 0
                for i = 1, 2000000 do sum = sum + i end
                return sum
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 2_000_001_000_000);
    }

    /// A `Capability` is a userdata the resolver has to see through, or every `oblisk.<name>`
    /// bound live into a property silently becomes a literal.
    #[test]
    fn from_userdata_sees_through_a_capability_to_its_read_signal() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(42), 1);
        lua.globals().set("probe", capability).unwrap();

        let ud: mlua::AnyUserData = lua.load("return probe").eval().unwrap();
        let signal = from_userdata(&ud).expect("a capability must resolve like the signal it wraps");
        assert_eq!(signal.get_value(&lua).unwrap().as_i64(), Some(42));
    }

    #[test]
    fn from_userdata_and_is_signal_agree() {
        // Stops one of the two growing a third type the other does not know about.
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, _handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        lua.globals().set("probe", capability).unwrap();
        lua.globals().set("plain", Signal::try_new_direct(Value::Integer(1)).unwrap()).unwrap();
        // A userdata that is neither, to prove both say no rather than both saying yes.
        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();

        for name in ["probe", "plain", "handle"] {
            let ud: mlua::AnyUserData = lua.load(format!("return {name}")).eval().unwrap();
            assert_eq!(from_userdata(&ud).is_some(), is_signal(&ud), "{name}");
        }
    }

    #[test]
    fn computed_accepts_a_capability_as_a_dependency_and_names_what_it_rejects() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(3), 1);
        lua.globals().set("probe", capability).unwrap();

        let doubled: i64 = lua.load("return computed({probe}, function(n) return n * 2 end):get()").eval().unwrap();
        assert_eq!(doubled, 6);

        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();
        let err = lua.load("return computed({handle}, function(n) return n end)").exec().unwrap_err().to_string();
        assert!(err.contains("must be Signals or `oblisk` capabilities"), "the error must say what was expected: {err}");
    }
}
