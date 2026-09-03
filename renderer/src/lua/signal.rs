//! The `Signal` reactive primitive (`oblisk-idl-api-specs.md` § 1.2): `signal:get()`,
//! `signal:map(fn)`, `signal:set(value)`, and the globals `computed(dependencies, fn)` and
//! `state(name, initial)` (ADR-0044 decision 5). Lua never constructs a bare `Signal` itself:
//! § 1.2 exposes it as Rust-owned userdata handed to Lua. It calls `fn` with each dependency's
//! current value, not the `Signal` handles, so a `computed` body doesn't call `:get()` on its
//! own deps.
//!
//! ponytail: `computed`/`map` recompute fresh on every `:get()`, no memoization, no
//! dependency-invalidation graph. Deciding when a cached value goes stale is the Watcher's job.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use crate::lua::marshal;

/// § 1.2: "CPU runtime is capped at 5ms per evaluation."
const CPU_CAP: Duration = Duration::from_millis(5);

/// What one whole `Scene::apply` gets, distinct from one signal evaluation: a bound on a whole
/// layout pass, not each getter call. Larger than [`CPU_CAP`]: a legitimate pass runs every
/// getter and blocks on the shaping thread once per text measurement. Measured on a 2000-sibling
/// row (up to 4000): release 202ms/298ms, debug 550ms/1.10s. 2 seconds is ~2x the worst of those
/// and ~300x ADR-0069's 6.14ms for a 500-row list, the shape a real config has; a tighter 250ms
/// cap refused that legitimate config.
///
/// Bounds damage, not performance: it stops a `margin` table whose spinning `__index` ran one
/// `Scene::apply` for 26.10 seconds returning `Ok(())` on the thread that also answers `configure`
/// and runs the VM (ADR-0039), now a 2 second stall and a reportable `LayoutError`. ponytail:
/// 200ms for 2000 siblings is its own problem this cap doesn't touch, dropping frames regardless
/// at ADR-0044's push cadence. Upgrade path: a real per-pass budget sized to cost.
const LAYOUT_PASS_CAP: Duration = Duration::from_secs(2);

/// One live evaluation's allowance, on both clocks. `cpu` is the one § 1.2 specifies and decides.
/// `wall` is a pre-filter only, since CPU time advances at most as fast as the clock: an
/// unexpired wall deadline proves an unexpired CPU one, so [`Deadline::expired`] answers "keep
/// going" without a syscall. `Instant::now()`'s 23.6ns beats `CLOCK_THREAD_CPUTIME_ID`'s 170.4ns,
/// so the common path pays the cheap clock. Wall alone would charge a descheduled evaluation for
/// time it didn't run: on a 12-thread machine it fired 5 times in 53 full suite runs on configs a
/// quiet machine evaluates in microseconds. Nothing is lost: a parked thread fires no instruction
/// hook, and ADR-0048 removes every blocking call anyway (`io` absent from `lua::config_stdlib`;
/// `lua::restrict_os`'s four `os` calls never wait).
#[derive(Clone, Copy)]
struct Deadline {
    wall: Instant,
    /// `None` when the clock could not be read, which leaves `wall` authoritative on its own.
    cpu: Option<Duration>,
}

impl Deadline {
    /// `cap` from now, on both clocks.
    fn lasting(cap: Duration) -> Self {
        Self { wall: Instant::now() + cap, cpu: thread_cpu_time().map(|used| used + cap) }
    }

    fn expired(&self) -> bool {
        if Instant::now() <= self.wall {
            return false;
        }
        // Past the wall pre-filter, so the CPU clock decides. An unreadable clock expires: a cap
        // that cannot measure must fire rather than quietly stop existing.
        self.cpu.is_none_or(|deadline| thread_cpu_time().is_none_or(|used| used > deadline))
    }
}

/// How much CPU the calling thread has burned, which is what § 1.2's cap is written against.
/// Per *thread*, not per process: one evaluation runs start to finish on the thread that entered
/// it, and the Lua VM is single-threaded by construction (ADR-0039 puts it on the Wayland
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

/// The recursion bound for [`Signal::get_value`]'s `Computed` arm: how many calls may be live on
/// the stack before the next returns an `mlua::Error`, the same "at most N levels" boundary
/// `layout::scene`'s `MAX_TREE_DEPTH` uses. Bounds both shapes of recursion, since
/// [`CpuBudget::enter`] runs before dependency resolution, not around the closure call alone:
/// body nesting (a `computed`/`map` body calling `:get()` on another signal, directly or through
/// a cycle) and dependency chains (`s:map(f):map(g):...`, which nest no Lua call).
/// A 200-link chain reaches `get_value` depth 200 with Rust stack depth 1 if the deadline pushes
/// only around the closure call, and a 5000-link chain aborts with `fatal runtime error: stack
/// overflow`; the 5ms CPU cap can't catch it either, since a chain does no Lua work for the hook
/// to fire on. 32 is generous headroom; see `layout::scene::MAX_TREE_DEPTH` for the measured
/// stack cost that sets both constants.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// The one message both cap gates raise, so a caller matching on it does not have to know which
/// gate fired (the hook mid-call, or [`CpuBudget::check_not_exceeded`] at the Rust boundary).
const CPU_CAP_EXCEEDED: &str = "computed/map exceeded its 5ms CPU budget";

/// The message the pass gate raises, kept distinct from [`CPU_CAP_EXCEEDED`] because the two name
/// different budgets and a config author needs to know which one it blew. Reached by a plain
/// `__index` metamethod with no signal anywhere, which is exactly the hole this budget closes.
const LAYOUT_PASS_CAP_EXCEEDED: &str = "the layout pass exceeded its 2s CPU budget";

