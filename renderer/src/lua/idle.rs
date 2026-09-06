//! `oblisk.idle`: idle-notify thresholds and the logind inhibit pair (ADR-0032,
//! docs/oblisk-supervisor-services-dbus.md § 7).
//!
//! **A roster capability with three extra methods.** Until ADR-0141 it exposed only methods: a
//! threshold crossing was treated as an event with no idle state. That missed whether anything
//! holds the session awake and which application does it, state the Supervisor already knows
//! (ADR-0139's gate). A config could otherwise draw "nothing is holding this awake" while every
//! threshold event was inhibited, with no way to represent the truth.
//!
//! `oblisk.idle` is now a `Capability` (`get`, `map`, `on_change`, `invoke`, hydrated by
//! `StateSnapshot`) wrapped in userdata that adds the three callbacks that cannot cross the wire as
//! `:invoke`. The wrapper sits directly on `oblisk`, outside `__index`, so every method sends
//! `start_capability` by hand; its read never passes through the index.
//!
//! The Supervisor creates one Wayland listener per duration and fans events out
//! (`hardware::idle::register_threshold_entry`). `shared::IdleEvent` names the duration, not the
//! registration: two `register_threshold(300, ...)` calls make one listener and two local entries,
//! both run by the same event.
//!
//! There is no unregister. Before each `shell.lua` evaluation, `Loader::evaluate_file` drops local
//! thresholds; otherwise an in-place reload (ADR-0047) stacks callbacks on the same VM, so the
//! tenth reload of a screen-dimming config dims ten times. The Supervisor keeps its listener, and
//! re-registering the duration is a no-op there.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, UserData, UserDataMethods};

use crate::lua::capability::{Capability, CommandSender};

/// Callbacks left by one `register_threshold` call.
struct Threshold {
    on_idle: Function,
    on_resume: Function,
}

/// Threshold callbacks and queued `"idle"` commands. `Rc<RefCell<_>>` is correct because this and
/// the Lua VM stay on the Wayland dispatch thread, as `lua::process` documents.
#[derive(Clone)]
pub struct IdleRegistry {
    inner: Rc<RefCell<Inner>>,
    /// Wrapped `oblisk.idle` read half, hydrated by `StateSnapshot` like other capabilities.
    state: Capability,
}

struct Inner {
    /// Keyed by seconds, the only field `shared::IdleEvent` carries for matching.
    thresholds: HashMap<u64, Vec<Threshold>>,
    commands: CommandSender,
}

impl IdleRegistry {
    pub fn new(commands: CommandSender, state: Capability) -> Self {
        IdleRegistry { inner: Rc::new(RefCell::new(Inner { thresholds: HashMap::new(), commands })), state }
    }

    /// Read half for the wrapper's `get`/`map` and `signal::from_userdata`.
    pub fn state(&self) -> &Capability {
        &self.state
    }

    /// `idle:register_threshold(sec, on_idle, on_resume)` (§ 7.1). Queue the start first: `idle`
    /// is off the roster, so `lua::namespace`'s `__index` cannot announce it (ADR-0070 decision 1).
    /// `CommandSender` deduplicates both starts, so the second call sends only its command.
    ///
    /// Registration is local; the command asks the Supervisor to create the listener, with no
    /// acknowledgement. Without `ext_idle_notifier_v1` (ADR-0032), the inert notify half never
    /// fires, which is indistinguishable from a user who never went idle.
    fn register_threshold(&self, sec: u64, on_idle: Function, on_resume: Function) {
        let mut inner = self.inner.borrow_mut();
        inner.thresholds.entry(sec).or_default().push(Threshold { on_idle, on_resume });
        inner.commands.start_capability("idle");
        inner.commands.send("idle", "register", vec![serde_json::json!(sec)], 0);
    }

    /// `idle:inhibit(reason)` (ADR-0032). The Supervisor counts holds per generation, so two
    /// callers hold two references to one logind fd and either release leaves the other alive.
    fn inhibit(&self, reason: String) {
        let inner = self.inner.borrow();
        inner.commands.start_capability("idle");
        inner.commands.send("idle", "inhibit", vec![serde_json::json!(reason)], 0);
    }

    /// `idle:release_inhibit()`, releasing one hold rather than every hold.
    fn release_inhibit(&self) {
        let inner = self.inner.borrow();
        inner.commands.start_capability("idle");
        inner.commands.send("idle", "release_inhibit", Vec::new(), 0);
    }

