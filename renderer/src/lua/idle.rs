//! `oblisk.idle`: idle-notify thresholds and the logind inhibit pair (ADR-0032,
//! docs/oblisk-supervisor-services-dbus.md § 7).
//!
//! **Not a capability**, and deliberately outside `shared::Capability::ALL` for the same reason
//! `screens` and `rescue` are outside it, from the other direction: every roster name is a signal
//! the Supervisor pushes a `StateSnapshot` for, and idle pushes none. `ext_idle_notifier_v1`
//! reports that a threshold a config asked for was crossed; there is no idle *state* to read. A
//! capability member here would hand a config a signal that reads `nil` forever and a `:map` that
//! never fires. So `oblisk.idle` is methods and nothing else.
//!
//! The Supervisor creates one Wayland listener per distinct duration and fans its events out
//! (`hardware::idle::register_threshold_entry`), so `shared::IdleEvent` names the threshold rather
//! than the registration. Two `register_threshold(300, ...)` calls are one listener there and two
//! entries here, and both run on the same event.
//!
//! There is no unregister. `Loader::evaluate_file` drops every threshold before it re-runs
//! `shell.lua`, because the callbacks belong to the tree being replaced -- without that an
//! in-place reload (ADR-0047) stacks a second copy of every callback on the same VM, and the
//! tenth reload of a config that dims the screen dims it ten times. The Supervisor keeps its
//! listener; re-registering the same duration is a no-op there.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, UserData, UserDataMethods};

use crate::lua::capability::CommandSender;

/// What one `register_threshold` call left behind.
struct Threshold {
    on_idle: Function,
    on_resume: Function,
}

/// Holds every registered threshold's callbacks and queues `"idle"` commands. `Rc<RefCell<_>>`
/// rather than `Arc<Mutex<_>>`: confined to the Wayland dispatch thread with the Lua VM, the same
/// confinement `lua::process` documents.
#[derive(Clone)]
pub struct IdleRegistry(Rc<RefCell<Inner>>);

struct Inner {
    /// Keyed by the threshold in seconds, which is the only thing `shared::IdleEvent` carries to
    /// match on.
    thresholds: HashMap<u64, Vec<Threshold>>,
    commands: CommandSender,
}

impl IdleRegistry {
    pub fn new(commands: CommandSender) -> Self {
        IdleRegistry(Rc::new(RefCell::new(Inner { thresholds: HashMap::new(), commands })))
    }

    /// `idle:register_threshold(sec, on_idle, on_resume)` (§ 7.1). Sends the start ahead of the
    /// command, because `idle` is off the roster and so has no `oblisk` member for
    /// `lua::namespace`'s `__index` to catch (ADR-0070 decision 1). Both are deduplicated by
    /// `CommandSender`, so the second call sends only the command.
    ///
    /// The registration is local and
    /// the command tells the Supervisor to create the listener; nothing waits for a reply, so a
    /// config registering a threshold gets no acknowledgement and none is needed -- an inert
    /// notify half (no `ext_idle_notifier_v1`, ADR-0032) is a listener that never fires, which
    /// reads exactly like a user who never went idle.
    fn register_threshold(&self, sec: u64, on_idle: Function, on_resume: Function) {
        let mut inner = self.0.borrow_mut();
        inner.thresholds.entry(sec).or_default().push(Threshold { on_idle, on_resume });
        inner.commands.start_capability("idle");
        inner.commands.send("idle", "register", vec![serde_json::json!(sec)], 0);
    }

    /// `idle:inhibit(reason)` (ADR-0032). The count is the Supervisor's, per generation, so two
    /// callers here hold two references to one logind fd and neither release kills the other.
    fn inhibit(&self, reason: String) {
        let inner = self.0.borrow();
        inner.commands.start_capability("idle");
        inner.commands.send("idle", "inhibit", vec![serde_json::json!(reason)], 0);
    }

    /// `idle:release_inhibit()`. Releases one hold, not every hold: see [`Self::inhibit`].
    fn release_inhibit(&self) {
        let inner = self.0.borrow();
        inner.commands.start_capability("idle");
        inner.commands.send("idle", "release_inhibit", Vec::new(), 0);
    }

    /// `SupervisorFrame::IdleEvent` dispatch: runs every callback registered for
    /// `threshold_sec`. An event for a threshold nothing registered is silently ignored, which is
    /// what a reload leaves behind -- the Supervisor keeps its listener alive for the generation.
    ///
    /// The callbacks are cloned out before any of them runs, so one that calls
    /// `register_threshold` (or raises) cannot invalidate the borrow the loop is walking.
    pub fn dispatch_event(&self, threshold_sec: u64, state: shared::IdleState) {
        let callbacks: Vec<Function> = {
            let inner = self.0.borrow();
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

    /// Drops every registration, run before each evaluation. See the module doc comment.
    pub fn forget_thresholds(&self) {
        self.0.borrow_mut().thresholds.clear();
    }

    /// The `oblisk.idle` member itself.
    pub fn member(&self) -> IdleMember {
        IdleMember(self.clone())
    }
}

/// The userdata `oblisk.idle` is. Userdata rather than a table of closures so `oblisk.idle:x()`
/// works the way every capability's `:invoke` does, without each method having to accept and
/// ignore the table Lua passes as argument one.
pub struct IdleMember(IdleRegistry);

impl UserData for IdleMember {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
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
        let registry = IdleRegistry::new(CommandSender::new(generation_id, tx));
        lua.globals().set("idle", registry.member()).unwrap();
        (lua, registry, rx)
    }

    /// The next queued command, stepping over the `idle` start that every method sends ahead of
    /// its first command (ADR-0070). `every_idle_method_starts_the_capability_first` is what
    /// asserts on those.
    fn queued_command(rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Option<CommandEnvelope> {
        loop {
            match rx.try_recv().ok()? {
                RendererFrame::Command(envelope) => return Some(envelope),
                RendererFrame::StartCapability { .. } => continue,
                other => panic!("idle commands must be queued as RendererFrame::Command, got {other:?}"),
            }
        }
    }

    /// ADR-0070 decision 1: `idle` is off the roster, so nothing indexes `oblisk` to reach
    /// it and the methods have to send the start themselves. Without this the Supervisor never
    /// builds `IdleController` and a `register` command lands on nothing.
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

    /// One start for the whole generation, not one per call.
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

    /// The Supervisor creates one listener per duration and fans out, so both registrations run on
    /// the one event rather than the second replacing the first.
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

    /// A callback raising must not stop the ones registered after it: they are separate
    /// registrations that happen to share a duration.
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

    /// A callback registering another threshold mid-dispatch must not panic on the borrow the
    /// dispatch loop is walking.
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