#[derive(Clone)]
enum SignalKind {
    // ponytail: only `Signal::try_new_direct` constructs this variant, and it has no production
    // caller yet either. Exercised by tests only.
    #[allow(dead_code)]
    Direct(Value),
    Computed {
        deps: Vec<Signal>,
        func: Function,
    },
    /// A value Rust can overwrite after construction (`Signal::new_live`/`LiveSignalHandle`).
    /// `Rc<RefCell<_>>`, not `Arc<Mutex<_>>`: the `Loader` this lives on stays confined to one
    /// dedicated OS thread (the Wayland dispatch thread, ADR-0039).
    Live(Rc<RefCell<Value>>),
    /// The engine's own reactive state: a boolean `crate::wayland`'s pointer handler writes and a
    /// config only reads, built by the `hover(name)` global (ADR-0062). Structurally identical to
    /// [`SignalKind::Live`] but kept separate over who may write: `Signal::hover_handle` hands a
    /// write end only for this variant, so `hover = oblisk.network` gets no writer.
    /// `paired_rect` is the slot's other half: the boolean `hover(name)` returns carries a
    /// reference to the rect cell `hover_rect(name)` reads, so one handle writes both. `None` on
    /// the rect signal itself, a hover signal in every respect but not itself a trigger.
    Hover {
        cell: Rc<RefCell<Value>>,
        paired_rect: Option<Rc<RefCell<Value>>>,
        dirty: DirtyFlag,
    },
    /// How far a scrollable container has been scrolled along its main axis, in logical pixels
    /// (ADR-0069). Written by `crate::wayland`'s pointer handler on a wheel and by
    /// `layout::scene`'s positioning pass when it clamps; read by a config that wants to know.
    /// A fifth variant for [`SignalKind::Hover`]'s reason, not a reuse of it:
    /// `Signal::scroll_handle` hands a write end only for this kind, so `scroll = oblisk.network`
    /// gets no writer instead of a wheel overwriting a capability snapshot.
    Scroll {
        cell: Rc<RefCell<Value>>,
        dirty: DirtyFlag,
        /// A one-shot ask from `signal:reveal(index)` (ADR-0112): the 1-based child the next
        /// positioning pass of the viewport naming this signal must bring into view. Taken by that
        /// pass, so a reveal is honoured once and the wheel is free again afterwards. Beside the
        /// offset rather than in it, because the config cannot say it in pixels: where child
        /// `index` sits and how tall the viewport is are both facts only the pass knows.
        reveal: Rc<Cell<Option<usize>>>,
    },
    /// Lua-authored state (ADR-0044 decision 5): the one signal kind `Signal::set` accepts,
    /// built by the `state(name, initial)` global and written from a config's own `on_click`.
    /// A fourth variant rather than a reuse of `Live`, though the storage is identical: ADR-0044
    /// makes capability signals read-only to Lua, and a `set` accepting `Live` would let a config
    /// overwrite the network SSID the Supervisor just pushed and every reader believe it. A
    /// distinct writable kind makes that rule a type-system fact, testable, not a comment.
    /// Carries its own `DirtyFlag` clone since `set` is a `UserData` method with no
    /// `RendererClient` in reach; every signal in a generation shares the one flag decision 2
    /// specifies anyway.
    State {
        cell: Rc<RefCell<Value>>,
        dirty: DirtyFlag,
    },
}

impl SignalKind {
    /// What a refused [`Signal::set`] calls this kind when it explains itself to a config author.
    fn describe(&self) -> &'static str {
        match self {
            SignalKind::Direct(_) => "a direct",
            SignalKind::Computed { .. } => "a computed",
            SignalKind::Live(_) => "a capability",
            SignalKind::Hover { .. } => "a hover",
            SignalKind::Scroll { .. } => "a scroll",
            SignalKind::State { .. } => "a state",
        }
    }
}

/// The marshalling boundary (`marshal.rs`, § 1.1) applied to one Lua-authored value: checks the
/// types § 1.1 constrains (`Number`/`Integer`/`String`); every other shape passes through
/// untouched. Shared by [`Signal::try_new_direct`], [`Signal::new_state`] and `set`, which guard
/// a value hand-authored in Lua crossing into Rust. `Signal::new_live` doesn't: its value comes
/// from a Rust struct.
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

/// Whether a `state` literal is one this can compare across two evaluations. Tables, functions
/// and userdata are not, because `mlua` compares them by pointer and every evaluation builds
/// fresh ones, so one would always look edited.
fn is_comparable_literal(value: &Value) -> bool {
    matches!(value, Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) | Value::String(_))
}

/// Did the config author edit this `state` call's literal since the evaluation that seeded it?
/// `None` means unanswerable, which ADR-0044's amendment reads as "no". The `false` for a table
/// is load-bearing: `lib/ui_state.lua`'s `popup_anchor` default is a table, and a pointer
/// comparison would call every reload an edit, snapping an open popup back to the corner.
/// Numbers compare across `Integer`/`Number` as Lua's own `==` does, so `0` vs `0.0` is not an
/// edit; two scalars of different types are.
fn literal_was_edited(current: &Value, seeded: &Value) -> Option<bool> {
    if !is_comparable_literal(current) || !is_comparable_literal(seeded) {
        return None;
    }
    Some(match (current, seeded) {
        (Value::Integer(i), Value::Number(n)) | (Value::Number(n), Value::Integer(i)) => (*i as f64) != *n,
        _ => current != seeded,
    })
}

/// A read-only reactive value. Wraps either a plain value (`Direct`, Rust-pushed) or a Lua
/// closure recomputed against its dependencies' current values on every `get()` (`Computed`).
#[derive(Clone)]
pub struct Signal(SignalKind);

