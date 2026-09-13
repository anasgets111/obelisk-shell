//! Capability and [`CapabilityHandle`] share the live signal, revision, and write path
//! (ADR-0052 decision 1). [`CommandSender::send`] queues `{capability, action, arguments}` as a
//! `RendererFrame::Command` on the channel drained by the socket thread's `pump` (ADR-0039), since
//! Lua runs on the Wayland dispatch thread and has no socket in scope. `Rc`, not `Arc`, is correct
//! because this state stays on that thread.
//!
//! One userdata owns both halves. Commands are invoked as methods via
//! `capability:invoke("action", ...)`, and ADR-0052 decision 4 reads lock state through the name
//! it locks, so `obelisk.lock:get().attempts` and `obelisk.lock:invoke("lock")` use the same
//! object; [`Capability`] delegates `get`/`map` to its [`Signal`].
//!
//! ponytail: an `__index` upgrade cannot distinguish `cap.lock()` from `cap:lock()`, which is why
//! commands dispatch through `invoke` instead of bare per-action methods (ADR-0052 decision 1).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, MultiValue, UserData, UserDataMethods, Value};
use shared::{CommandEnvelope, CommandParams, RendererFrame};
use tokio::sync::mpsc::UnboundedSender;

use crate::lua::signal::{CpuBudget, DirtyFlag, LiveSignalHandle, Signal};

/// Builds the generation-guarded envelope and queues it for the socket thread. One sender per
/// generation is cloned into every [`Capability`] on `obelisk`.
#[derive(Clone)]
pub struct CommandSender {
    generation_id: u32,
    /// JSON-RPC request id (e.g. `"id": 105`), shared across clones so capabilities never reuse
    /// an id. `Rc<Cell<_>>` is safe here because the sender is single-threaded.
    next_id: Rc<Cell<u64>>,
    /// Capabilities this generation already asked the Supervisor to start. Shared across clones;
    /// `lua::namespace` and `secure_submit`'s sweep both use it (ADR-0070 decisions 1, 5), so a
    /// second `obelisk.audio` reader costs nothing.
    started: Rc<RefCell<HashSet<String>>>,
    outbound_tx: UnboundedSender<RendererFrame>,
}

impl CommandSender {
    /// `generation_id` comes from `socket::generation_id_from_env`, the value `ProcessRegistry`
    /// stamps into every command envelope.
    pub fn new(generation_id: u32, outbound_tx: UnboundedSender<RendererFrame>) -> Self {
        CommandSender {
            generation_id,
            next_id: Rc::new(Cell::new(0)),
            started: Rc::new(RefCell::new(HashSet::new())),
            outbound_tx,
        }
    }

    /// Asks the Supervisor to construct `capability`'s controller once per generation. The
    /// Supervisor drops repeats (ADR-0070 decision 3); the local set also stops a `map` over
    /// `obelisk.audio` from writing a frame on every layout pass.
    pub(crate) fn start_capability(&self, capability: &str) {
        if !self.started.borrow_mut().insert(capability.to_string()) {
            return;
        }
        let frame = RendererFrame::StartCapability { capability: capability.to_string() };
        if self.outbound_tx.send(frame).is_err() {
            eprintln!("obelisk.{capability}: failed to queue the start request, the control-socket writer is gone");
        }
    }

    /// The channel where `RendererClient` queues non-command frames (`ReevaluateReport`,
    /// `RequestReload`) alongside commands.
    pub fn frames(&self) -> UnboundedSender<RendererFrame> {
        self.outbound_tx.clone()
    }

    /// Queues the command envelope. `expected_revision` is the last hydrated `StateSnapshot` revision,
    /// kept current by [`CapabilityHandle`]. `0` means "never hydrated": `bump_revision` starts
    /// at `1`, and state-less `lock` capabilities send it forever (ADR-0052 decision 1,
    /// `process.rs`).
    pub(crate) fn send(
        &self,
        capability: &str,
        action: &str,
        arguments: Vec<serde_json::Value>,
        expected_revision: u32,
    ) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let envelope = CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "ExecuteCommand".to_string(),
            params: CommandParams {
                generation_id: self.generation_id,
                capability: capability.to_string(),
                action: action.to_string(),
                arguments,
                expected_revision,
            },
            id,
        };
        if self.outbound_tx.send(RendererFrame::Command(envelope)).is_err() {
            eprintln!(
                "obelisk.{capability}:invoke(\"{action}\"): failed to queue the command, the control-socket writer is gone"
            );
        }
    }
}

