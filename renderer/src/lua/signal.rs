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

use std::cell::RefCell;
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
    pub fn new_live(initial: Value) -> (Self, LiveSignalHandle) {
        let cell = Rc::new(RefCell::new(initial));
        (Signal(SignalKind::Live(Rc::clone(&cell))), LiveSignalHandle(cell))
    }

    /// Reads this signal's current value (ADR-0044 decision 1, `CONTEXT.md`'s Signal resolution
    /// entry). `pub(crate)`, not private: `layout::node`'s property parsers call this directly to
    /// resolve a `Signal` userdata found in a property slot, instead of rejecting it -- the same
    /// method `Signal::get`'s Lua-facing method already calls, just reachable from Rust now too.
    /// `&Lua` is threaded in rather than recovered from `self`, because a `Computed` signal's
    /// closure runs through `call_with_cpu_cap`, which needs a real `Lua` to install its
    /// instruction-count hook on -- mlua 0.12 exposes no way to recover a `Lua` from an
    /// `AnyUserData`/`Value` (checked the vendored source under `~/.cargo/registry`; no such
    /// accessor exists), so there is nothing to recover it from.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        match &self.0 {
            SignalKind::Direct(value) => Ok(value.clone()),
            SignalKind::Live(cell) => Ok(cell.borrow().clone()),
            SignalKind::Computed { deps, func } => {
                let mut args = Vec::with_capacity(deps.len());
                for dep in deps {
                    args.push(dep.get_value(lua)?);
                }
                call_with_cpu_cap(lua, func, MultiValue::from_vec(args))
            }
        }
    }
}

/// The Rust-side handle to a [`Signal::new_live`] signal's storage: lets Rust push a new value in
/// after construction, e.g. on every received `StateSnapshot` (Phase 11). The paired `Signal`
/// (Lua-side) always reads whatever was last set here -- no memoization, matching every other
/// `Signal` kind in this file.
#[derive(Clone)]
pub struct LiveSignalHandle(Rc<RefCell<Value>>);

impl LiveSignalHandle {
    pub fn set(&self, value: Value) {
        *self.0.borrow_mut() = value;
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

/// Runs `func(args)` with a hook installed that aborts the call once it's run past [`CPU_CAP`].
///
/// `Lua::set_hook`/`remove_hook` operate on one unstacked slot per Lua thread, not a stack --
/// but this call can be reentrant: a `computed`/`map` body can read a *second* `Signal` (an
/// upvalue or a global, not just its declared `dependencies`), which re-enters this function
/// before the outer call returns. An earlier version installed/removed the hook per call, so the
/// inner call's `remove_hook()` silently stripped the outer call's still-active cap, leaving the
/// rest of the outer body's execution unguarded. The deadline stack in `app_data` fixes that: the
/// hook is installed only on the 0->1 transition and removed only on the 1->0 transition, and it
/// always checks the innermost (topmost) active deadline, so a finished inner call correctly
/// hands enforcement back to the outer one instead of erasing it.
fn call_with_cpu_cap(lua: &Lua, func: &Function, args: MultiValue) -> mlua::Result<Value> {
    let deadline = Instant::now() + CPU_CAP;
    if push_deadline(lua, deadline) == 1 {
        lua.set_hook(
            mlua::HookTriggers { every_nth_instruction: Some(CHECK_EVERY_N_INSTRUCTIONS), ..mlua::HookTriggers::new() },
            |lua, _| {
                let expired = lua
                    .app_data_ref::<Vec<Instant>>()
                    .and_then(|stack| stack.last().copied())
                    .is_some_and(|deadline| Instant::now() > deadline);
                if expired {
                    Err(mlua::Error::runtime("computed/map exceeded its 5ms CPU budget"))
                } else {
                    Ok(mlua::VmState::Continue)
                }
            },
        )?;
    }
    let result = func.call::<Value>(args);
    if pop_deadline(lua) == 0 {
        lua.remove_hook();
    }
    result
}

/// Pushes `deadline` onto the per-`Lua` deadline stack (creating it on first use) and returns
/// the new stack depth.
fn push_deadline(lua: &Lua, deadline: Instant) -> usize {
    if lua.app_data_ref::<Vec<Instant>>().is_none() {
        lua.set_app_data(Vec::<Instant>::new());
    }
    let mut stack = lua.app_data_mut::<Vec<Instant>>().expect("just ensured the deadline stack exists");
    stack.push(deadline);
    stack.len()
}

/// Pops the innermost deadline and returns the remaining stack depth.
fn pop_deadline(lua: &Lua) -> usize {
    let mut stack = lua.app_data_mut::<Vec<Instant>>().expect("push_deadline always runs before pop_deadline");
    stack.pop();
    stack.len()
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
        let (signal, handle) = Signal::new_live(Value::Integer(1));
        lua.globals().set("live", signal).unwrap();

        let first: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(first, 1, "must read the value passed to new_live before any push");

        handle.set(Value::Integer(42));
        let second: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(second, 42, "must reflect the pushed value without re-registering the global");
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