impl Signal {
    /// Wraps `value` as a `Direct` signal, enforcing the marshalling boundary (`marshal.rs`) on
    /// the types it constrains (`Number`/`Integer`/`String`); every other shape passes untouched.
    /// ponytail: no production caller yet, every live value goes through `Signal::new_live`
    /// instead; a real Lua-constructed `Direct` signal is a future phase's job. Tests only.
    #[allow(dead_code)]
    pub fn try_new_direct(value: Value) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&value)?;
        Ok(Signal(SignalKind::Direct(value)))
    }

    /// The signal behind `state(name, initial)` (ADR-0044 decision 5): writable from Lua through
    /// `signal:set(value)`, which marks `dirty` so the next poll turn re-resolves. Marshal-checked,
    /// unlike [`Self::new_live`]: `initial` is written in `shell.lua`, so it is a hand-authored
    /// value crossing into Rust, the boundary `marshal.rs` exists for.
    pub fn new_state(initial: Value, dirty: DirtyFlag) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&initial)?;
        Ok(Signal(SignalKind::State { cell: Rc::new(RefCell::new(initial)), dirty }))
    }

    /// Overwrites a `state` signal's value with a new `initial`, for ADR-0044's amendment: the
    /// config author changed the literal, so the file is the later write and beats whatever
    /// `signal:set()` last left here. Marks dirty through the same flag `set` does: the amendment
    /// is about a value the author expects to see on screen, and a silent re-seed would defeat
    /// that. Only [`SignalKind::State`] can be re-seeded, the only kind the state registry holds;
    /// any other kind here is a caller bug, not a config error.
    pub fn reseed(&self, value: Value) -> Result<(), marshal::MarshalError> {
        check_lua_authored(&value)?;
        let SignalKind::State { cell, dirty } = &self.0 else {
            debug_assert!(false, "reseed on {} signal, which the state registry cannot hold", self.0.describe());
            return Ok(());
        };
        *cell.borrow_mut() = value;
        dirty.mark();
        Ok(())
    }

    /// A signal Rust can push new values into after construction via the paired
    /// [`LiveSignalHandle`]. `try_new_direct`'s marshalling checks don't apply: the value arrives
    /// already `serde_json`-serialized from a Rust struct, which can't produce a NaN/Inf/oversized
    /// string the way hand-authored Lua can. `dirty` is decision 2's scene-dirty flag, marked by
    /// the paired [`LiveSignalHandle`] on every `set`; every live signal in one generation shares
    /// the same `DirtyFlag` (`renderer/src/socket.rs`'s `RendererClient` holds the clone that
    /// reads and clears it): one flag for the whole scene.
    pub fn new_live(initial: Value, dirty: DirtyFlag) -> (Self, LiveSignalHandle) {
        let cell = Rc::new(RefCell::new(initial));
        (Signal(SignalKind::Live(Rc::clone(&cell))), LiveSignalHandle(cell, dirty))
    }

    /// The signal `hover(name)` builds: a boolean the engine writes from `wl_pointer`, read-only
    /// to Lua (ADR-0062 decision 2). Its own kind rather than a second [`Self::new_live`] caller,
    /// so `signal:set()` refuses it by name (a hover slot, not a capability) and
    /// [`Self::hover_handle`] answers `None` for every other kind. Starts `false`, not nil: a
    /// config binds this straight to `visible`, and nil would mean the property absent (ADR-0044
    /// decision 1's amendment), not "no pointer yet."
    /// `initial_rect` isn't optional for the same reason: § 6.3 requires the `anchor_rect` a
    /// tooltip binds non-zero, and the popup resolves from the first frame, before any pointer has
    /// been near it, so a nil rect would refuse it until something hovers. The caller passes a
    /// real 1x1 rect since a `Value::Table` needs a `Lua` this constructor doesn't have.
    pub fn new_hover(dirty: DirtyFlag, initial_rect: Value) -> (Self, Self) {
        let over = Rc::new(RefCell::new(Value::Boolean(false)));
        let rect = Rc::new(RefCell::new(initial_rect));
        (
            Signal(SignalKind::Hover {
                cell: Rc::clone(&over),
                paired_rect: Some(Rc::clone(&rect)),
                dirty: dirty.clone(),
            }),
            Signal(SignalKind::Hover { cell: rect, paired_rect: None, dirty }),
        )
    }

    /// A scroll offset, starting at the top (ADR-0069 decision 2). A plain number rather than a
    /// pair like [`Self::new_hover`]'s: the content extent a scrollbar would also want is
    /// deliberately not published, since nothing draws one yet and the first config that does is
    /// the place to decide what shape it should arrive in.
    pub fn new_scroll(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Scroll {
            cell: Rc::new(RefCell::new(Value::Number(0.0))),
            dirty,
            reveal: Rc::new(Cell::new(None)),
        })
    }

    /// Asks the next positioning pass to scroll child `index` (1-based, counting visible children
    /// of the viewport) into view, and marks the scene dirty so that pass happens (ADR-0112).
    /// `false` for any other signal kind, which is the refusal `signal:reveal()` reports by name.
    pub(crate) fn request_reveal(&self, index: usize) -> bool {
        match &self.0 {
            SignalKind::Scroll { reveal, dirty, .. } => {
                reveal.set(Some(index));
                dirty.mark();
                true
            }
            _ => false,
        }
    }

    /// The pending reveal, consumed: `layout::scene`'s positioning pass is the only caller, and it
    /// takes the ask on the pass that honours it so the next wheel event is not fighting a reveal
    /// that already happened.
    pub(crate) fn take_reveal(&self) -> Option<usize> {
        match &self.0 {
            SignalKind::Scroll { reveal, .. } => reveal.take(),
            _ => None,
        }
    }

    /// The write end of a scroll signal, for the wheel handler and for the positioning pass that
    /// clamps what the wheel asked for. `None` for every other kind, which is what keeps a wheel
    /// off a capability signal.
    pub(crate) fn scroll_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Scroll { cell, dirty, .. } => Some(LiveSignalHandle(Rc::clone(cell), dirty.clone())),
            _ => None,
        }
    }

    /// This scroll signal's current offset, without a `Lua` to hand. [`Self::get_value`] needs a
    /// `&Lua` this can't supply: `layout::scene`'s positioning pass runs the scroll clamp deep
    /// inside a pass holding no VM reference, and threading one down just to read a number out of
    /// a `RefCell` would add a parameter to every frame of the layout recursion for one property.
    pub(crate) fn scroll_offset(&self) -> Option<f32> {
        match &self.0 {
            SignalKind::Scroll { cell, .. } => match *cell.borrow() {
                Value::Number(n) => Some(n as f32),
                Value::Integer(n) => Some(n as f32),
                _ => Some(0.0),
            },
            _ => None,
        }
    }

    /// The write end of a hover signal, for `crate::wayland`'s pointer handler. `None` for every
    /// other kind; see [`Self::new_hover`] for why that refusal is the point, not a missing case.
    pub(crate) fn hover_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Hover { cell, dirty, .. } => Some(LiveSignalHandle(Rc::clone(cell), dirty.clone())),
            _ => None,
        }
    }

    /// The write end of the rect half of this hover slot: where the node carrying it last was, in
    /// its surface's logical coordinates, which is what a tooltip `popup` binds `anchor_rect` to.
    /// `None` for every kind but the boolean half of a hover slot, including the rect half itself,
    /// which no node's `hover` property should be naming.
    pub(crate) fn hover_rect_handle(&self) -> Option<LiveSignalHandle> {
        match &self.0 {
            SignalKind::Hover { paired_rect: Some(rect), dirty, .. } => {
                Some(LiveSignalHandle(Rc::clone(rect), dirty.clone()))
            }
            _ => None,
        }
    }

    /// The signal `map(f)` builds: a `Computed` with this one as its only dependency, recomputed
    /// on every read like any other (ADR-0044 decision 3, no memoization). Lifted out of the
    /// Lua-facing `map` method so a Rust caller can build the same thing:
    /// `lua::capability::Capability` delegates its own `map` here, so `oblisk.lock` reads like
    /// the bare capability globals beside it instead of a second, drift-prone construction.
    pub(crate) fn mapped(&self, func: Function) -> Signal {
        Signal(SignalKind::Computed { deps: vec![self.clone()], func })
    }

    /// Reads this signal's current value (ADR-0044 decision 1). `pub(crate)`, not private:
    /// `layout::node`'s property parsers call this directly to resolve a `Signal` userdata in a
    /// slot instead of rejecting it. `&Lua` is threaded in rather than recovered from `self`: a
    /// `Computed` closure runs under a [`CpuBudget`], which needs one to install its hook, and
    /// mlua 0.12 exposes no way to recover a `Lua` from an `AnyUserData`/`Value`.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        match &self.0 {
            SignalKind::Direct(value) => Ok(value.clone()),
            SignalKind::Live(cell) => Ok(cell.borrow().clone()),
            SignalKind::Hover { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::Scroll { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::State { cell, .. } => Ok(cell.borrow().clone()),
            SignalKind::Computed { deps, func } => {
                // Entered before dependency resolution, not around `func.call` alone: keeps the
                // deadline-stack depth equal to the true `get_value` nesting depth, so
                // MAX_SIGNAL_NESTING_DEPTH bounds dependency chains as well as body nesting.
                let budget = CpuBudget::enter(lua)?;

                // ponytail: resolving every dependency on every read, with no memoization
                // (ADR-0044 decision 3), makes evaluation exponential in graph depth: a diamond
                // `computed({s, s}, f)` N deep evaluates `s` 2^N times (1,048,575 calls at N =
                // 20). Upgrade path: memoize a dependency's resolved value per `get_value`.
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
/// after construction, e.g. on every received `StateSnapshot`. The paired Lua-side `Signal`
/// always reads whatever was last set here, with no memoization, like every other kind here.
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

    /// Writes without marking the scene dirty, for a value the running pass derives from its own
    /// geometry. `layout::scene`'s positioning pass is the only caller: it clamps a scroll offset
    /// against the content extent it just measured, and marking dirty there would schedule
    /// another pass to observe a number this one already used, since clamping is idempotent. The
    /// cost is one frame of staleness, only when the clamp actually bit: a config reading
    /// `scroll("x")` in the same pass sees what the wheel asked for, then the clamped value from
    /// the next pass on. Invisible for the scroll itself, positioned from the clamped number here;
    /// it matters only to a derived readout like a scrollbar.
    pub(crate) fn set_quiet(&self, value: Value) {
        *self.0.borrow_mut() = value;
    }

    /// [`Self::set`], except storing the value already there does nothing: no write, no dirty
    /// mark, and it answers `false`. ADR-0062 decision 4: the pointer handler calls this on every
    /// `wl_pointer` motion event (device rate), and one mark re-resolves every surface in the
    /// generation (ADR-0044 decision 2). Comparing first turns that into one re-resolve per
    /// boundary crossed, not per event.
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
/// [`LiveSignalHandle`] in a generation and the `RendererClient` that reads and clears it, not
/// per-signal or per-surface. `Rc<Cell<bool>>`, not `Arc<AtomicBool>`: lives on the Wayland
/// dispatch thread alone (ADR-0039). ponytail: one flag for the whole scene re-resolves every
/// surface on any push. Upgrade path: a per-surface flag keyed on signals read, needing
/// read-tracking that doesn't exist yet.
#[derive(Clone)]
pub struct DirtyFlag(Rc<Cell<bool>>);

impl DirtyFlag {
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    /// `pub(crate)`: a `configure` carrying a new size for one surface instance means exactly
    /// what a capability push means, the resolved geometry no longer matches its inputs, so
    /// `crate::socket::RendererClient::set_instance_size` marks this same flag rather than adding
    /// a second mechanism beside it (ADR-0044 decision 2).
    pub(crate) fn mark(&self) {
        self.0.set(true);
    }

    /// Reads and clears the flag in one step: the "drain first, then re-resolve once" rule
    /// (ADR-0044 decision 2). `wayland::run`'s poll loop drains every pending inbound frame
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
        // ADR-0112: the one thing a config may say to a scroll signal. Not a pixel offset -- the
        // pass owns that (ADR-0069 decision 2) -- but which child it wants to see.
        methods.add_method("reveal", |_, this, index: i64| {
            let Some(index) = usize::try_from(index).ok().filter(|index| *index >= 1) else {
                return Err(mlua::Error::runtime(format!(
                    "signal:reveal() takes a 1-based child index, and {index} is not one"
                )));
            };
            if !this.request_reveal(index) {
                return Err(mlua::Error::runtime(format!(
                    "signal:reveal() is only valid on a scroll(name) signal, and this is {} signal",
                    this.0.describe()
                )));
            }
            Ok(())
        });
        // ADR-0044 decision 5's write path, and the only one Lua has. Every other kind is refused
        // by name rather than by a type error, so `network:set(...)` says *why* it refuses.
        methods.add_method("set", |_, this, value: Value| {
            let SignalKind::State { cell, dirty } = &this.0 else {
                return Err(mlua::Error::runtime(format!(
                    "signal:set() is only valid on a state(name, initial) signal, and this is {} signal: every other signal kind is read-only to Lua (ADR-0044 decision 5)",
                    this.0.describe()
                )));
            };
            // Checked before the write, so a refused value leaves the stored one alone and marks
            // nothing, the same Lua-authored boundary `Signal::new_state` puts on `initial`.
            check_lua_authored(&value).map_err(|err| {
                mlua::Error::runtime(format!("signal:set() refused its value at the marshalling boundary: {err}"))
            })?;
            *cell.borrow_mut() = value;
            dirty.mark();
            Ok(())
        });
    }
}

/// Applies an `oblisk set`/`oblisk toggle` to the `state(name, initial)` signal it names
/// (ADR-0112), through the same checks the config's own `signal:set()` passes: the value is
/// marshal-checked, and the scene is marked dirty. Refused, with the reason, when this config
/// declared no such state, or when a toggle finds something other than a boolean -- the two ways a
/// keybind can be out of step with the config it was written for.
pub fn write_state(lua: &Lua, set: &shared::SetState) -> Result<(), String> {
    let signal = lua
        .app_data_ref::<StateRegistry>()
        .and_then(|registry| registry.0.get(&set.name).map(|(signal, _)| signal.clone()))
        .ok_or_else(|| format!("this config declares no state({:?}, ...)", set.name))?;
    let value = match &set.write {
        shared::StateWrite::Set(json) => {
            crate::lua::json::to_lua(lua, json).map_err(|err| format!("the value does not convert to Lua: {err}"))?
        }
        shared::StateWrite::Toggle => match signal.get_value(lua) {
            Ok(Value::Boolean(current)) => Value::Boolean(!current),
            Ok(other) => return Err(format!("it holds {}, and only a boolean toggles", other.type_name())),
            Err(err) => return Err(format!("its value could not be read: {err}")),
        },
    };
    signal.reseed(value).map_err(|err| format!("refused at the marshalling boundary: {err}"))
}

/// Whether this VM's config ever called `hover(name)`. `crate::wayland`'s pointer handler asks
/// before doing any hover work at all, so a config with no tooltip and no expand-on-hover pays
/// nothing for the feature existing: no tree clone, no walk, no signal writes, on an event that
/// arrives at pointer-report rate.
pub fn any_hover_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<HoverRegistry>().is_some_and(|registry| !registry.0.is_empty())
}

/// Whether this config ever called `scroll(name)`, so the wheel handler can skip walking a tree
/// that has nothing to write. Same early-out [`any_hover_registered`] exists for.
pub fn any_scroll_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<ScrollRegistry>().is_some_and(|registry| !registry.0.is_empty())
}