/// One `obelisk` member: its live signal and write path. `name` is the
/// `shared::Capability::ALL` roster name, the Lua field and every envelope's `capability` value.
///
/// Only `lua::idle` clones it, wrapping `obelisk.idle` so three threshold methods share userdata
/// with `get`/`map`/`on_change` (ADR-0141). The signal and handler list remain shared.
#[derive(Clone)]
pub struct Capability {
    name: String,
    signal: Signal,
    /// Shared with the paired [`CapabilityHandle`], so `invoke` stamps the last snapshot revision
    /// the config could have read; see [`CommandSender::send`].
    revision: Rc<Cell<u32>>,
    commands: CommandSender,
    /// `on_change` handlers run by [`CapabilityHandle::notify_change`] after each push (ADR-0115).
    handlers: Rc<RefCell<Vec<Function>>>,
}

impl Capability {
    /// Builds an `obelisk.<name>` member and the handle `socket::RendererClient` hydrates. Return
    /// them together: value and revision must move as one, because ordinary dispatch does not
    /// enforce envelope revision claims. Pairing them here is the only
    /// guard against a `set` that stamps a stale read onto the current write.
    pub fn new(name: &str, dirty: DirtyFlag, commands: CommandSender) -> (Self, CapabilityHandle) {
        // `nil` until the Supervisor's first push (ADR-0037), paired with revision `0`, which no
        // push can produce.
        let (signal, signal_handle) = Signal::new_live(Value::Nil, dirty);
        let revision = Rc::new(Cell::new(0));
        let handlers = Rc::new(RefCell::new(Vec::new()));
        let capability = Capability {
            name: name.to_string(),
            signal,
            revision: Rc::clone(&revision),
            commands,
            handlers: Rc::clone(&handlers),
        };
        (capability, CapabilityHandle { name: name.to_string(), signal: signal_handle, revision, handlers })
    }

    /// Registers an `on_change` handler, including for wrappers such as `lua::idle` that re-export
    /// this capability's read half.
    pub fn add_handler(&self, handler: Function) {
        self.handlers.borrow_mut().push(handler);
    }

    /// Roster name for a wrapper that must send the `start_capability` request hidden by
    /// `obelisk`'s `__index` path.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Command sender, so a wrapper can announce a read hidden by `__index`.
    pub fn commands(&self) -> &CommandSender {
        &self.commands
    }

    /// Wrapped read signal for `signal::from_userdata`, allowing live forms
    /// (`content = obelisk.mpris`, `computed({obelisk.audio}, f)`) through a wrapper the engine
    /// otherwise cannot see past.
    pub fn signal(&self) -> Signal {
        self.signal.clone()
    }
}

/// Rust-side half of an `obelisk.<name>` member, where `StateSnapshot` writes. One handle carries
/// the `LiveSignalHandle` and revision because `socket::RendererClient` holds one per capability.
#[derive(Clone)]
pub struct CapabilityHandle {
    name: String,
    signal: LiveSignalHandle,
    revision: Rc<Cell<u32>>,
    handlers: Rc<RefCell<Vec<Function>>>,
}

impl CapabilityHandle {
    /// Writes a `StateSnapshot` revision before its value. The value write marks the scene dirty
    /// (ADR-0044 decision 2), so it must go last. Returns the replaced value for
    /// [`Self::notify_change`].
    pub fn hydrate(&self, value: Value, revision: u32) -> Value {
        self.revision.set(revision);
        let previous = self.signal.get();
        self.signal.set(value);
        previous
    }

    /// Runs each `on_change` handler with `(current, previous)` (ADR-0115), under its own 5ms CPU
    /// budget, the same budget as `map`. A raising handler is logged and skipped after the push;
    /// handlers run from a copied list, so one can register another without borrowing the
    /// `RefCell` recursively.
    pub fn notify_change(&self, lua: &Lua, previous: Value) {
        let handlers = self.handlers.borrow().clone();
        if handlers.is_empty() {
            return;
        }
        let current = self.signal.get();
        for handler in handlers {
            let outcome = CpuBudget::enter(lua).and_then(|budget| {
                handler.call::<()>((current.clone(), previous.clone()))?;
                budget.check_not_exceeded()
            });
            if let Err(err) = outcome {
                eprintln!("obelisk.{}:on_change handler raised, ignoring it: {err}", self.name);
            }
        }
    }

