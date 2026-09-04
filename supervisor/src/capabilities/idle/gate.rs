//! The inhibit gate for `oblisk.idle` (ADR-0139): what a held logind idle inhibitor does to the
//! threshold events this Supervisor forwards.
//!
//! Oblisk is the idle daemon on this machine. `IdleAction=ignore` in `logind.conf` is the usual
//! configuration for a session that runs its own, so nothing but this shell acts on idleness, and
//! that makes honouring an inhibitor this shell's job rather than logind's.
//!
//! ADR-0032 gave `oblisk.idle` a write half (`inhibit`/`release_inhibit`) and no read half, which
//! left the capability holding an inhibitor it then ignored: a config could ask logind not to let
//! the session idle and still get the `on_idle` that dims the screen. `systemd-inhibit
//! --what=idle mpv film.mkv` from any other application had the same problem from the outside.
//!
//! The gate closes both at once. `Manager.BlockInhibited` is a colon-separated list of what is
//! currently blocked, with change notification, so one property watch answers "is anything holding
//! an idle inhibitor" for every holder including this shell's own. While it names `idle`, no
//! threshold event is forwarded, and `idle:inhibit(reason)` becomes the mechanism it always read
//! like rather than a flag set and then ignored.
//!
//! Wayland surface inhibitors are a separate mechanism and need nothing here: ADR-0032 picked
//! `get_idle_notification` over `get_input_idle_notification`, and the compositor already withholds
//! `idled` for those. This is the logind half of the same idea.

use std::collections::HashSet;

/// `Manager.BlockInhibited`'s `what` field, split. A colon-separated list, e.g.
/// `"handle-power-key"` or `"idle:sleep"`, and `"idle"` has to match a whole entry: a substring
/// test would read `"handle-lid-switch"` as an idle block on a machine with a different set.
pub(crate) fn blocks_idle(block_inhibited: &str) -> bool {
    block_inhibited.split(':').any(|what| what == "idle")
}

/// Tracks which thresholds were told the session went idle, so a newly-held inhibitor can take
/// that back rather than leaving a config's `on_idle` unanswered.
#[derive(Debug, Default)]
pub(crate) struct IdleGate {
    blocked: bool,
    /// Every `(generation_id, threshold_sec)` an `Idled` was forwarded for and no `Resumed` has
    /// been forwarded for yet.
    idled: HashSet<(u32, u64)>,
}

impl IdleGate {
    /// One raw threshold event. `None` drops it.
    ///
    /// A `Resumed` is dropped under a block along with the `Idled`s: if the pair was open when the
    /// inhibitor arrived, [`Self::set_blocked`] already closed it, and if it was not, this
    /// `Resumed` answers nothing.
    pub(crate) fn observe(&mut self, event: shared::IdleEvent) -> Option<shared::IdleEvent> {
        if self.blocked {
            return None;
        }
        let key = (event.generation_id, event.threshold_sec);
        match event.state {
            shared::IdleState::Idled => {
                self.idled.insert(key);
            }
            shared::IdleState::Resumed => {
                self.idled.remove(&key);
            }
        }
        Some(event)
    }

    /// A change in what logind reports as blocked. `None` when this is the same answer as last
    /// time, which is most calls: `BlockInhibited` changes whenever *anything* is inhibited, and
    /// almost none of it is idle. `Some` carries the `Resumed` events now owed, so a config that
    /// dimmed the screen at 30 seconds gets its undim when a film starts -- which is the whole
    /// reason the `idled` set is tracked rather than a bare flag.
    ///
    /// The gate starts unblocked, so the first observation of an unblocked system is `None` and
    /// logs nothing. An inhibitor already held at startup is a real change and does log.
    ///
    /// Nothing is replayed on release. If the seat is still idle when the inhibitor goes away, the
    /// compositor has already sent its `idled` and will not send it again, so the screen stays
    /// awake until the next idle period.
    ///
    /// ponytail: that is the safe direction to fail and it is still wrong. The fix is asking the
    /// compositor for the seat's current idleness on release, which `ext-idle-notifier-v1` has no
    /// call for -- it would mean tearing down every notification and re-creating it, and the
    /// re-created ones fire from zero rather than from when the user actually stopped.
    pub(crate) fn set_blocked(&mut self, blocked: bool) -> Option<Vec<shared::IdleEvent>> {
        if blocked == self.blocked {
            return None;
        }
        self.blocked = blocked;
        if !blocked {
            return Some(Vec::new());
        }
        let mut owed: Vec<shared::IdleEvent> = self
            .idled
            .drain()
            .map(|(generation_id, threshold_sec)| shared::IdleEvent {
                generation_id,
                threshold_sec,
                state: shared::IdleState::Resumed,
            })
            .collect();
        // A `HashSet`'s drain order is arbitrary and these reach Lua callbacks; sorted so a
        // config's undim runs in the same order twice.
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

    /// The startup case: nothing is inhibited, the gate already thinks so, and the watcher must
    /// not announce a release that never happened.
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

    /// The point of tracking the set: a config that dimmed at 30s must get its undim when a film
    /// takes an inhibitor, not be left dimmed until the user touches the keyboard.
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

    /// `BlockInhibited` changes whenever anything at all is inhibited, most of it unrelated to
    /// idle, so the same answer twice must not re-announce a resume.
    #[test]
    fn repeating_the_same_block_state_owes_nothing() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        assert_eq!(gate.set_blocked(true).map(|owed| owed.len()), Some(1));
        assert_eq!(gate.set_blocked(true), None, "the same answer twice is not a change");
    }

    #[test]
    fn releasing_an_inhibitor_replays_nothing_and_reopens_the_gate() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.set_blocked(true);

        assert_eq!(gate.set_blocked(false), Some(Vec::new()), "the seat's current idleness cannot be re-read");
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), Some(event(1, 30, IdleState::Idled)));
    }
}
