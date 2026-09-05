//! The `oblisk` namespace's capability objects: one capability's read signal, its revision, and
//! § 3.2's write path share the same handle (ADR-0052 decision 1). [`CommandSender::send`] builds
//! an envelope from `{capability, action, arguments}` so every write lands on one binding, and the
//! revision lives beside it so a write's stamp and the push that moves it never drift into two
//! maps ([`Capability`] and [`CapabilityHandle`] are the two ends of one). A Lua call on the
//! Wayland dispatch thread has no socket in scope, so it queues a `RendererFrame::Command` onto
//! the one outbound channel the socket thread's `pump` drains (ADR-0039); `Rc`, not `Arc`, since
//! everything here is confined to that one thread.
//!
//! One userdata rather than a signal beside a writer: § 3.2 writes every command as a method on
//! the capability itself, and ADR-0052 decision 4 reads a lock screen's state off the same name it
//! locks through, so [`Capability`] delegates `get`/`map` to the wrapped [`Signal`] and
//! `oblisk.lock:get().attempts` / `oblisk.lock:invoke("lock")` are the same object.
//!
//! ponytail: `capability:invoke("action", ...)`, not § 3.2's `capability:action(...)`. The
//! upgrade is an `__index` metamethod per § 7.1, not taken because it cannot tell `cap.lock()`
//! from `cap:lock()` (ADR-0052 decision 1 vs. § 3.2's spelling).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, MultiValue, UserData, UserDataMethods, Value};
use shared::{CommandEnvelope, CommandParams, RendererFrame};
use tokio::sync::mpsc::UnboundedSender;

use crate::lua::signal::{CpuBudget, DirtyFlag, LiveSignalHandle, Signal};

/// Builds § 7's generation-guarded envelope and queues it for the socket thread. One per
/// generation, cloned into every [`Capability`] on the `oblisk` table.
#[derive(Clone)]
pub struct CommandSender {
    generation_id: u32,
    /// JSON-RPC's request id (§ 7's `"id": 105`), shared across [`Capability`] clones so two
    /// capabilities never hand out the same id. `Rc<Cell<_>>`: single-threaded.
    next_id: Rc<Cell<u64>>,
    /// Capabilities this generation already asked the Supervisor to start, so a second reader of
    /// `oblisk.audio` costs nothing. Shared across clones: `lua::namespace` and `secure_submit`'s
    /// sweep both ask this (ADR-0070 decisions 1, 5).
    started: Rc<RefCell<HashSet<String>>>,
    outbound_tx: UnboundedSender<RendererFrame>,
}

impl CommandSender {
    /// `generation_id` is this Renderer's own generation id (`socket::generation_id_from_env`),
    /// the same value `ProcessRegistry` stamps and the field § 7.3's guard rule drops a packet on.
    pub fn new(generation_id: u32, outbound_tx: UnboundedSender<RendererFrame>) -> Self {
        CommandSender {
            generation_id,
            next_id: Rc::new(Cell::new(0)),
            started: Rc::new(RefCell::new(HashSet::new())),
            outbound_tx,
        }
    }

    /// Asks the Supervisor to construct `capability`'s controller, once per generation. The
    /// Supervisor already drops a repeat (ADR-0070 decision 3); the local set instead keeps a
    /// `map` over `oblisk.audio` that re-resolves every layout pass from writing a frame per pass.
    pub(crate) fn start_capability(&self, capability: &str) {
        if !self.started.borrow_mut().insert(capability.to_string()) {
            return;
        }
        let frame = RendererFrame::StartCapability { capability: capability.to_string() };
        if self.outbound_tx.send(frame).is_err() {
            eprintln!("oblisk.{capability}: failed to queue the start request, the control-socket writer is gone");
        }
    }

