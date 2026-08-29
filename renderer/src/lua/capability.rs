//! The `oblisk` namespace's capability objects: one capability's read signal and § 3.2's write
//! path on the same handle (build-steps.md Phase 25 item 1, docs/adr/0052 decision 1).
//!
//! Phase 25 exists because Lua could not write at all: `Signal` exposes `get`/`map`/`set` and
//! § 3.2's roughly thirty write commands had no implementation outside `process.run`'s hand-built
//! envelope. Item 1 of that phase comes forward here, because ADR-0052 decision 1 makes
//! `oblisk.lock` an ordinary capability and a capability with no caller is dead code. It comes
//! forward *whole*: [`CommandSender::send`] builds an envelope from `{capability, action,
//! arguments}` and knows nothing about locking, so the other twenty-nine commands land on it too
//! rather than each growing a bespoke binding Phase 25 would then have to delete.
//!
//! `renderer/src/lua/process.rs` is the working template, not a special case, and this is the
//! same shape with the capability name lifted out of it: a Lua call on the Wayland dispatch
//! thread has no socket in scope, so it queues a `RendererFrame::Command` onto the one outbound
//! channel the socket thread's `pump` drains (docs/adr/0039). `Rc`, not `Arc`, for the same
//! reason that module states: everything here is confined to the thread that owns the Lua VM.
//!
//! **Why one userdata rather than a signal beside a writer.** § 3.2 writes every command as a
//! method on the capability itself (`audio:set_volume(v)`), and ADR-0052 decision 4 makes a lock
//! screen read `{ active, authenticating, attempts, error }` off the same name it locks through.
//! Splitting the two would hand a config author two objects for one capability and force the
//! engine to keep their names in step; [`Capability`] delegates `get`/`map` to the wrapped
//! [`Signal`] instead, so `oblisk.lock:get().attempts` and `oblisk.lock:invoke("lock")` are the
//! same object and read like § 2's `oblisk.<name>` throughout.
//!
//! ponytail: the write method is spelled `capability:invoke("action", ...)` rather than § 3.2's
//! `capability:action(...)`. § 7.1 wants the engine to "intercept all method invocations on
//! exported singletons", which is an `__index` metamethod handing back a closure bound to
//! whatever name was looked up -- the upgrade path, and it is deliberately not taken yet. A
//! generated closure cannot tell `cap.lock()` from `cap:lock()` (the second passes the userdata
//! as argument one, and ADR-0052 decision 1's own example uses the first spelling while § 3.2
//! uses the second), so the sugar has to settle that ambiguity before it is worth the indirection
//! of turning every typo'd field read into a callable.

use std::cell::Cell;
use std::rc::Rc;

use mlua::{Function, LuaSerdeExt, MultiValue, UserData, UserDataMethods};
use shared::{CommandEnvelope, CommandParams, RendererFrame};
use tokio::sync::mpsc::UnboundedSender;

use crate::lua::signal::Signal;

/// Builds § 7.2's generation-guarded envelope for any `{capability, action, arguments}` and
/// queues it for the socket thread. One per generation, cloned into every [`Capability`] the
/// `oblisk` table carries.
#[derive(Clone)]
pub struct CommandSender {
    generation_id: u32,
    /// JSON-RPC's request id (§ 7.2's `"id": 105`). Shared across every [`Capability`] built from
    /// one sender, so two capabilities cannot hand out the same id for two different writes.
    /// `Rc<Cell<_>>` for this module's single-threaded confinement; nothing correlates a reply to
    /// it yet, because a write command has no reply frame, which is exactly why it must not be a
    /// hardcoded constant that would make two commands indistinguishable the day one does.
    next_id: Rc<Cell<u64>>,
    outbound_tx: UnboundedSender<RendererFrame>,
}

impl CommandSender {
    /// `generation_id` is this Renderer's own generation id (`socket::generation_id_from_env`),
    /// the same value `ProcessRegistry` stamps and the field § 7.3's guard rule drops a packet
    /// on. Taken from the caller rather than re-read from the environment here, so there is one
    /// source for it in the process rather than two that could disagree.
    pub fn new(generation_id: u32, outbound_tx: UnboundedSender<RendererFrame>) -> Self {
        CommandSender { generation_id, next_id: Rc::new(Cell::new(0)), outbound_tx }
    }

    /// The one outbound frame channel this sender writes to, handed back so `RendererClient` can
    /// keep queueing its own non-command frames (`ReevaluateReport`, `RequestReload`) onto the
    /// exact channel a command goes out on rather than being passed a second clone beside it.
    pub fn frames(&self) -> UnboundedSender<RendererFrame> {
        self.outbound_tx.clone()
    }