/// The `name -> (Signal, literal)` map ADR-0044 decision 5 hangs `state` off: the name is the
/// identity, so an in-place reload finds the signal built last time, still holding the user's
/// last click, and an open dropdown stays open across a config edit. The second half is decision
/// 5's amendment: the `initial` this name was last seeded from, to ask whether the config author
/// edited the literal. An edit wins over the live value; without it a `state` default would be
/// the one value editing can't change, the wallpaper path that found this.
///
/// Lives in `Lua::set_app_data`, like [`CpuBudget::enter`]'s deadline stack: decision 4 keeps the
/// VM alive across a reload, and a generation swap is a new process with a new VM, giving
/// decision 5's "named state dies on a generation swap" for free.
#[derive(Default)]
struct StateRegistry(HashMap<String, (Signal, Value)>);

/// `state`'s registry, for `hover(name)` (ADR-0062 decision 2). Same rule and lifetime: the name
/// is the identity, so an in-place reload finds the signal built last time and a tooltip open
/// across a `config/theme.lua` edit stays open. Not shared with [`StateRegistry`]: one name space
/// would let `state("volume", 0)` and `hover("volume")` collide, surfacing as `signal:set()`
/// refusing a name the config thought it owned.
#[derive(Default)]
struct HoverRegistry(HashMap<String, (Signal, Signal)>);

