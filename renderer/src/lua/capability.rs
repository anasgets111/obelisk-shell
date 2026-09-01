//! The `oblisk` namespace's capability objects: one capability's read signal, its revision, and
//! § 3.2's write path on the same handle (build-steps.md Phase 25 items 1 and 2, docs/adr/0052
//! decision 1).
//!
//! [`CommandSender::send`] builds an envelope from `{capability, action, arguments}` and knows
//! nothing about locking, so every § 3.2 write command lands on it rather than each growing a
//! bespoke binding. The revision lives beside it in the same object for the same reason:
//! [`Capability`] and [`CapabilityHandle`] are the two ends of one capability, so the number a
//! write is stamped with and the push that moves it never drift into two separate maps.
//!
//! A Lua call on the Wayland dispatch thread has no socket in scope, so it queues a
//! `RendererFrame::Command` onto the one outbound channel the socket thread's `pump` drains
//! (docs/adr/0039). `Rc`, not `Arc`: everything here is confined to that one thread.
//!
//! One userdata rather than a signal beside a writer: § 3.2 writes every command as a method on
//! the capability itself, and ADR-0052 decision 4 reads a lock screen's state off the same name
//! it locks through. [`Capability`] delegates `get`/`map` to the wrapped [`Signal`], so
//! `oblisk.lock:get().attempts` and `oblisk.lock:invoke("lock")` are the same object.
//!
//! ponytail: the write method is spelled `capability:invoke("action", ...)` rather than § 3.2's
//! `capability:action(...)`. § 7.1 wants the engine to "intercept all method invocations on
//! exported singletons", which is an `__index` metamethod handing back a closure bound to
//! whatever name was looked up -- the upgrade path, and it is deliberately not taken yet. A
//! generated closure cannot tell `cap.lock()` from `cap:lock()` (the second passes the userdata
//! as argument one, and ADR-0052 decision 1's own example uses the first spelling while § 3.2
//! uses the second), so the sugar has to settle that ambiguity before it is worth the indirection
//! of turning every typo'd field read into a callable.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use mlua::{Function, LuaSerdeExt, MultiValue, UserData, UserDataMethods, Value};
use shared::{CommandEnvelope, CommandParams, RendererFrame};
use tokio::sync::mpsc::UnboundedSender;

use crate::lua::signal::{DirtyFlag, LiveSignalHandle, Signal};

/// Builds § 7.2's generation-guarded envelope for any `{capability, action, arguments}` and
/// queues it for the socket thread. One per generation, cloned into every [`Capability`] the
/// `oblisk` table carries.
#[derive(Clone)]
pub struct CommandSender {
    generation_id: u32,
    /// JSON-RPC's request id (§ 7.2's `"id": 105`). Shared across every [`Capability`] built from
    /// one sender, so two capabilities cannot hand out the same id for two different writes.
    /// `Rc<Cell<_>>` for this module's single-threaded confinement.
    next_id: Rc<Cell<u64>>,
    /// Every capability this generation has already asked the Supervisor to start, so the second
    /// reader of `oblisk.audio` costs nothing. Shared across every clone of this sender because
    /// `lua::namespace`'s `__index` and `socket`'s `secure_submit` sweep are two callers of the
    /// same question (docs/adr/0070 decisions 1 and 5).
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

    /// Asks the Supervisor to construct `capability`'s controller, once per generation.
    ///
    /// The Supervisor drops a repeat itself (ADR-0070 decision 3), so the local set is not what
    /// makes this correct -- it is what keeps a `map` over `oblisk.audio` that re-resolves on every
    /// layout pass from writing a frame per pass.
    pub(crate) fn start_capability(&self, capability: &str) {
        if !self.started.borrow_mut().insert(capability.to_string()) {
            return;
        }
        let frame = RendererFrame::StartCapability { capability: capability.to_string() };
        if self.outbound_tx.send(frame).is_err() {
            eprintln!("oblisk.{capability}: failed to queue the start request, the control-socket writer is gone");
        }
    }

