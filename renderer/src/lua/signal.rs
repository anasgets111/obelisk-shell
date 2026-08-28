//! The `Signal` reactive primitive (`oblisk-idl-api-specs.md` § 1.2): `signal:get()`,
//! `signal:map(fn)`, and the global `computed(dependencies, fn)`.
//!
//! Lua never constructs a bare `Signal` itself -- § 1.2 exposes it as Rust-owned userdata handed
//! *to* Lua, never built *by* Lua from a raw value. `Signal::try_new_direct` is the Rust-side
//! entry point Phase 11 will call once there's a real `StateSnapshot` value to wrap; this phase
//! only exercises it from tests.
//!
//! ponytail: `computed`/`map` recompute their function fresh on every `:get()` call -- no
//! memoization, no dependency-invalidation graph. Deciding *when* a cached computed value goes
//! stale is the Watcher's job (`CONTEXT.md`, Watcher; build-steps.md Phase 13), not this loader's.
//! Recomputing on every read is the correct, simple baseline until something needs the graph.
//!
//! `computed(dependencies, fn)` calls `fn` with each dependency's *current value* as a positional
//! argument (`fn(dep1_value, dep2_value, ...)`), not the `Signal` handles themselves -- the spec
//! doesn't literally pin this calling convention down, and passing already-unwrapped values means
//! every `computed` body doesn't have to redundantly call `:get()` on each of its own dependencies.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use crate::lua::marshal;

/// § 1.2: "CPU runtime is capped at 5ms per evaluation."
const CPU_CAP: Duration = Duration::from_millis(5);

/// How many VM instructions run between budget checks. `mlua`'s `HookTriggers` docs warn a low
/// value "can incur a very high overhead"; 1000 keeps the check cheap while still catching a
/// runaway closure within roughly one instruction-batch of the 5ms mark, not after it's spun for
/// seconds.
const CHECK_EVERY_N_INSTRUCTIONS: u32 = 1000;

/// The recursion bound for [`Signal::get_value`]'s `Computed` arm (build-steps.md Phase 19 item
/// 3, defect 3): how many of those calls may be live on the stack at once before the next one
/// returns an `mlua::Error` instead of recursing further. At most this many levels are admitted;
/// the level past it is refused, the same "at most N levels" boundary `layout::scene`'s
/// `MAX_TREE_DEPTH` uses.
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
/// A legitimate dependency chain is shallow -- a handful of `map` calls, not dozens -- so 32 is
/// generous headroom above anything reasonable, and is the more expensive of the two caps per
/// level. See `layout::scene::MAX_TREE_DEPTH` for the measured compounded stack cost of this
/// constant and that one together, which is what actually sets both.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// The one message both cap gates raise, so a caller matching on it does not have to know which
/// gate fired (the hook mid-call, or [`CpuBudget::check_not_exceeded`] at the Rust boundary).
const CPU_CAP_EXCEEDED: &str = "computed/map exceeded its 5ms CPU budget";

#[derive(Clone)]
enum SignalKind {
    // ponytail: only `Signal::try_new_direct` constructs this variant, and that constructor has
    // no production caller yet either (see its own doc comment) -- exercised by tests only.
    #[allow(dead_code)]
    Direct(Value),
    Computed { deps: Vec<Signal>, func: Function },
    /// A value Rust can overwrite after construction (`Signal::new_live`/`LiveSignalHandle`,
    /// Phase 11). `Rc<RefCell<_>>`, not `Arc<Mutex<_>>`: the `Loader` this lives on stays
    /// confined to one dedicated OS thread (the Wayland dispatch thread, docs/adr/0039),
    /// the same single-threaded-state convention `supervisor/src/audio/mixer.rs`'s
    /// `Rc<RefCell<MixerState>>` already uses.
    Live(Rc<RefCell<Value>>),
}

/// A read-only reactive value. Wraps either a plain value (`Direct`, Rust-pushed) or a Lua
/// closure recomputed against its dependencies' current values on every `get()` (`Computed`).
#[derive(Clone)]
pub struct Signal(SignalKind);

