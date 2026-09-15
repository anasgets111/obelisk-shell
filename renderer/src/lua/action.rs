//! `action(name, fn)`: the config's answer to `obelisk call <name>` (ADR-0197).
//!
//! The outward direction of [`capability`](super::capability), and the only way anything outside
//! this process runs config code with an effect. A keybind writes a `state` when it wants the shell
//! to *look* different and calls an action when it wants the shell to *do* something; rendering may
//! not have effects, so a `state` write could never stop a recording.
//!
//! `name` is one opaque string. `"rec.toggle"` groups for a reader the way a Lua module path does,
//! and nothing splits it, so an action may contain any character its config wrote and no delimiter
//! rule can surprise anyone.
//!
//! Registrations last exactly one evaluation. They are closures over that evaluation's locals, so a
//! handler kept past a reload would call yesterday's code over yesterday's captures -- the reason
//! `capability::CapabilityHandle`'s `on_change` handlers are cleared the same way (ADR-0115).

use std::collections::HashMap;

use mlua::{Function, Lua, LuaSerdeExt, MultiValue, Value};

use super::signal::CpuBudget;

/// Most an action may answer with. Far below `framing::MAX_FRAME_LEN`, which a larger answer would
/// breach on the way out -- and a frame that cannot be written takes the Renderer's socket with it,
/// so one config's oversized return would cost the whole connection rather than its own call. A
/// megabyte is already past what `obelisk call` can usefully print.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

/// Every `action(name, fn)` this evaluation declared. In `app_data` beside the other registries, so
/// ADR-0044 decision 4's persistent VM holds it and a replaced Renderer starts empty.
#[derive(Default)]
struct ActionRegistry(HashMap<String, Function>);

pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "action",
        lua.create_function(|lua, (name, handler): (String, Function)| {
            if name.is_empty() {
                return Err(mlua::Error::runtime("action() needs a name; `obelisk call` has nothing to ask for"));
            }
            if lua.app_data_ref::<ActionRegistry>().is_none() {
                lua.set_app_data(ActionRegistry::default());
            }
            let mut registry = lua.app_data_mut::<ActionRegistry>().expect("just ensured the registry exists");
            // Refused rather than replaced: two modules claiming one name is a collision the config
            // author has to see, and silently keeping the last would make which one wins depend on
            // `require` order.
            if registry.0.contains_key(&name) {
                return Err(mlua::Error::runtime(format!(
                    "action({name:?}) was already declared this evaluation; two of them cannot share a name"
                )));
            }
            registry.0.insert(name, handler);
            Ok(())
        })?,
    )
}

/// Runs `name`'s handler, or says why it could not. Never raises: a config's mistake is this
/// caller's answer, not a dead frame.
pub fn dispatch(lua: &Lua, name: &str, arguments: &[serde_json::Value]) -> shared::CallOutcome {
    let Some(handler) = lua.app_data_ref::<ActionRegistry>().and_then(|registry| registry.0.get(name).cloned()) else {
        return shared::CallOutcome::Failed(format!("this config declares no action({name:?})"));
    };
    let mut args = Vec::with_capacity(arguments.len());
    for (index, argument) in arguments.iter().enumerate() {
        match super::json::to_lua(lua, argument) {
            Ok(value) => args.push(value),
            Err(err) => {
                return shared::CallOutcome::Failed(format!("argument {} does not convert to Lua: {err}", index + 1));
            }
        }
    }
    // The same cap a capability's `on_change` handler runs under: config code reached from outside
    // is still config code, and a handler that spins must not take the frame with it.
    let returned = CpuBudget::enter(lua).and_then(|budget| {
        let value = handler.call::<Value>(MultiValue::from_vec(args))?;
        budget.check_not_exceeded()?;
        Ok(value)
    });
    match returned {
        // `nil` and no return at all are the same value here, because Lua cannot tell them apart.
        // `from_value`, as `capability::invoke` marshals its arguments: one conversion for both
        // directions across this boundary.
        Ok(value) => match lua.from_value::<serde_json::Value>(value) {
            Ok(json) => match serde_json::to_vec(&json).map(|bytes| bytes.len()) {
                Ok(len) if len > MAX_ANSWER_BYTES => shared::CallOutcome::Failed(format!(
                    "what it returned is {len} bytes, over the {MAX_ANSWER_BYTES}-byte limit for an answer"
                )),
                Ok(_) => shared::CallOutcome::Returned(json),
                Err(err) => shared::CallOutcome::Failed(format!("what it returned does not serialize: {err}")),
            },
            Err(err) => shared::CallOutcome::Failed(format!("what it returned does not convert to JSON: {err}")),
        },
        Err(err) => shared::CallOutcome::Failed(err.to_string()),
    }
}