    /// The one outbound frame channel this sender writes to, handed back so `RendererClient` can
    /// queue its own non-command frames (`ReevaluateReport`, `RequestReload`) onto the same
    /// channel a command goes out on.
    pub fn frames(&self) -> UnboundedSender<RendererFrame> {
        self.outbound_tx.clone()
    }

    /// § 7.2's envelope, queued rather than written: see the module doc comment.
    ///
    /// `expected_revision` is the revision of the last `StateSnapshot` this capability hydrated
    /// from -- § 7.3's staleness half: names which read a write was reacting to. [`CapabilityHandle`]
    /// keeps it current.
    ///
    /// `0` is not a revision any push can produce (`supervisor::snapshot::bump_revision` starts at
    /// `1`), so it means "never hydrated in this Renderer": honest for a write before the first
    /// snapshot, and permanently correct for a capability with no state to be stale about, which
    /// is why `lock` (ADR-0052 decision 1) and `process.rs` both send it forever.
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
/// `name` is the `shared::CAPABILITIES` roster name, both the Lua field it is registered under
/// and the `capability` field of every envelope it sends.
pub struct Capability {
    name: String,
    signal: Signal,
    /// Shared with the [`CapabilityHandle`] built beside this member, so an `invoke` stamps the
    /// revision of the snapshot the config could last have read. See [`CommandSender::send`].
    revision: Rc<Cell<u32>>,
    commands: CommandSender,
}

impl Capability {
    /// Builds one `oblisk.<name>` member and the handle `socket::RendererClient` hydrates it
    /// through. Returned together because the value and the revision must move as one: a `set`
    /// that missed its matching revision bump would stamp the previous read onto a write reacting
    /// to the current one -- the race § 7.3 exists to drop.
    pub fn new(name: &str, dirty: DirtyFlag, commands: CommandSender) -> (Self, CapabilityHandle) {
        // `nil` until the Supervisor's first push (ADR-0037), paired with revision `0`, which no
        // push can produce.
        let (signal, signal_handle) = Signal::new_live(Value::Nil, dirty);
        let revision = Rc::new(Cell::new(0));
        let capability = Capability { name: name.to_string(), signal, revision: Rc::clone(&revision), commands };
        (capability, CapabilityHandle { signal: signal_handle, revision })
    }

    /// The wrapped read signal, for `signal::from_userdata`: lets a config write § 1.2's live
    /// spelling (`content = oblisk.mpris`, `computed({oblisk.audio}, f)`) against a capability,
    /// since the engine resolves the inner signal and never sees the `Capability` wrapper.
    pub fn signal(&self) -> Signal {
        self.signal.clone()
    }
}

/// The Rust-side half of one `oblisk.<name>` member: what a `StateSnapshot` writes into.
///
/// One handle rather than a `LiveSignalHandle` beside a revision cell, because
/// `socket::RendererClient` holds one of these per capability and every caller wants both fields.
#[derive(Clone)]
pub struct CapabilityHandle {
    signal: LiveSignalHandle,
    revision: Rc<Cell<u32>>,
}

impl CapabilityHandle {
    /// Writes one `StateSnapshot` into the Lua-visible signal and records the revision it
    /// arrived with. Revision first: the value write marks the scene dirty (ADR-0044 decision 2),
    /// so it must happen last.
    pub fn hydrate(&self, value: Value, revision: u32) {
        self.revision.set(revision);
        self.signal.set(value);
    }
}

impl UserData for Capability {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `get`/`map` delegate rather than reimplement, so a capability reads exactly like the
        // two bare `Signal` globals beside it in the `oblisk` table (`rescue` and `screens`).
        methods.add_method("get", |lua, this, ()| this.signal.get_value(lua));
        methods.add_method("map", |_, this, f: Function| Ok(this.signal.mapped(f)));
        // No `set`: ADR-0044 decision 5 keeps capability state read-only to Lua. A config changes
        // a capability by commanding it, which is `invoke`.
        methods.add_method("invoke", |lua, this, (action, args): (String, MultiValue)| {
            let mut arguments = Vec::with_capacity(args.len());
            for (index, value) in args.into_iter().enumerate() {
                // Fails here rather than silently dropping the slot: a command missing its third
                // argument is worse to debug from the Supervisor's side than a config error
                // naming the slot.
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