impl Signal {
    /// Wraps `value` as a `Direct` signal, enforcing the marshalling boundary (`marshal.rs`) on
    /// the types it constrains (`Number`/`Integer`/`String`); every other Lua value shape passes
    /// through untouched, since § 1.1 places no extra constraint on it.
    ///
    /// ponytail: no production caller yet -- every live value this phase pushes goes through
    /// `Signal::new_live` instead (Rust-pushed, not Lua-authored, so the marshalling boundary
    /// this guards doesn't apply the same way; see `new_live`'s own doc comment). A real
    /// Lua-constructed `Direct` signal is still a future phase's job. Exercised by this module's
    /// tests only.
    #[allow(dead_code)]
    pub fn try_new_direct(value: Value) -> Result<Self, marshal::MarshalError> {
        match &value {
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
        Ok(Signal(SignalKind::Direct(value)))
    }

    /// A signal Rust can push new values into after construction via the paired
    /// [`LiveSignalHandle`] -- `try_new_direct`'s marshalling checks don't apply here: the value
    /// arrives already `serde_json`-serialized from a Rust struct (e.g. `AppStream`), which can't
    /// produce a NaN/Inf/oversized string the way hand-authored Lua can. `try_new_direct` guards
    /// Lua-authored values crossing into Rust; this is a value Rust itself produced.
    ///
    /// `dirty` is the scene-dirty flag (ADR-0044 decision 2) the paired [`LiveSignalHandle`]
    /// marks on every `set`. Every live signal in one generation shares the same `DirtyFlag`
    /// (`renderer/src/socket.rs`'s `RendererClient` holds the clone that reads and clears it),
    /// because decision 2's "one flag for the whole scene" means there is exactly one to share,
    /// not one per capability.
    pub fn new_live(initial: Value, dirty: DirtyFlag) -> (Self, LiveSignalHandle) {
        let cell = Rc::new(RefCell::new(initial));
        (Signal(SignalKind::Live(Rc::clone(&cell))), LiveSignalHandle(cell, dirty))
    }

    /// Reads this signal's current value (ADR-0044 decision 1, `CONTEXT.md`'s Signal resolution
    /// entry). `pub(crate)`, not private: `layout::node`'s property parsers call this directly to
    /// resolve a `Signal` userdata found in a property slot, instead of rejecting it -- the same
    /// method `Signal::get`'s Lua-facing method already calls, just reachable from Rust now too.
    /// `&Lua` is threaded in rather than recovered from `self`, because a `Computed` signal's
    /// closure runs under a [`CpuBudget`], which needs a real `Lua` to install its
    /// instruction-count hook on -- mlua 0.12 exposes no way to recover a `Lua` from an
    /// `AnyUserData`/`Value` (checked the vendored source under `~/.cargo/registry`; no such
    /// accessor exists), so there is nothing to recover it from.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        match &self.0 {
            SignalKind::Direct(value) => Ok(value.clone()),
            SignalKind::Live(cell) => Ok(cell.borrow().clone()),
            SignalKind::Computed { deps, func } => {
                // The budget is entered *before* dependency resolution, not around `func.call`
                // alone, and that ordering is the whole point (see [`CpuBudget`]): it makes the
                // deadline-stack depth equal the true `get_value` nesting depth, so
                // MAX_SIGNAL_NESTING_DEPTH bounds dependency chains as well as body nesting, and
                // it keeps the outermost deadline live across the entire dependency graph rather
                // than restarting the 5ms clock at every leaf.
                let budget = CpuBudget::enter(lua)?;

                // ponytail: resolving every dependency on every read, with no memoization
                // (ADR-0044 decision 3, and this module's header), makes evaluation exponential
                // in graph depth for a diamond-shaped graph -- `computed({s, s}, f)` stacked N
                // deep evaluates `s` 2^N times, measured at 1,048,575 closure calls for N = 20.
                // N = 30 is about a billion calls. The 5ms budget entered just above is the only
                // thing that makes that survivable instead of fatal: it is shared by the whole
                // graph rather than restarting per leaf, so the evaluation is cut off with an
                // error. Memoizing a dependency's resolved value for the duration of one
                // outermost `get_value` -- a per-evaluation cache keyed on signal identity, which
                // would collapse the DAG back to linear -- is the upgrade path if a real config
                // ever grows a diamond deep enough to notice. Not built now: ADR-0044 decision 3
                // says no memoization, and no config has needed it.
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
/// after construction, e.g. on every received `StateSnapshot` (Phase 11). The paired `Signal`
/// (Lua-side) always reads whatever was last set here -- no memoization, matching every other
/// `Signal` kind in this file.
#[derive(Clone)]
pub struct LiveSignalHandle(Rc<RefCell<Value>>, DirtyFlag);

impl LiveSignalHandle {
    /// Writes `value` and marks the shared scene dirty (ADR-0044 decision 2, `CONTEXT.md`'s
    /// Dirty scene entry): every push, not just this capability's own next read, has to make the
    /// next poll turn re-resolve the whole scene, since decision 3 rejects a per-signal
    /// dependency graph that could narrow that down.
    pub fn set(&self, value: Value) {
        *self.0.borrow_mut() = value;
        self.1.mark();
    }
}

/// The one scene-dirty flag ADR-0044 decision 2 specifies: a single `bool`, shared by every
/// [`LiveSignalHandle`] in a generation and by the `RendererClient` that reads and clears it, not
/// a per-signal or per-surface set. `Rc<Cell<bool>>`, not `Arc<AtomicBool>`: this lives entirely
/// on the Wayland dispatch thread (docs/adr/0039), the same single-threaded-state reasoning
/// `SignalKind::Live`'s `Rc<RefCell<Value>>` documents above.
///
/// `ponytail:` one flag for the whole scene means any push re-resolves every surface, including
/// one that reads nothing from the capability that changed. The upgrade path is a per-surface
/// flag keyed on which signals a surface actually reads, which needs read-tracking that doesn't
/// exist yet (ADR-0044 decision 2's own ceiling).
#[derive(Clone)]
pub struct DirtyFlag(Rc<Cell<bool>>);

impl DirtyFlag {
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    /// `pub(crate)` since build-steps.md Phase 20 item 4: a `configure` carrying a new size for one
    /// surface instance means exactly what a capability push means -- the resolved geometry no
    /// longer matches its inputs -- so `crate::socket::RendererClient::set_instance_size` marks
    /// this same flag rather than adding a second mechanism beside it (ADR-0044 decision 2).
    pub(crate) fn mark(&self) {
        self.0.set(true);
    }

    /// Reads and clears the flag in one step -- the "drain first, then re-resolve once" rule
    /// (ADR-0044 decision 2): `wayland::run`'s poll loop drains every pending inbound frame
    /// before calling this once, so a burst of `StateSnapshot` pushes marks the flag repeatedly
    /// but this only ever reports it once, coalescing the burst into a single re-resolve instead
    /// of one per push.
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
        methods.add_method("map", |_, this, f: Function| {
            Ok(Signal(SignalKind::Computed { deps: vec![this.clone()], func: f }))
        });
    }
}

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
/// (`stack.last()`, this code's original design; build-steps.md Phase 19 item 3 defect 3,
/// `docs/adr/0021`'s amendment). Reading the innermost meant a monotonically deepening recursion
/// always saw the *freshest* deadline, recomputed from `Instant::now()` at every new level's own
/// push, so the check could never observe an expired budget no matter how long the whole chain
/// ran. `stack[0]` makes "5ms" mean what § 1.2 says: a budget for the evaluation as a whole, not
/// a per-level allowance that resets on every recursive `:get()`.
///
/// `stack[0]` is also, specifically, the *minimum*, so this is `first()` and not `iter().min()`:
/// every deadline is `Instant::now() + CPU_CAP` on `Instant`'s monotonic clock, `CPU_CAP` is a
/// constant, and entries are pushed and popped strictly LIFO by this guard's own construction and
/// `Drop`. A later push therefore always carries a later-or-equal deadline than the one beneath
/// it, so the vector is non-decreasing and its first element is its minimum. That matters because
/// the hook runs every [`CHECK_EVERY_N_INSTRUCTIONS`] instructions: `first()` is O(1) where
/// `min()` walked up to [`MAX_SIGNAL_NESTING_DEPTH`] `Instant`s per fire. Do not "fix" it to
/// `last()` either -- that is the per-level reset this design exists to avoid.
struct CpuBudget<'lua> {
    lua: &'lua Lua,
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`].
    ///
    /// The hook is installed *before* the deadline is pushed, so no early return can leave the
    /// stack unbalanced (build-steps.md Phase 19 item 3): an install failure returns with nothing
    /// pushed and nothing to pop, where pushing first would have stranded an entry that the
    /// depth-1 branch then never revisits, silently disabling the cap for the rest of the VM's
    /// life. No Lua runs between the install and the push, and the hook tolerates an empty stack.
    fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        if lua.app_data_ref::<Vec<Instant>>().is_none() {
            lua.set_app_data(Vec::<Instant>::new());
        }
        let depth = lua.app_data_ref::<Vec<Instant>>().expect("just ensured the deadline stack exists").len();
        if depth >= MAX_SIGNAL_NESTING_DEPTH {
            return Err(mlua::Error::runtime(format!(
                "signal nesting exceeded its maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels -- a computed/map chain recursing into itself, or a dependency chain that long?"
            )));
        }
        if depth == 0 {
            // `set_global_hook`, not `set_hook`: mlua's per-thread hook (`HookKind::Thread`) is
            // keyed by Lua thread in a registry table, so a coroutine created by a `computed`
            // body inherited the hook C function from `lua_newthread` (lstate.c copies `hook`,
            // `hookmask` and `basehookcount` from the creating thread) but found no callback for
            // itself and *disabled* the hook on that thread. Measured: 5.75s of uninterrupted Lua
            // inside `coroutine.create`/`resume`, returning `Ok`. `HookKind::Global` stores one
            // callback on the `Lua` itself and `global_hook_proc` reads it whichever thread it
            // fires on, so the inherited hook resolves and the cap covers coroutines too.
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
        lua.app_data_mut::<Vec<Instant>>()
            .expect("just ensured the deadline stack exists")
            .push(Instant::now() + CPU_CAP);
        Ok(Self { lua })
    }

    /// The second gate on the 5ms budget, at the Rust boundary rather than inside the VM
    /// (build-steps.md Phase 19 item 3). The hook raises an ordinary Lua error inside the running
    /// function, so a `pcall` in a `computed` body catches it and carries on -- measured, a body
    /// looping over `pcall(function() ... end)` ran 7.5x the cap and returned a partially
    /// computed `Ok` value. Checking the governing deadline again once the call has returned
    /// makes a caught-and-ignored hook error an `Err` anyway: config Lua can catch the hook, but
    /// it cannot catch this.
    ///
    /// ponytail: this gate only runs when the call *returns*. A body that both swallows the hook
    /// error and never returns (`while true do pcall(f) end`) still spins until a hook fire
    /// happens to land on an instruction outside the `pcall`, which is eventually but not
    /// promptly. Bounding that properly needs preemption this VM cannot offer from inside itself;
    /// the upgrade path is the generation-swap process boundary (`docs/adr/0039`), which can kill
    /// a wedged renderer outright.
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
                self.lua.app_data_mut::<Vec<Instant>>().expect("CpuBudget::enter always runs before its Drop");
            stack.pop();
            stack.len()
        };
        if remaining == 0 {
            // Both, and in this order: `remove_global_hook` clears the callback so any coroutine
            // that inherited the hook mask stops calling back (mlua's `global_hook_proc` then
            // clears its own thread's hook on its next fire), and `remove_hook` clears the mask on
            // *this* thread now, so unrelated top-level Lua after a finished evaluation is not
            // paying for a hook that is about to no-op anyway.
            self.lua.remove_global_hook();
            self.lua.remove_hook();
        }
    }
}

/// Whether the outermost live [`CpuBudget`]'s deadline has passed. `first()`, not `min()` or
/// `last()`: see [`CpuBudget`]'s doc comment for the monotonicity argument that makes `first()`
/// the minimum and why the other two are wrong. An empty stack (no evaluation in flight) is never
/// expired, which is what lets [`CpuBudget::enter`] install the hook before its first push.
fn governing_deadline_expired(lua: &Lua) -> bool {
    lua.app_data_ref::<Vec<Instant>>()
        .and_then(|stack| stack.first().copied())
        .is_some_and(|deadline| Instant::now() > deadline)
}

/// Registers the `computed(dependencies, fn)` global (§ 1.2). `dependencies` must be an array of
/// `Signal` userdata handles.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "computed",
        lua.create_function(|_, (deps, func): (Table, Function)| {
            let mut collected = Vec::new();
            for dep in deps.sequence_values::<mlua::AnyUserData>() {
                let dep = dep?;
                let signal = dep.borrow::<Signal>()?.clone();
                collected.push(signal);
            }
            Ok(Signal(SignalKind::Computed { deps: collected, func }))
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_signal(name: &str, value: Value) -> Lua {
        let lua = Lua::new();
        register(&lua).unwrap();
        let signal = Signal::try_new_direct(value).unwrap();
        lua.globals().set(name, signal).unwrap();
        lua
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
        register(&lua).unwrap();
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
        // No memoization: rebuilding the dependency signal under the same global name and
        // re-reading the computed's :get() must observe the new value, not a cached first read.
        let lua = Lua::new();
        register(&lua).unwrap();
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
        register(&lua).unwrap();
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
        // A computed body reading a *second* Signal (an upvalue/global, not just its declared
        // `dependencies`) re-enters `call_with_cpu_cap` before the outer call returns. The inner
        // call must hand enforcement back to the outer one on return, not erase it.
        let lua = Lua::new();
        register(&lua).unwrap();
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
        register(&lua).unwrap();
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
        // build-steps.md Phase 19 item 3, defect 3: `loop = computed({}, function() return
        // loop:get() end)` recurses through Signal::get_value with no bound the CPU cap can stop
        // (see MAX_SIGNAL_NESTING_DEPTH's doc comment: it recursed the process into a stack
        // overflow before this cap existed -- observed with this exact test run alone, recorded
        // in the task report).
        let lua = Lua::new();
        register(&lua).unwrap();
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
        register(&lua).unwrap();
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
        // build-steps.md Phase 19 item 3: a dependency *chain* nests `Signal::get_value` frames
        // without nesting any Lua call, so it is the shape the nesting cap has to bound. Before
        // the deadline push moved to wrap dependency resolution, a chain this long aborted the
        // process with `fatal runtime error: stack overflow`; the cap never saw depth above 1.
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
        // The assertions are about which *gate* fired, not about the value, because the 5ms
        // budget is wall clock and not CPU time: on a loaded machine a chain this long can be
        // descheduled past its deadline, and the whole suite running in parallel is exactly that
        // machine. A CPU-cap error at the limit is the budget doing its job; what must never
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
        // ADR-0044 decision 3's "no memoization" makes a diamond graph exponential in depth: each
        // level evaluates its single dependency once per reference. 20 levels is 2^20 - 1 closure
        // calls, which ran to completion in 1.9s returning Ok(1048576) before the deadline covered
        // dependency resolution. The one deadline the outermost `get_value` pushes is what bounds
        // it now: instrumented, the cap fires after roughly 3,200 closure calls.
        //
        // The graph is built in a separate `exec` so `start` times only the evaluation. Building
        // it is itself exponential and dominates the wall clock -- `Signal` is `Clone` by value,
        // so `computed({s, s}, f)` deep-copies `s` twice and the "diamond" is a 2^20-node tree in
        // memory, not a shared DAG. That construction cost is not what this test is about.
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.load("for _ = 1, 20 do a = computed({a, a}, function(x, y) return x + y end) end").exec().unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> = lua.load("return a:get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "an exponential diamond graph must be cut off, not returned: {result:?}");
        assert!(elapsed < Duration::from_millis(100), "the shared 5ms budget must cut it off early, took {elapsed:?}");
    }

    #[test]
    fn a_pcall_swallowing_the_hook_error_still_fails_at_the_rust_boundary() {
        // The hook raises an ordinary Lua error inside the running function, so a `pcall` in the
        // body catches it and carries on with a partially computed value. Bounded iteration
        // counts, not `while true`, so a regression here is a slow test rather than a hung runner.
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
        // ran entirely unhooked: measured 5.75s returning `Ok`. Bounded iteration count so a
        // regression is slow rather than a permanent hang; the elapsed assertion is what catches
        // the coroutine escaping the hook, since the Rust-boundary gate would error either way.
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
        register(&lua).unwrap();
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
}