/// The `name -> Signal` map behind `scroll(name)`, keyed the way [`HoverRegistry`] and the `state`
/// registry are: the name is the identity, so an in-place reload finds the offset the user left and
/// an open panel does not jump back to the top when the config is edited (ADR-0069 decision 2).
#[derive(Default)]
struct ScrollRegistry(HashMap<String, Signal>);

/// An RAII claim on the 5ms evaluation budget, held for one `Computed` [`Signal::get_value`]:
/// dependency resolution and the closure call, not the closure call alone. Entering pushes a
/// deadline onto a per-`Lua` stack in `app_data`; dropping pops it.
/// A stack, not mlua's single hook slot: this reenters three ways, a dependency can itself be
/// `Computed`, a body can read a second `Signal` (an upvalue or global, not just a declared
/// dependency), and a `computed` can be self- or mutually-referential. `Lua::set_hook` keeps one
/// unstacked callback, so installing/removing per call would let an inner removal strip the outer
/// call's still-active cap; the hook installs only on the 0->1 transition and removes on 1->0,
/// leaving a finished inner call's outer deadline on the stack.
/// The governing deadline is `stack[0]`, the outermost call's, not `stack.last()`: the innermost
/// is always the freshest, recomputed on every push, so it could never observe an expired budget.
/// `stack[0]` makes "5ms" mean § 1.2's whole-evaluation budget, not a per-level reset on `:get()`.
/// `stack[0]` is also the minimum: entries push/pop strictly LIFO with each [`CPU_CAP`] from its
/// own push, so the stack is non-decreasing, and `first()`'s O(1) beats `min()`'s walk of up to
/// [`MAX_SIGNAL_NESTING_DEPTH`] entries on every [`CHECK_EVERY_N_INSTRUCTIONS`] hook fire. Never
/// "fix" it to `last()`, the per-level reset this avoids.
struct CpuBudget<'lua> {
    lua: &'lua Lua,
}