    /// Clears handlers before the client re-evaluates `shell.lua`; re-registering without this
    /// would double every reload side effect.
    pub fn clear_handlers(&self) {
        self.handlers.borrow_mut().clear();
    }
}

impl UserData for Capability {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Delegate `get`/`map` so capabilities read like bare `Signal` globals
        // (`rescue`, `screens`).
        methods.add_method("get", |lua, this, ()| this.signal.get_value(lua));
        methods.add_method("map", |_, this, f: Function| Ok(this.signal.mapped(f)));
        // The one non-rendering push reaction (ADR-0115): once per `StateSnapshot`, outside layout,
        // with new and old payloads, and input-callback powers (`invoke`, `process.run`, state).
        methods.add_method("on_change", |_, this, f: Function| {
            this.handlers.borrow_mut().push(f);
            Ok(())
        });
        // No `set`: capability state is read-only (ADR-0044 decision 5); use `invoke`.
        methods.add_method("invoke", |lua, this, (action, args): (String, MultiValue)| {
            let mut arguments = Vec::with_capacity(args.len());
            for (index, value) in args.into_iter().enumerate() {
                // Reject here instead of dropping the slot; a config error naming argument 3 is
                // easier to debug than a malformed command at the Supervisor.
                let json = lua.from_value::<serde_json::Value>(value).map_err(|err| {
                    mlua::Error::runtime(format!(
                        "obelisk.{}:invoke(\"{action}\") could not marshal argument {}: {err}",
                        this.name,
                        index + 1
                    ))
                })?;
                arguments.push(json);
            }
            this.commands.send(&this.name, &action, arguments, this.revision.get());
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use mlua::{Lua, Value};
    use tokio::sync::mpsc;

    use super::*;

    /// Test VM with an unowned `obelisk.probe`; the roster name is irrelevant to the write path.
    fn lua_with_capability(generation_id: u32) -> (Lua, CapabilityHandle, mpsc::UnboundedReceiver<RendererFrame>) {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(generation_id, tx));
        let table = lua.create_table().unwrap();
        table.set("probe", capability).unwrap();
        lua.globals().set("obelisk", table).unwrap();
        (lua, handle, rx)
    }

    fn queued_command(rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Option<CommandEnvelope> {
        match rx.try_recv().ok()? {
            RendererFrame::Command(envelope) => Some(envelope),
            other => panic!("a capability write must be queued as RendererFrame::Command, got {other:?}"),
        }
    }

    #[test]
    fn invoke_queues_the_generation_guarded_envelope() {
        let (lua, _handle, mut rx) = lua_with_capability(4);

        lua.load(r#"obelisk.probe:invoke("set_volume", 0.75)"#).exec().unwrap();

        let envelope = queued_command(&mut rx).expect("invoke must queue a command");
        assert_eq!(envelope.jsonrpc, "2.0");
        assert_eq!(envelope.method, "ExecuteCommand");
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "probe");
        assert_eq!(envelope.params.action, "set_volume");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!(0.75)]);
        // No snapshot is hydrated; `bump_revision` starts at 1, so `0` is correct.
        assert_eq!(envelope.params.expected_revision, 0);
    }