    /// Dispatches `SupervisorFrame::IdleEvent` to every callback for `threshold_sec`. An
    /// unregistered threshold is ignored, as after reload the Supervisor keeps its listener.
    /// Clone callbacks first so one can register another threshold, or raise, without invalidating
    /// the borrow being walked.
    pub fn dispatch_event(&self, threshold_sec: u64, state: shared::IdleState) {
        let callbacks: Vec<Function> = {
            let inner = self.inner.borrow();
            let Some(entries) = inner.thresholds.get(&threshold_sec) else { return };
            entries
                .iter()
                .map(|entry| match state {
                    shared::IdleState::Idled => entry.on_idle.clone(),
                    shared::IdleState::Resumed => entry.on_resume.clone(),
                })
                .collect()
        };
        for callback in callbacks {
            if let Err(err) = callback.call::<()>(()) {
                let which = match state {
                    shared::IdleState::Idled => "on_idle",
                    shared::IdleState::Resumed => "on_resume",
                };
                eprintln!("oblisk.idle:register_threshold({threshold_sec}): {which} raised an error: {err}");
            }
        }
    }

    /// Drops local registrations before each evaluation.
    pub fn forget_thresholds(&self) {
        self.inner.borrow_mut().thresholds.clear();
    }

    /// The `oblisk.idle` member.
    pub fn member(&self) -> IdleMember {
        IdleMember(self.clone())
    }
}

/// `oblisk.idle` userdata. Userdata preserves capability-style `oblisk.idle:x()` calls without
/// each closure accepting and ignoring Lua's table argument.
pub struct IdleMember(IdleRegistry);

impl IdleMember {
    /// Wrapped capability read signal for `signal::from_userdata`.
    pub fn signal(&self) -> crate::lua::signal::Signal {
        self.0.state().signal()
    }
}

impl IdleMember {
    /// Announces the read that `oblisk`'s `__index` would have announced. Because this member is
    /// set directly, a config that only `:map`s `oblisk.idle` would otherwise read `nil` forever
    /// from a capability the Supervisor never started.
    fn announce(&self) {
        let state = self.0.state();
        state.commands().start_capability(state.name());
    }
}

impl UserData for IdleMember {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Delegate the read half so `oblisk.idle` reads like `oblisk.privacy` (ADR-0141).
        methods.add_method("get", |lua, this, ()| {
            this.announce();
            this.0.state().signal().get_value(lua)
        });
        methods.add_method("map", |_, this, f: Function| {
            this.announce();
            Ok(this.0.state().signal().mapped(f))
        });
        methods.add_method("on_change", |_, this, f: Function| {
            this.announce();
            this.0.state().add_handler(f);
            Ok(())
        });
        methods.add_method("register_threshold", |_, this, (sec, on_idle, on_resume): (u64, Function, Function)| {
            this.0.register_threshold(sec, on_idle, on_resume);
            Ok(())
        });
        methods.add_method("inhibit", |_, this, reason: String| {
            this.0.inhibit(reason);
            Ok(())
        });
        methods.add_method("release_inhibit", |_, this, ()| {
            this.0.release_inhibit();
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use mlua::Lua;
    use shared::{CommandEnvelope, IdleState, RendererFrame};
    use tokio::sync::mpsc;

    use super::*;

    fn lua_with_idle(generation_id: u32) -> (Lua, IdleRegistry, mpsc::UnboundedReceiver<RendererFrame>) {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let commands = CommandSender::new(generation_id, tx);
        // Tests exercise the method half; drop the `oblisk.idle` roster handle (ADR-0141).
        let (state, _handle) = Capability::new("idle", crate::lua::signal::DirtyFlag::new(), commands.clone());
        let registry = IdleRegistry::new(commands, state);
        lua.globals().set("idle", registry.member()).unwrap();
        (lua, registry, rx)
    }

    /// Next command, skipping the `idle` start every method queues first (ADR-0070).
    fn queued_command(rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Option<CommandEnvelope> {
        loop {
            match rx.try_recv().ok()? {
                RendererFrame::Command(envelope) => return Some(envelope),
                RendererFrame::StartCapability { .. } => continue,
                other => panic!("idle commands must be queued as RendererFrame::Command, got {other:?}"),
            }
        }
    }

    /// ADR-0070 decision 1: `idle` is off the roster, so methods must start it themselves or the
    /// Supervisor never builds `IdleController` and `register` lands on nothing.
    #[test]
    fn every_idle_method_starts_the_capability_first() {
        for call in [
            "idle:register_threshold(60, function() end, function() end)",
            "idle:inhibit(\"video\")",
            "idle:release_inhibit()",
        ] {
            let (lua, _registry, mut rx) = lua_with_idle(0);
            lua.load(call).exec().unwrap();
            let first = rx.try_recv().expect("a method must queue something");
            assert!(
                matches!(&first, RendererFrame::StartCapability { capability } if capability == "idle"),
                "{call} must send the start ahead of its command, got {first:?}"
            );
        }
    }

    /// One start per generation, not per call.
    #[test]
    fn a_second_idle_call_sends_no_second_start() {
        let (lua, _registry, mut rx) = lua_with_idle(0);
        lua.load(r#"idle:inhibit("a"); idle:inhibit("b")"#).exec().unwrap();

        let starts = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|frame| matches!(frame, RendererFrame::StartCapability { .. }))
            .count();
        assert_eq!(starts, 1);
    }

    #[test]
    fn register_threshold_queues_a_register_command_carrying_only_the_duration() {
        let (lua, _registry, mut rx) = lua_with_idle(4);

        lua.load("idle:register_threshold(300, function() end, function() end)").exec().unwrap();

        let envelope = queued_command(&mut rx).expect("a register command must have been queued");
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "idle");
        assert_eq!(envelope.params.action, "register");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!(300)]);
        assert_eq!(
            envelope.params.expected_revision, 0,
            "idle pushes no snapshot, so there is no revision a registration could be reacting to"
        );
    }