/// Drops every registration before an evaluation re-adds them, the counterpart to
/// `capability::CapabilityHandle::clear_handlers`. Also correct after a *failed* evaluation: half a
/// config's actions answering is worse than none, and the scene still on screen belongs to the
/// evaluation before it.
pub fn clear(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<ActionRegistry>() {
        registry.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VM with `action` installed and nothing else; dispatch needs no scene.
    fn lua() -> Lua {
        let lua = Lua::new();
        register(&lua).unwrap();
        lua
    }

    fn failure(outcome: shared::CallOutcome) -> String {
        match outcome {
            shared::CallOutcome::Failed(why) => why,
            shared::CallOutcome::Returned(value) => panic!("expected a failure, got {value}"),
        }
    }

    #[test]
    fn a_declared_action_runs_and_its_return_comes_back_as_json() {
        let lua = lua();
        lua.load(r#"action("rec.toggle", function() return "recording" end)"#).exec().unwrap();
        assert_eq!(dispatch(&lua, "rec.toggle", &[]), shared::CallOutcome::Returned(serde_json::json!("recording")));
    }

    #[test]
    fn arguments_arrive_in_order_and_the_answer_may_be_any_json() {
        let lua = lua();
        lua.load(r#"action("sum", function(a, b) return { total = a + b } end)"#).exec().unwrap();
        let outcome = dispatch(&lua, "sum", &[serde_json::json!(2), serde_json::json!(3)]);
        assert_eq!(outcome, shared::CallOutcome::Returned(serde_json::json!({ "total": 5 })));
    }

    #[test]
    fn returning_nothing_and_returning_nil_are_the_same_answer() {
        let lua = lua();
        lua.load(r#"action("quiet", function() end)"#).exec().unwrap();
        lua.load(r#"action("nil", function() return nil end)"#).exec().unwrap();
        // Lua cannot tell them apart, so neither may this: inventing a difference here would invent
        // one in every config.
        assert_eq!(dispatch(&lua, "quiet", &[]), shared::CallOutcome::Returned(serde_json::Value::Null));
        assert_eq!(dispatch(&lua, "nil", &[]), shared::CallOutcome::Returned(serde_json::Value::Null));
    }

    #[test]
    fn an_undeclared_name_is_a_failure_that_names_it_rather_than_a_panic() {
        let why = failure(dispatch(&lua(), "no.such.thing", &[]));
        assert!(why.contains("no.such.thing"), "the caller has only this line to debug with: {why}");
    }

    #[test]
    fn a_handler_that_raises_answers_the_caller_instead_of_killing_the_frame() {
        let lua = lua();
        lua.load(r#"action("boom", function() error("no") end)"#).exec().unwrap();
        assert!(failure(dispatch(&lua, "boom", &[])).contains("no"));
    }

    #[test]
    fn a_returned_value_that_cannot_be_json_is_a_failure_not_a_silent_null() {
        let lua = lua();
        // A function has no JSON form. Answering `null` would report success for a config that
        // returned something it cannot send.
        lua.load(r#"action("fn", function() return function() end end)"#).exec().unwrap();
        failure(dispatch(&lua, "fn", &[]));
    }

    #[test]
    fn an_answer_past_the_size_limit_fails_that_call_rather_than_the_connection() {
        let lua = lua();
        // A frame over `framing::MAX_FRAME_LEN` cannot be written, and the writer treats a failed
        // write as a dead socket. One config's runaway return must not cost the connection.
        //
        // Built at registration, not in the handler: `string.rep` of a megabyte inside the call
        // spends the 5ms CPU budget first, so the test would prove that cap rather than this one.
        // A handler returning something it already held is also the case worth guarding.
        lua.load(format!(
            r#"local big = string.rep("x", {}) action("big", function() return big end)"#,
            MAX_ANSWER_BYTES + 1
        ))
        .exec()
        .unwrap();
        let why = failure(dispatch(&lua, "big", &[]));
        assert!(why.contains("limit"), "{why}");
    }

    #[test]
    fn a_returned_error_table_is_a_value_and_not_a_failure() {
        let lua = lua();
        lua.load(r#"action("t", function() return { error = "nope" } end)"#).exec().unwrap();
        // A config returning a table shaped like an error is still returning a table; only a raise
        // is a failure, or the two could never be told apart.
        assert_eq!(dispatch(&lua, "t", &[]), shared::CallOutcome::Returned(serde_json::json!({ "error": "nope" })));
    }

    #[test]
    fn two_declarations_of_one_name_are_refused_rather_than_the_last_one_winning() {
        let lua = lua();
        lua.load(r#"action("dup", function() return 1 end)"#).exec().unwrap();
        let err = lua.load(r#"action("dup", function() return 2 end)"#).exec().unwrap_err().to_string();
        assert!(err.contains("dup"), "which module collided is the whole message: {err}");
        // The first is still the one that answers, so a refused second cannot half-replace it.
        assert_eq!(dispatch(&lua, "dup", &[]), shared::CallOutcome::Returned(serde_json::json!(1)));
    }

    #[test]
    fn an_empty_name_is_refused_because_nothing_could_ask_for_it() {
        let lua = lua();
        assert!(lua.load(r#"action("", function() end)"#).exec().is_err());
    }

    #[test]
    fn a_registration_made_before_an_evaluation_raised_does_not_survive_it() {
        let lua = lua();
        // `shell.lua` may declare several actions and then fail. Clearing before evaluation cannot
        // reach these, so the caller clears again on failure; without it a rejected config keeps
        // answering `obelisk call`.
        assert!(lua.load(r#"action("half", function() return 1 end) error("bad")"#).exec().is_err());
        assert_eq!(dispatch(&lua, "half", &[]), shared::CallOutcome::Returned(serde_json::json!(1)));
        clear(&lua);
        failure(dispatch(&lua, "half", &[]));
    }

    #[test]
    fn clearing_leaves_nothing_answering_so_a_reload_cannot_run_the_old_closure() {
        let lua = lua();
        lua.load(r#"action("gone", function() return 1 end)"#).exec().unwrap();
        clear(&lua);
        failure(dispatch(&lua, "gone", &[]));
        // And the name is free again, which is what re-evaluation depends on.
        lua.load(r#"action("gone", function() return 2 end)"#).exec().unwrap();
        assert_eq!(dispatch(&lua, "gone", &[]), shared::CallOutcome::Returned(serde_json::json!(2)));
    }
}