    #[test]
    fn invoke_stamps_the_revision_of_the_snapshot_the_config_could_last_have_read() {
        let (lua, handle, mut rx) = lua_with_capability(0);

        handle.hydrate(Value::Nil, 7);
        lua.load(r#"obelisk.probe:invoke("set_volume", 0.5)"#).exec().unwrap();
        assert_eq!(queued_command(&mut rx).unwrap().params.expected_revision, 7);

        handle.hydrate(Value::Nil, 8);
        lua.load(r#"obelisk.probe:invoke("set_volume", 0.6)"#).exec().unwrap();
        assert_eq!(queued_command(&mut rx).unwrap().params.expected_revision, 8);
    }

    #[test]
    fn an_action_with_no_arguments_sends_an_empty_array_not_a_missing_field() {
        // `null` would not deserialize into `Vec<serde_json::Value>`.
        let (lua, _handle, mut rx) = lua_with_capability(0);

        lua.load(r#"obelisk.probe:invoke("lock")"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().params.arguments, Vec::<serde_json::Value>::new());
    }

    #[test]
    fn each_invoke_gets_a_distinct_json_rpc_id() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        lua.load(r#"obelisk.probe:invoke("a"); obelisk.probe:invoke("b")"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().id, 0);
        assert_eq!(queued_command(&mut rx).unwrap().id, 1);
    }

    #[test]
    fn an_unmarshallable_argument_is_a_config_error_naming_its_slot_and_queues_nothing() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        let err = lua.load(r#"obelisk.probe:invoke("connect", "ssid", function() end)"#).exec().unwrap_err();

        assert!(err.to_string().contains("argument 2"), "the error must name the offending slot: {err}");
        assert!(rx.try_recv().is_err(), "a refused argument must not queue a half-built command");
    }

    #[test]
    fn on_change_runs_after_a_push_with_the_new_and_the_replaced_payload() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load(
            r#"
            seen = {}
            obelisk.probe:on_change(function(current, previous)
                seen[#seen + 1] = { current = current, previous = previous }
            end)
        "#,
        )
        .exec()
        .unwrap();

        let previous = handle.hydrate(Value::Integer(1), 1);
        handle.notify_change(&lua, previous);
        let previous = handle.hydrate(Value::Integer(2), 2);
        handle.notify_change(&lua, previous);

        let seen: mlua::Table = lua.globals().get("seen").unwrap();
        assert_eq!(seen.len().unwrap(), 2);
        let first: mlua::Table = seen.get(1).unwrap();
        assert_eq!(first.get::<i64>("current").unwrap(), 1);
        assert_eq!(first.get::<Value>("previous").unwrap(), Value::Nil, "the first push replaces nil");
        let second: mlua::Table = seen.get(2).unwrap();
        assert_eq!(second.get::<i64>("current").unwrap(), 2);
        assert_eq!(second.get::<i64>("previous").unwrap(), 1);
    }

    #[test]
    fn a_raising_handler_does_not_stop_the_next_one_or_the_push() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load(
            r#"
            ran = false
            obelisk.probe:on_change(function() error("first handler broke") end)
            obelisk.probe:on_change(function() ran = true end)
        "#,
        )
        .exec()
        .unwrap();

        let previous = handle.hydrate(Value::Integer(5), 1);
        handle.notify_change(&lua, previous);

        assert!(lua.globals().get::<bool>("ran").unwrap());
        let read: i64 = lua.load("return obelisk.probe:get()").eval().unwrap();
        assert_eq!(read, 5);
    }

    #[test]
    fn clear_handlers_forgets_what_the_last_evaluation_registered() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load("count = 0; obelisk.probe:on_change(function() count = count + 1 end)").exec().unwrap();
        handle.clear_handlers();
        let previous = handle.hydrate(Value::Integer(1), 1);
        handle.notify_change(&lua, previous);
        assert_eq!(lua.globals().get::<i64>("count").unwrap(), 0);
    }

    #[test]
    fn get_reads_the_same_live_value_a_bare_capability_global_would() {
        let (lua, handle, _rx) = lua_with_capability(0);

        let before: bool = lua.load("return obelisk.probe:get() == nil").eval().unwrap();
        assert!(before, "a capability reads nil until its first snapshot (ADR-0037)");

        let pushed = lua.create_table().unwrap();
        pushed.set("attempts", 2).unwrap();
        handle.hydrate(Value::Table(pushed), 1);

        let attempts: i64 = lua.load("return obelisk.probe:get().attempts").eval().unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    fn map_returns_a_signal_that_tracks_later_pushes() {
        // The lock screen's failure text maps this handle; freezing the map at registration would
        // paint the first snapshot forever.
        let (lua, handle, _rx) = lua_with_capability(0);
        lua.load(r#"mapped = obelisk.probe:map(function(s) return (s and s.error) or "" end)"#).exec().unwrap();

        let first: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(first, "");

        let pushed = lua.create_table().unwrap();
        pushed.set("error", "authentication failed").unwrap();
        handle.hydrate(Value::Table(pushed), 2);

        let second: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(second, "authentication failed");
    }
}