    /// The outbound frame channel, handed back so `RendererClient` can queue its own non-command
    /// frames (`ReevaluateReport`, `RequestReload`) onto the same channel a command goes out on.
    pub fn frames(&self) -> UnboundedSender<RendererFrame> {
        self.outbound_tx.clone()
    }

    /// § 7's envelope, queued rather than written: see the module doc comment.
    /// `expected_revision` is the revision of the last `StateSnapshot` hydrated, § 7.3's staleness
    /// half ([`CapabilityHandle`] keeps it current). `0` is not a revision any push can produce
    /// (`bump_revision` starts at `1`), so it means "never hydrated": honest before the first
    /// snapshot, and permanently correct for a capability with no state to be stale about (`lock`,
    /// ADR-0052 decision 1, and `process.rs` send it forever).
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
                "oblisk.{capability}:invoke(\"{action}\"): failed to queue the command, the control-socket writer is gone"
            );
        }
    }
}

/// One member of the `oblisk` table: the capability's live state signal plus its write path.
/// `name` is the `shared::Capability::ALL` roster name, both the Lua field it is registered under
/// and the `capability` field of every envelope it sends.
///
/// `Clone` for exactly one caller: `lua::idle` wraps `oblisk.idle`'s roster member in its own
/// userdata so the three threshold methods sit on the same object as `get`/`map`/`on_change`
/// (ADR-0141). Cloning shares the signal and the handler list rather than copying them.
#[derive(Clone)]
pub struct Capability {
    name: String,
    signal: Signal,
    /// Shared with the [`CapabilityHandle`] built beside this member, so `invoke` stamps the
    /// revision of the snapshot the config could last have read; see [`CommandSender::send`].
    revision: Rc<Cell<u32>>,
    commands: CommandSender,
    /// `on_change` handlers, run by [`CapabilityHandle::notify_change`] after each push (ADR-0115).
    handlers: Rc<RefCell<Vec<Function>>>,
}

impl Capability {
    /// Builds one `oblisk.<name>` member and the handle `socket::RendererClient` hydrates it
    /// through: returned together since value and revision must move as one, or a `set` missing its
    /// bump could stamp a stale read onto the current write, the race § 7.3 exists to drop.
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

    /// Registers an `on_change` handler, for a wrapper that re-exports this capability's read half
    /// under its own userdata (`lua::idle`). The method below does the same for the ordinary case.
    pub fn add_handler(&self, handler: Function) {
        self.handlers.borrow_mut().push(handler);
    }

    /// This capability's roster name, for a wrapper that must send the same `start_capability`
    /// a read through `oblisk`'s `__index` would have sent.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The command sender, so a wrapper can announce a read the `__index` path never sees.
    pub fn commands(&self) -> &CommandSender {
        &self.commands
    }

    /// The wrapped read signal, for `signal::from_userdata`: lets a config write § 1.2's live
    /// spelling (`content = oblisk.mpris`, `computed({oblisk.audio}, f)`) against a capability,
    /// since the engine resolves the inner signal and never sees the wrapper.
    pub fn signal(&self) -> Signal {
        self.signal.clone()
    }
}

/// The Rust-side half of one `oblisk.<name>` member: what a `StateSnapshot` writes into. One
/// handle rather than a `LiveSignalHandle` beside a revision cell, because `socket::RendererClient`
/// holds one per capability and every caller wants both fields.
#[derive(Clone)]
pub struct CapabilityHandle {
    name: String,
    signal: LiveSignalHandle,
    revision: Rc<Cell<u32>>,
    handlers: Rc<RefCell<Vec<Function>>>,
}

impl CapabilityHandle {
    /// Writes one `StateSnapshot` into the Lua-visible signal and records its revision. Revision
    /// first: the value write marks the scene dirty (ADR-0044 decision 2), so it goes last. Returns
    /// what the push replaced, for [`Self::notify_change`].
    pub fn hydrate(&self, value: Value, revision: u32) -> Value {
        self.revision.set(revision);
        let previous = self.signal.get();
        self.signal.set(value);
        previous
    }