/// How many live budgets want the instruction hook installed: a [`CpuBudget`] and a
/// [`LayoutPassBudget`] can each hold one, installing on the 0->1 transition and removing on
/// 1->0. A count, not [`CpuBudget`]'s own stack depth: keyed on depth alone, a signal evaluation
/// finishing inside a layout pass would take the stack to 0 and remove the hook the pass still
/// relies on, leaving every `__index` after the first signal read unbounded again.
#[derive(Default)]
struct HookHolders(usize);

/// Installs the instruction hook if this is the first holder. Balanced by [`release_hook`].
fn acquire_hook(lua: &Lua) -> mlua::Result<()> {
    if lua.app_data_ref::<HookHolders>().is_none() {
        lua.set_app_data(HookHolders::default());
    }
    let first = lua.app_data_ref::<HookHolders>().expect("just ensured the counter exists").0 == 0;
    if first {
        // `set_global_hook`, not `set_hook`: mlua's per-thread hook is keyed by Lua thread, so a
        // coroutine created by a `computed` body inherited the hook C function, found no callback
        // for itself, and disabled the hook on that thread, measured 5.75s of uninterrupted Lua
        // inside `coroutine.create`/`resume` returning `Ok`. `HookKind::Global` stores one
        // callback on the `Lua` itself, resolving on whichever thread fires, covering coroutines.
        lua.set_global_hook(
            mlua::HookTriggers { every_nth_instruction: Some(CHECK_EVERY_N_INSTRUCTIONS), ..mlua::HookTriggers::new() },
            |lua, _| match expired_budget(lua) {
                Some(message) => Err(mlua::Error::runtime(message)),
                None => Ok(mlua::VmState::Continue),
            },
        )?;
    }
    lua.app_data_mut::<HookHolders>().expect("just ensured the counter exists").0 += 1;
    Ok(())
}

/// Drops one hook claim, removing the hook when the last holder lets go.
fn release_hook(lua: &Lua) {
    let remaining = {
        let mut holders = lua.app_data_mut::<HookHolders>().expect("acquire_hook always runs before its release");
        holders.0 = holders.0.saturating_sub(1);
        holders.0
    };
    if remaining == 0 {
        // Both, in order: `remove_global_hook` stops an inheriting coroutine calling back, and
        // `remove_hook` clears the mask on *this* thread so later Lua isn't paying for a no-op.
        lua.remove_global_hook();
        lua.remove_hook();
    }
}

/// The deadline covering one whole `Scene::apply`, if a pass is in flight.
#[derive(Default)]
struct PassDeadline(Option<Deadline>);

/// An RAII claim on [`LAYOUT_PASS_CAP`], held for one entire layout pass, not one getter call.
/// Closes two holes: a resolved table's `__index` runs through `layout::node`'s metamethod-aware
/// `Table::get` after [`CpuBudget`] dropped its hook, covered by no budget at all (`while true do
/// end` behind a `margin` key hung the Wayland dispatch thread), and ADR-0021's cap is per
/// `get_value` call, so a tree of margined nodes bought one 5ms budget each, unbounded across
/// however many nodes the pass had. Runs beside [`CpuBudget`], not instead of it:
/// [`expired_budget`] fails on whichever expires first, keeping § 1.2's 5ms intact and adding a
/// ceiling.
pub(crate) struct LayoutPassBudget<'lua> {
    lua: &'lua Lua,
}

impl<'lua> LayoutPassBudget<'lua> {
    /// Starts the pass clock and keeps the instruction hook installed for the whole pass, which is
    /// what puts an `__index` metamethod under a budget for the first time.
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        if lua.app_data_ref::<PassDeadline>().is_none() {
            lua.set_app_data(PassDeadline::default());
        }
        // Before the deadline is stored, so a failed install leaves nothing for `Drop` to undo.
        acquire_hook(lua)?;
        lua.app_data_mut::<PassDeadline>().expect("just ensured the slot exists").0 =
            Some(Deadline::lasting(LAYOUT_PASS_CAP));
        Ok(Self { lua })
    }

    /// The gate at the Rust boundary, for the same reason [`CpuBudget::check_not_exceeded`] has
    /// one: the hook raises an ordinary Lua error, and a `pcall` inside a config's `__index` or
    /// getter can swallow it. Config Lua can catch the hook. It cannot catch this.
    pub(crate) fn exceeded(&self) -> bool {
        self.lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0).is_some_and(|d| d.expired())
    }
}

