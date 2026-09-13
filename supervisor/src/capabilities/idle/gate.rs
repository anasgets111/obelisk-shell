//! The `obelisk.idle` inhibit gate (ADR-0139): how a held logind inhibitor affects threshold events.
//!
//! Obelisk is the idle daemon here. With `IdleAction=ignore` in `logind.conf`, this shell alone acts
//! on idleness, so it must honor inhibitors rather than logind.
//!
//! ADR-0032 gave `obelisk.idle` `inhibit`/`release_inhibit` writes but no read half. A config could
//! ask logind to prevent idle and still receive `on_idle`; external `systemd-inhibit
//! --what=idle mpv film.mkv` had the same problem.
//!
//! The gate closes both. `Manager.BlockInhibited` is a change-notified, colon-separated list, so
//! one property watch answers whether any holder, including this shell, blocks idle. While it
//! names `idle`, threshold events stop and `idle:inhibit(reason)` has its expected effect.
//!
//! Wayland surface inhibitors need nothing here: ADR-0032 uses `get_idle_notification` rather than
//! `get_input_idle_notification`, and the compositor already withholds `idled` for them.

use std::collections::HashSet;

/// `Manager.BlockInhibited`'s colon-separated `what` entries, e.g. `"handle-power-key"` or
/// `"idle:sleep"`. Match `"idle"` as a whole entry; substring matching would misread
/// `"handle-lid-switch"`.
pub(crate) fn blocks_idle(block_inhibited: &str) -> bool {
    block_inhibited.split(':').any(|what| what == "idle")
}

/// Tracks which thresholds were told the session went idle, so a newly-held inhibitor can take
/// that back rather than leaving a config's `on_idle` unanswered.
#[derive(Debug, Default)]
pub(crate) struct IdleGate {
    blocked: bool,
    /// Every `(generation_id, threshold_sec)` with an `Idled` and no `Resumed` since, blocked or not.
    idled: HashSet<(u32, u64)>,
}

impl IdleGate {
    /// One raw threshold event, recorded even under a block so release knows what is still idle.
    /// `None` drops it.
    pub(crate) fn observe(&mut self, event: shared::IdleEvent) -> Option<shared::IdleEvent> {
        let key = (event.generation_id, event.threshold_sec);
        match event.state {
            shared::IdleState::Idled => self.idled.insert(key),
            shared::IdleState::Resumed => self.idled.remove(&key),
        };
        (!self.blocked).then_some(event)
    }

    /// A change in logind's idle-block answer; `None` if unchanged. A block takes back every idle
    /// threshold; release announces those still idle, since a notification never resends `idled`.
    ///
    /// Starts unblocked: an unblocked first observation is `None` and logs nothing; an inhibitor
    /// already held at startup is a real change and logs.
    pub(crate) fn set_blocked(&mut self, blocked: bool) -> Option<Vec<shared::IdleEvent>> {
        if blocked == self.blocked {
            return None;
        }
        self.blocked = blocked;
        let state = if blocked { shared::IdleState::Resumed } else { shared::IdleState::Idled };
        let mut owed: Vec<shared::IdleEvent> = self
            .idled
            .iter()
            .map(|&(generation_id, threshold_sec)| shared::IdleEvent { generation_id, threshold_sec, state })
            .collect();
        // HashSet order is arbitrary; sort before Lua callbacks for repeatable undims.
        owed.sort_by_key(|event| (event.generation_id, event.threshold_sec));
        Some(owed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{IdleEvent, IdleState};

    fn event(generation_id: u32, threshold_sec: u64, state: IdleState) -> IdleEvent {
        IdleEvent { generation_id, threshold_sec, state }
    }

    #[test]
    fn block_inhibited_names_idle_only_as_a_whole_entry() {
        assert!(blocks_idle("idle"));
        assert!(blocks_idle("idle:sleep"));
        assert!(blocks_idle("shutdown:idle:handle-power-key"));
        assert!(!blocks_idle(""));
        assert!(!blocks_idle("handle-power-key"));
        assert!(!blocks_idle("shutdown:sleep"));
    }

    /// Startup with no inhibitor: the gate already agrees, so do not announce a nonexistent
    /// release.
    #[test]
    fn observing_an_unblocked_system_at_startup_is_not_a_change() {
        assert_eq!(IdleGate::default().set_blocked(false), None);
    }

    #[test]
    fn an_unblocked_gate_forwards_everything_untouched() {
        let mut gate = IdleGate::default();
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), Some(event(1, 30, IdleState::Idled)));
        assert_eq!(gate.observe(event(1, 30, IdleState::Resumed)), Some(event(1, 30, IdleState::Resumed)));
    }

    #[test]
    fn a_blocked_gate_forwards_nothing() {
        let mut gate = IdleGate::default();
        gate.set_blocked(true);
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), None);
        assert_eq!(gate.observe(event(1, 30, IdleState::Resumed)), None);
    }

    /// A config dimmed at 30s must get its undim when a film takes an inhibitor, not wait for
    /// input.
    #[test]
    fn an_arriving_inhibitor_takes_back_every_idle_it_had_announced() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 300, IdleState::Idled));

        assert_eq!(
            gate.set_blocked(true),
            Some(vec![event(1, 30, IdleState::Resumed), event(1, 300, IdleState::Resumed)])
        );
    }

    #[test]
    fn a_threshold_that_already_resumed_is_not_resumed_again() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 30, IdleState::Resumed));

        assert_eq!(gate.set_blocked(true), Some(Vec::new()));
    }

    #[test]
    fn an_inhibitor_arriving_while_nothing_was_idle_owes_nothing() {
        let mut gate = IdleGate::default();
        assert_eq!(gate.set_blocked(true), Some(Vec::new()));
    }

    /// `BlockInhibited` changes for unrelated inhibitors too; the same idle answer must not
    /// re-announce a resume.
    #[test]
    fn repeating_the_same_block_state_owes_nothing() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        assert_eq!(gate.set_blocked(true).map(|owed| owed.len()), Some(1));
        assert_eq!(gate.set_blocked(true), None, "the same answer twice is not a change");
    }

    #[test]
    fn releasing_an_inhibitor_announces_the_thresholds_still_idle() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 60, IdleState::Idled));
        gate.set_blocked(true);
        gate.observe(event(1, 60, IdleState::Resumed));
        gate.observe(event(1, 300, IdleState::Idled));

        assert_eq!(
            gate.set_blocked(false),
            Some(vec![event(1, 30, IdleState::Idled), event(1, 300, IdleState::Idled)])
        );
    }
}