    /// Runs every `on_change` handler with `(current, previous)` (ADR-0115). Each call is under
    /// its own 5ms CPU budget, the one a `map` callback gets, and a handler that raises is logged
    /// and skipped: the push has already landed, and one config mistake must not stop the others.
    /// Handlers are called on a copy of the list, so one that registers another does not deadlock
    /// on the `RefCell`.
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
                eprintln!("oblisk.{}:on_change handler raised, ignoring it: {err}", self.name);
            }
        }
    }

    /// Forgets every handler, for the client to call before it re-evaluates `shell.lua`: the
    /// evaluation registers them again, and without this a reload would double every side effect.
    pub fn clear_handlers(&self) {
        self.handlers.borrow_mut().clear();
    }
}

impl UserData for Capability {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `get`/`map` delegate rather than reimplement, so a capability reads like the two bare
        // `Signal` globals beside it in the `oblisk` table (`rescue` and `screens`).
        methods.add_method("get", |lua, this, ()| this.signal.get_value(lua));
        methods.add_method("map", |_, this, f: Function| Ok(this.signal.mapped(f)));
        // The one place a config reacts to a push rather than rendering it (ADR-0115): the handler
        // runs once per `StateSnapshot`, outside any layout pass, with the new and old payloads, and
        // may do what an input callback may do -- `invoke`, `process.run`, write a `state` signal.
        methods.add_method("on_change", |_, this, f: Function| {
            this.handlers.borrow_mut().push(f);
            Ok(())
        });
        // No `set`: ADR-0044 decision 5 keeps capability state read-only; a config commands it via
        // `invoke` instead.
        methods.add_method("invoke", |lua, this, (action, args): (String, MultiValue)| {
            let mut arguments = Vec::with_capacity(args.len());
            for (index, value) in args.into_iter().enumerate() {
                // Fails here rather than dropping the slot: a missing third argument is worse to
                // debug from the Supervisor's side than a config error naming the slot.
                let json = lua.from_value::<serde_json::Value>(value).map_err(|err| {
                    mlua::Error::runtime(format!(
                        "oblisk.{}:invoke(\"{action}\") could not marshal argument {}: {err}",
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

    /// A VM with `oblisk.probe` on it: the tests use a name no capability owns, since the roster
    /// name is irrelevant to a generic write path.
    fn lua_with_capability(generation_id: u32) -> (Lua, CapabilityHandle, mpsc::UnboundedReceiver<RendererFrame>) {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(generation_id, tx));
        let table = lua.create_table().unwrap();
        table.set("probe", capability).unwrap();
        lua.globals().set("oblisk", table).unwrap();
        (lua, handle, rx)
    }

    fn queued_command(rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Option<CommandEnvelope> {
        match rx.try_recv().ok()? {
            RendererFrame::Command(envelope) => Some(envelope),
            other => panic!("a capability write must be queued as RendererFrame::Command, got {other:?}"),
        }
    }

    #[test]
    fn invoke_queues_the_generation_guarded_envelope_section_7_2_specifies() {
        let (lua, _handle, mut rx) = lua_with_capability(4);

        lua.load(r#"oblisk.probe:invoke("set_volume", 0.75)"#).exec().unwrap();

        let envelope = queued_command(&mut rx).expect("invoke must queue a command");
        assert_eq!(envelope.jsonrpc, "2.0");
        assert_eq!(envelope.method, "ExecuteCommand");
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "probe");
        assert_eq!(envelope.params.action, "set_volume");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!(0.75)]);
        // No snapshot hydrated yet, and `bump_revision` starts at 1, so `0` is correct here.
        assert_eq!(envelope.params.expected_revision, 0);
    }

    #[test]
    fn invoke_stamps_the_revision_of_the_snapshot_the_config_could_last_have_read() {
        let (lua, handle, mut rx) = lua_with_capability(0);

        handle.hydrate(Value::Nil, 7);
        lua.load(r#"oblisk.probe:invoke("set_volume", 0.5)"#).exec().unwrap();
        assert_eq!(queued_command(&mut rx).unwrap().params.expected_revision, 7);

        handle.hydrate(Value::Nil, 8);
        lua.load(r#"oblisk.probe:invoke("set_volume", 0.6)"#).exec().unwrap();
        assert_eq!(queued_command(&mut rx).unwrap().params.expected_revision, 8);
    }

    #[test]
    fn an_action_with_no_arguments_sends_an_empty_array_not_a_missing_field() {
        // A null would not deserialize into `Vec<serde_json::Value>`.
        let (lua, _handle, mut rx) = lua_with_capability(0);

        lua.load(r#"oblisk.probe:invoke("lock")"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().params.arguments, Vec::<serde_json::Value>::new());
    }

    #[test]
    fn each_invoke_gets_a_distinct_json_rpc_id() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        lua.load(r#"oblisk.probe:invoke("a"); oblisk.probe:invoke("b")"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().id, 0);
        assert_eq!(queued_command(&mut rx).unwrap().id, 1);
    }

    #[test]
    fn an_unmarshallable_argument_is_a_config_error_naming_its_slot_and_queues_nothing() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        let err = lua.load(r#"oblisk.probe:invoke("connect", "ssid", function() end)"#).exec().unwrap_err();

        assert!(err.to_string().contains("argument 2"), "the error must name the offending slot: {err}");
        assert!(rx.try_recv().is_err(), "a refused argument must not queue a half-built command");
    }

    #[test]
    fn on_change_runs_after_a_push_with_the_new_and_the_replaced_payload() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load(
            r#"
            seen = {}
            oblisk.probe:on_change(function(current, previous)
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
            oblisk.probe:on_change(function() error("first handler broke") end)
            oblisk.probe:on_change(function() ran = true end)
        "#,
        )
        .exec()
        .unwrap();

        let previous = handle.hydrate(Value::Integer(5), 1);
        handle.notify_change(&lua, previous);

        assert!(lua.globals().get::<bool>("ran").unwrap());
        let read: i64 = lua.load("return oblisk.probe:get()").eval().unwrap();
        assert_eq!(read, 5);
    }

    #[test]
    fn clear_handlers_forgets_what_the_last_evaluation_registered() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load("count = 0; oblisk.probe:on_change(function() count = count + 1 end)").exec().unwrap();
        handle.clear_handlers();
        let previous = handle.hydrate(Value::Integer(1), 1);
        handle.notify_change(&lua, previous);
        assert_eq!(lua.globals().get::<i64>("count").unwrap(), 0);
    }

    #[test]
    fn get_reads_the_same_live_value_a_bare_capability_global_would() {
        let (lua, handle, _rx) = lua_with_capability(0);

        let before: bool = lua.load("return oblisk.probe:get() == nil").eval().unwrap();
        assert!(before, "a capability reads nil until its first snapshot (ADR-0037)");

        let pushed = lua.create_table().unwrap();
        pushed.set("attempts", 2).unwrap();
        handle.hydrate(Value::Table(pushed), 1);

        let attempts: i64 = lua.load("return oblisk.probe:get().attempts").eval().unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    fn map_returns_a_signal_that_tracks_later_pushes() {
        // The lock screen's failure text is a `map` over this handle: a `map` frozen at
        // registration would paint the first snapshot forever.
        let (lua, handle, _rx) = lua_with_capability(0);
        lua.load(r#"mapped = oblisk.probe:map(function(s) return (s and s.error) or "" end)"#).exec().unwrap();

        let first: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(first, "");

        let pushed = lua.create_table().unwrap();
        pushed.set("error", "authentication failed").unwrap();
        handle.hydrate(Value::Table(pushed), 2);

        let second: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(second, "authentication failed");
    }
}