    #[test]
    fn inhibit_and_release_queue_their_own_commands() {
        let (lua, _registry, mut rx) = lua_with_idle(1);

        lua.load(r#"idle:inhibit("playing a video"); idle:release_inhibit()"#).exec().unwrap();

        let inhibit = queued_command(&mut rx).unwrap();
        assert_eq!(inhibit.params.action, "inhibit");
        assert_eq!(inhibit.params.arguments, vec![serde_json::json!("playing a video")]);
        let release = queued_command(&mut rx).unwrap();
        assert_eq!(release.params.action, "release_inhibit");
        assert!(release.params.arguments.is_empty());
    }

    #[test]
    fn an_idled_event_runs_on_idle_and_a_resumed_event_runs_on_resume() {
        let (lua, registry, _rx) = lua_with_idle(0);
        lua.load(
            r#"
            fired = {}
            idle:register_threshold(30, function() fired[#fired + 1] = "idled" end,
                                        function() fired[#fired + 1] = "resumed" end)
            "#,
        )
        .exec()
        .unwrap();

        registry.dispatch_event(30, IdleState::Idled);
        registry.dispatch_event(30, IdleState::Resumed);

        let fired: Vec<String> = lua.load("return fired").eval().unwrap();
        assert_eq!(fired, vec!["idled".to_string(), "resumed".to_string()]);
    }

    /// One listener per duration fans out, so both registrations run on one event.
    #[test]
    fn two_registrations_at_the_same_threshold_both_run() {
        let (lua, registry, _rx) = lua_with_idle(0);
        lua.load(
            r#"
            count = 0
            idle:register_threshold(60, function() count = count + 1 end, function() end)
            idle:register_threshold(60, function() count = count + 10 end, function() end)
            "#,
        )
        .exec()
        .unwrap();

        registry.dispatch_event(60, IdleState::Idled);

        assert_eq!(lua.load("return count").eval::<i64>().unwrap(), 11);
    }

    #[test]
    fn an_event_for_an_unregistered_threshold_runs_nothing() {
        let (lua, registry, _rx) = lua_with_idle(0);
        lua.load("ran = false; idle:register_threshold(30, function() ran = true end, function() end)").exec().unwrap();

        registry.dispatch_event(31, IdleState::Idled);

        assert!(!lua.load("return ran").eval::<bool>().unwrap());
    }

    /// A raising callback must not stop later registrations sharing its duration.
    #[test]
    fn a_raising_callback_does_not_stop_the_rest() {
        let (lua, registry, _rx) = lua_with_idle(0);
        lua.load(
            r#"
            ran = false
            idle:register_threshold(5, function() error("boom") end, function() end)
            idle:register_threshold(5, function() ran = true end, function() end)
            "#,
        )
        .exec()
        .unwrap();

        registry.dispatch_event(5, IdleState::Idled);

        assert!(lua.load("return ran").eval::<bool>().unwrap());
    }

    /// Registering another threshold during dispatch must not panic on the loop's borrow.
    #[test]
    fn a_callback_that_registers_another_threshold_does_not_panic() {
        let (lua, registry, _rx) = lua_with_idle(0);
        lua.load(
            r#"
            idle:register_threshold(5, function()
                idle:register_threshold(10, function() end, function() end)
            end, function() end)
            "#,
        )
        .exec()
        .unwrap();

        registry.dispatch_event(5, IdleState::Idled);
    }

    #[test]
    fn forgetting_thresholds_leaves_nothing_to_dispatch_to() {
        let (lua, registry, _rx) = lua_with_idle(0);
        lua.load("ran = false; idle:register_threshold(30, function() ran = true end, function() end)").exec().unwrap();

        registry.forget_thresholds();
        registry.dispatch_event(30, IdleState::Idled);

        assert!(!lua.load("return ran").eval::<bool>().unwrap());
    }
}