    /// § 7.2's envelope, queued rather than written: see the module doc comment.
    ///
    /// `expected_revision` is `0`, matching `process.rs`'s hardcoded value, and it is correct for
    /// the same reason ADR-0052 decision 1 gives for `lock`: a command that is not a
    /// read-modify-write has no state for § 7.3's staleness guard to be measured against. It is
    /// **not** correct for a capability whose command reacts to a snapshot it just read --
    /// `audio:set_app_volume` against a stream list, say -- and making it real for those is Phase
    /// 25 item 2, which stores each capability's `StateSnapshot.revision` (dropped today by
    /// `socket::RendererClient::apply_state_snapshot`) alongside its `LiveSignalHandle` and stamps
    /// that here instead.
    fn send(&self, capability: &str, action: &str, arguments: Vec<serde_json::Value>) {
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
                expected_revision: 0,
            },
            id,
        };
        if self.outbound_tx.send(RendererFrame::Command(envelope)).is_err() {
            eprintln!("oblisk.{capability}:invoke(\"{action}\"): failed to queue the command, the control-socket writer is gone");
        }
    }
}

/// One member of the `oblisk` table: the capability's live state signal plus its write path.
/// `name` is the `shared::CAPABILITIES` roster name, which is both the Lua field it is registered
/// under and the `capability` field of every envelope it sends -- one string, so a config that
/// reads `oblisk.lock` cannot write to something else.
pub struct Capability {
    name: String,
    signal: Signal,
    commands: CommandSender,
}

impl Capability {
    pub fn new(name: &str, signal: Signal, commands: CommandSender) -> Self {
        Capability { name: name.to_string(), signal, commands }
    }
}

impl UserData for Capability {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `get`/`map` delegate rather than reimplement, so `oblisk.lock` reads exactly like the
        // ten bare capability globals beside it (ADR-0037's uniform nil-until-hydrated contract
        // included: the wrapped signal holds `nil` until the Supervisor's first push).
        methods.add_method("get", |lua, this, ()| this.signal.get_value(lua));
        methods.add_method("map", |_, this, f: Function| Ok(this.signal.mapped(f)));
        // No `set`: ADR-0044 decision 5 keeps capability state read-only to Lua, and the wrapped
        // `Signal` is a `Live` kind, so exposing it here would only re-raise that same refusal
        // one call deeper. A config changes a capability by commanding it, which is `invoke`.
        methods.add_method("invoke", |lua, this, (action, args): (String, MultiValue)| {
            let mut arguments = Vec::with_capacity(args.len());
            for (index, value) in args.into_iter().enumerate() {
                // A Lua function, thread, or userdata in an argument slot fails here rather than
                // being dropped from the array, because a command silently missing its third
                // argument is a far worse thing to debug from the Supervisor's side than a
                // config error naming the slot.
                let json = lua.from_value::<serde_json::Value>(value).map_err(|err| {
                    mlua::Error::runtime(format!(
                        "oblisk.{}:invoke(\"{action}\") could not marshal argument {}: {err}",
                        this.name,
                        index + 1
                    ))
                })?;
                arguments.push(json);
            }
            this.commands.send(&this.name, &action, arguments);
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use mlua::{Lua, Value};
    use tokio::sync::mpsc;

    use super::*;
    use crate::lua::signal::{DirtyFlag, LiveSignalHandle};

    /// A VM with `oblisk.probe` on it: the roster name is irrelevant to everything in this
    /// module (that is the point of a generic write path), so the tests use a name no capability
    /// owns rather than implying `lock` is special here.
    fn lua_with_capability(generation_id: u32) -> (Lua, LiveSignalHandle, mpsc::UnboundedReceiver<RendererFrame>) {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let (signal, handle) = Signal::new_live(Value::Nil, DirtyFlag::new());
        let table = lua.create_table().unwrap();
        table.set("probe", Capability::new("probe", signal, CommandSender::new(generation_id, tx))).unwrap();
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
        // Phase 25 item 2 is what makes this a real revision; until then a write is not a
        // read-modify-write and there is nothing for § 7.3's guard to compare (docs/adr/0052).
        assert_eq!(envelope.params.expected_revision, 0);
    }

    #[test]
    fn an_action_with_no_arguments_sends_an_empty_array_not_a_missing_field() {
        // § 3.2 writes `network:scan()` as `arguments: []`, and the Supervisor's `dispatch`
        // indexes that array -- a null would not deserialize into `Vec<serde_json::Value>`.
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
        handle.set(Value::Table(pushed));

        let attempts: i64 = lua.load("return oblisk.probe:get().attempts").eval().unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    fn map_returns_a_signal_that_tracks_later_pushes() {
        // The lock screen's failure text is a `map` over this handle, so a `map` that froze its
        // input at registration time would paint the first snapshot forever.
        let (lua, handle, _rx) = lua_with_capability(0);
        lua.load(r#"mapped = oblisk.probe:map(function(s) return (s and s.error) or "" end)"#).exec().unwrap();

        let first: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(first, "");

        let pushed = lua.create_table().unwrap();
        pushed.set("error", "authentication failed").unwrap();
        handle.set(Value::Table(pushed));

        let second: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(second, "authentication failed");
    }
}
