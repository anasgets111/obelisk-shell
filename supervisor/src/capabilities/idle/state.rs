//! What `oblisk.idle` publishes: whether anything is holding the session awake, and who.
//!
//! ADR-0139 built the gate and told Lua nothing about it, on the grounds that `register_threshold`
//! takes callbacks that cannot cross the wire, so the roster would buy only push machinery. Push
//! machinery turned out to be the whole point: a config drawing "nothing is holding this awake"
//! while the Supervisor was holding every threshold event is a lie on screen, and the config had no
//! way to know better. Observed live with `systemd-inhibit --what=idle --who=mpv`: the countdown
//! stopped and nothing said why. ADR-0141 amends the rejection.
//!
//! [`IdleState::inhibited`] is the gate's own answer, off `Manager.BlockInhibited`, and
//! [`IdleState::inhibitors`] names the holders. The two are not redundant: this shell's own hold is
//! excluded (see [`foreign_idle_inhibitors`]), so `inhibited` true with an empty list means "the
//! only thing holding this awake is you", which the config can already explain in better words than
//! the `why` it passed down.

use serde::Serialize;

/// One logind inhibitor blocking idle, as a config would draw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IdleInhibitor {
    /// The `who` the holder passed to `Inhibit`, e.g. `"mpv"`. Free text chosen by that program, so
    /// it is a label to draw and never something to match on.
    pub who: String,
    /// The `why` the holder passed, e.g. `"Playing video"`. Also free text, and often empty.
    pub why: String,
}

/// `oblisk.idle`'s payload (ADR-0141).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IdleState {
    /// Anything at all is holding an idle inhibitor, this shell included. While true no threshold
    /// event reaches the config, so a config's own countdown has to stop here rather than keep
    /// running against events that will never arrive.
    pub inhibited: bool,
    /// The holders that are not this shell.
    pub inhibitors: Vec<IdleInhibitor>,
}

/// One row of `Manager.ListInhibitors`: `what`, `who`, `why`, `mode`, `uid`, `pid`.
pub(crate) type InhibitorRow = (String, String, String, String, u32, u32);

/// The rows that actually block idling, minus this shell's own.
///
/// `mode` matters as much as `what`: a `delay` inhibitor asks for a grace period before a sleep and
/// does not stop the seat idling, so counting one would report a hold that is not there. `what` goes
/// through [`super::gate::blocks_idle`] rather than being re-parsed beside it, so the list this
/// draws and the gate that acts cannot disagree about what counts.
///
/// `who` is compared against [`super::inhibit::INHIBIT_WHO`] rather than a pid, because the fd is
/// held by the Supervisor and the pid logind records is this process either way.
pub(crate) fn foreign_idle_inhibitors(rows: Vec<InhibitorRow>) -> Vec<IdleInhibitor> {
    rows.into_iter()
        .filter(|(what, who, _, mode, _, _)| {
            mode == "block" && super::gate::blocks_idle(what) && who != super::inhibit::INHIBIT_WHO
        })
        .map(|(_, who, why, _, _, _)| IdleInhibitor { who, why })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(what: &str, who: &str, why: &str, mode: &str) -> InhibitorRow {
        (what.to_string(), who.to_string(), why.to_string(), mode.to_string(), 1000, 42)
    }

    #[test]
    fn a_block_mode_idle_inhibitor_is_listed() {
        let listed = foreign_idle_inhibitors(vec![row("idle", "mpv", "Playing video", "block")]);
        assert_eq!(listed, vec![IdleInhibitor { who: "mpv".into(), why: "Playing video".into() }]);
    }

    #[test]
    fn a_delay_mode_inhibitor_does_not_stop_the_seat_idling_and_is_not_listed() {
        assert!(foreign_idle_inhibitors(vec![row("idle", "mpv", "Playing video", "delay")]).is_empty());
    }

    #[test]
    fn an_inhibitor_that_blocks_something_other_than_idle_is_not_listed() {
        let rows = vec![row("handle-power-key:handle-lid-switch", "gdm", "", "block")];
        assert!(foreign_idle_inhibitors(rows).is_empty());
    }

    #[test]
    fn idle_is_found_inside_a_colon_separated_what() {
        let listed = foreign_idle_inhibitors(vec![row("sleep:idle", "steam", "Game running", "block")]);
        assert_eq!(listed, vec![IdleInhibitor { who: "steam".into(), why: "Game running".into() }]);
    }

    #[test]
    fn this_shells_own_hold_is_excluded_because_the_config_already_knows_its_own_reasons() {
        let rows = vec![row("idle", super::super::inhibit::INHIBIT_WHO, "manual + video", "block")];
        assert!(foreign_idle_inhibitors(rows).is_empty());
    }

    #[test]
    fn an_empty_why_is_kept_rather_than_dropping_the_holder() {
        let listed = foreign_idle_inhibitors(vec![row("idle", "systemd-inhibit", "", "block")]);
        assert_eq!(listed, vec![IdleInhibitor { who: "systemd-inhibit".into(), why: String::new() }]);
    }

    #[test]
    fn a_holder_that_is_neither_block_mode_nor_idle_is_filtered_on_both_counts() {
        let rows = vec![row("sleep", "backup", "Copying", "delay"), row("idle", "mpv", "Playing", "block")];
        assert_eq!(foreign_idle_inhibitors(rows).len(), 1);
    }
}