impl Drop for LayoutPassBudget<'_> {
    fn drop(&mut self) {
        self.lua.app_data_mut::<PassDeadline>().expect("enter always runs before its Drop").0 = None;
        release_hook(self.lua);
    }
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`]. The hook installs
    /// before the deadline is pushed, so no early return leaves the stack unbalanced: pushing
    /// first would strand an entry the depth-1 branch never revisits, silently disabling the cap
    /// for the VM's life. No Lua runs between install and push, and the hook tolerates an empty
    /// stack.
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
        acquire_hook(lua)?;
        lua.app_data_mut::<Vec<Deadline>>()
            .expect("just ensured the deadline stack exists")
            .push(Deadline::lasting(CPU_CAP));
        Ok(Self { lua })
    }

    /// The second gate on the 5ms budget, at the Rust boundary rather than the VM: the hook raises
    /// an ordinary Lua error inside the running function, so a `pcall` in a `computed` body
    /// catches it and carries on, measured running 7.5x the cap and returning a partially computed
    /// `Ok`. Checking the deadline again once the call returns makes a caught hook error an `Err`
    /// anyway: config Lua can catch the hook, but not this.
    /// ponytail: runs only when the call returns; a body that swallows the hook error and never
    /// returns still spins until a fire lands outside the `pcall`. Needs preemption this VM can't
    /// offer; upgrade path is the generation-swap process boundary (ADR-0039).
    fn check_not_exceeded(&self) -> mlua::Result<()> {
        match expired_budget(self.lua) {
            Some(message) => Err(mlua::Error::runtime(message)),
            None => Ok(()),
        }
    }
}

impl Drop for CpuBudget<'_> {
    fn drop(&mut self) {
        self.lua.app_data_mut::<Vec<Deadline>>().expect("CpuBudget::enter always runs before its Drop").pop();
        release_hook(self.lua);
    }
}

/// Which budget, if either, has run out: the message to raise, or `None` to keep going. Two
/// independent deadlines, the earlier one wins. The signal deadline is the outermost live
/// [`CpuBudget`]'s, `first()` not `min()`/`last()` (see that type's doc for why); the pass
/// deadline is [`LayoutPassBudget`]'s, kept out of that stack so `first()` stays O(1) and correct.
/// Neither present is never expired, letting [`acquire_hook`] install before the first deadline.
fn expired_budget(lua: &Lua) -> Option<&'static str> {
    let signal = lua.app_data_ref::<Vec<Deadline>>().and_then(|stack| stack.first().copied());
    if signal.is_some_and(|deadline| deadline.expired()) {
        return Some(CPU_CAP_EXCEEDED);
    }
    let pass = lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0);
    pass.filter(Deadline::expired).map(|_| LAYOUT_PASS_CAP_EXCEEDED)
}

/// The one answer to "does this Lua userdata resolve like a signal?", and the `Signal` to
/// resolve it through. Two userdata types answer yes: [`Signal`] and `capability::Capability`.
/// Every § 2 capability sits behind a `Capability`, so `computed({oblisk.audio}, f)` and
/// `content = oblisk.mpris` hand the engine a `Capability`, not a bare `Signal`. Without one
/// shared answer, every live capability binding would resolve as a skipped, literal property,
/// freezing at the first frame.
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
/// `dirty` is decision 2's one scene-dirty flag, taken explicitly rather than fished out of
/// `app_data`: a hidden coupling failing inside a config author's own `state()` call is worse
/// than threading one argument through, and it must be the same flag `Signal::new_live` hands
/// out and `RendererClient` drains, or `:set()` would mark a flag nothing reads.
pub fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    let hover_dirty = dirty.clone();
    let rect_dirty = dirty.clone();
    let scroll_dirty = dirty.clone();
    lua.globals().set(
        "computed",
        lua.create_function(|_, (deps, func): (Table, Function)| {
            let mut collected = Vec::new();
            for dep in deps.sequence_values::<mlua::AnyUserData>() {
                let dep = dep?;
                // Named rather than left to `borrow`'s own type error, which never says what was
                // expected.
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
            let existing = lua
                .app_data_ref::<StateRegistry>()
                .expect("just ensured the state registry exists")
                .0
                .get(&name)
                .cloned();
            if let Some((signal, seeded)) = existing {
                // Decision 5: a name already in the map wins, so an in-place reload keeps the
                // value. Its amendment narrows that to a literal the author left alone: an
                // `initial` differing from the seed is an edit, and an edit outweighs a `:set()`.
                if literal_was_edited(&initial, &seeded) == Some(true) {
                    signal.reseed(initial.clone()).map_err(|err| {
                        mlua::Error::runtime(format!(
                            "state(\"{name}\", ...) refused its new initial value at the marshalling boundary: {err}"
                        ))
                    })?;
                    lua.app_data_mut::<StateRegistry>()
                        .expect("just ensured the state registry exists")
                        .0
                        .insert(name, (signal.clone(), initial));
                }
                return Ok(signal);
            }
            let signal = Signal::new_state(initial.clone(), dirty.clone()).map_err(|err| {
                mlua::Error::runtime(format!(
                    "state(\"{name}\", ...) refused its initial value at the marshalling boundary: {err}"
                ))
            })?;
            lua.app_data_mut::<StateRegistry>()
                .expect("just ensured the state registry exists")
                .0
                .insert(name, (signal.clone(), initial));
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
    lua.globals()
        .set("hover_rect", lua.create_function(move |lua, name: String| Ok(hover_slot(lua, &rect_dirty, name)?.1))?)?;
    lua.globals().set(
        "scroll",
        lua.create_function(move |lua, name: String| {
            if lua.app_data_ref::<ScrollRegistry>().is_none() {
                lua.set_app_data(ScrollRegistry::default());
            }
            let existing = lua
                .app_data_ref::<ScrollRegistry>()
                .expect("just ensured the scroll registry exists")
                .0
                .get(&name)
                .cloned();
            if let Some(signal) = existing {
                return Ok(signal);
            }
            let signal = Signal::new_scroll(scroll_dirty.clone());
            lua.app_data_mut::<ScrollRegistry>()
                .expect("just ensured the scroll registry exists")
                .0
                .insert(name, signal.clone());
            Ok(signal)
        })?,
    )
}

/// One hover slot by name, built on first ask: the boolean `hover(name)` returns and the rect
/// `hover_rect(name)` returns, in that order (ADR-0062 decision 2).
/// The name is the identity, as `state(name, initial)` (ADR-0044 decision 5): it carries a hover
/// across an in-place reload and lets two files reach one slot; both globals build the pair, so
/// naming either first is not a different slot. No marshalling check on either initial: these
/// only hold the booleans and rect table the pointer handler writes, none hand-authored in Lua.
fn hover_slot(lua: &Lua, dirty: &DirtyFlag, name: String) -> mlua::Result<(Signal, Signal)> {
    if lua.app_data_ref::<HoverRegistry>().is_none() {
        lua.set_app_data(HoverRegistry::default());
    }
    let existing =
        lua.app_data_ref::<HoverRegistry>().expect("just ensured the hover registry exists").0.get(&name).cloned();
    if let Some(slot) = existing {
        return Ok(slot);
    }
    let slot = Signal::new_hover(dirty.clone(), Value::Table(unhovered_rect(lua)?));
    lua.app_data_mut::<HoverRegistry>().expect("just ensured the hover registry exists").0.insert(name, slot.clone());
    Ok(slot)
}

/// What `hover_rect(name)` reads before the pointer has ever been on its node: a 1x1 rect at the
/// origin. Non-zero on both axes because § 6.3 refuses a zero `anchor_rect`; a real table, not
/// nil, since a nil property is absent. A tooltip bound to this sits invisibly at the origin
/// (`visible = hover(name)` is false) until the same event replaces this with the real rect.
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
        // ADR-0062 decision 2: the engine writes this one, so a config that reads it before
        // the pointer has ever been over the node must get `false`, not nil -- `visible` binds to
        // it directly and a nil there would mean "absent" (ADR-0044 decision 1's amendment).
        let (lua, _dirty) = lua_with_state();
        let started: bool = lua.load(r#"return hover("volume"):get()"#).eval().unwrap();
        assert!(!started);
    }

    #[test]
    fn hover_hands_the_same_name_the_same_signal_so_an_in_place_reload_keeps_it_open() {
        // The `state(name, initial)` rule of ADR-0044 decision 5, applied to hover by
        // ADR-0062 decision 2: the name is the identity, so re-running the config finds the
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
        // is holding a capability (ADR-0062 decision 2).
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
        // ADR-0062 decision 4. The pointer pushes this on every `wl_pointer` motion event and
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
        // entry edge only and not on every motion event (ADR-0062 decision 4) -- a caller
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
    fn the_same_state_name_and_the_same_initial_keeps_the_value_written_since() {
        // ADR-0044 decision 5's whole point: an in-place reload must hand back the signal holding
        // the user's last click, not reset it to what the config literal says.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "an unedited literal must keep the current value, not reset to the initial");
    }

    #[test]
    fn a_changed_initial_re_seeds_the_signal_and_marks_dirty() {
        // Decision 5's amendment. The literal changed, so the config author edited the file, and
        // the edit is a later write than the `:set()` it lands on. This is the wallpaper path.
        let (lua, dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 99, "an edited literal must win over the value `:set()` left behind");
        assert!(dirty.take(), "a re-seed must mark the scene dirty, or nothing repaints from it");
    }

    #[test]
    fn re_seeding_twice_from_the_same_edited_literal_only_happens_once() {
        // The registry has to remember the *new* literal, not the one it was built with, or every
        // later evaluation would re-seed against a stale comparison and clobber `:set()` forever.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0)
                state("open", 99)
                state("open", 99):set(7)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 7, "the second evaluation of an already-adopted literal is not another edit");
    }

    #[test]
    fn a_table_initial_never_counts_as_edited() {
        // `lib/ui_state.lua`'s `popup_anchor`. mlua compares tables by pointer and every
        // evaluation builds a fresh one, so comparing them would call every reload an edit and
        // snap an open popup back to the corner.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("anchor", { x = 0 }):set(5)
                return state("anchor", { x = 0 }):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "a table literal must keep the live value, since it cannot be compared");
    }

    #[test]
    fn rewriting_an_integer_literal_as_a_float_is_not_an_edit() {
        // Lua's own `==` says `0 == 0.0`, and a config author who reformats a number did not
        // change the value they wrote.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("count", 0):set(5)
                return state("count", 0.0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "0 and 0.0 are the same literal");
    }

    #[test]
    fn changing_a_literals_type_is_an_edit() {
        let (lua, _dirty) = lua_with_state();
        let result: String = lua
            .load(
                r#"
                state("kind", false):set("clicked")
                return state("kind", "waiting"):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "waiting", "two scalars of different types are a different literal");
    }

    #[test]
    fn two_state_names_are_two_independent_signals() {
        let (lua, _dirty) = lua_with_state();
        let (a, b): (i64, i64) = lua
            .load(
                r#"
                state("a", 1):set(10)
                return state("a", 1):get(), state("b", 2):get()
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

    /// ADR-0112: a keybind's write lands on the config's own signal, and is refused by name when
    /// the config declares no such state or the toggle finds no boolean.
    #[test]
    fn a_control_clients_write_reaches_a_declared_state_and_is_refused_otherwise() {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        lua.load(r#"OPEN = state("launcher_open", false); KIND = state("panel_kind", "none")"#).exec().unwrap();
        dirty.take();

        write_state(&lua, &shared::SetState { name: "launcher_open".into(), write: shared::StateWrite::Toggle })
            .unwrap();
        assert!(dirty.take(), "a write from outside re-resolves the scene like any other");
        assert!(lua.load("return OPEN:get()").eval::<bool>().unwrap());

        let set = shared::StateWrite::Set(serde_json::json!("notifications"));
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: set }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "notifications");
        dirty.take();

        let missing = write_state(&lua, &shared::SetState { name: "nope".into(), write: shared::StateWrite::Toggle });
        assert!(missing.unwrap_err().contains("declares no state"));
        let not_bool =
            write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: shared::StateWrite::Toggle });
        assert!(not_bool.unwrap_err().contains("only a boolean toggles"));
        assert!(!dirty.take(), "a refused write changes nothing");
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

        let result: i64 = lua.load("return computed({a, b}, function(x, y) return x + y end):get()").eval().unwrap();
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
        let result: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) while true do end end):get()").eval();
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
        let result: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) local y = other:get(); while true do end end):get()").eval();
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
        let _: mlua::Result<i64> = lua.load("return computed({a}, function(x) while true do end end):get()").eval();

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
        assert!(
            err.contains("must be Signals or `oblisk` capabilities"),
            "the error must say what was expected: {err}"
        );
    }
}
