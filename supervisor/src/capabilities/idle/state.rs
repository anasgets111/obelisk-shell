//! What `oblisk.idle` publishes: whether anything holds the session awake, and who.
//!
//! ADR-0139 kept the gate out of Lua: a read-state roster would buy only push machinery because
//! `register_threshold` callbacks cannot cross the wire. That hid why a countdown stopped while
//! the Supervisor dropped every threshold event: observed live with `systemd-inhibit --what=idle
//! --who=mpv`, the screen said nothing. ADR-0141 adds the roster.
//!
//! [`IdleState::inhibited`] comes from `Manager.BlockInhibited`; [`IdleState::inhibitors`] names
//! holders. The shell's own hold is excluded (see [`foreign_idle_inhibitors`]), which config can
//! explain better than the `why` it passed down. Thus `inhibited` true with an empty list means
//! only this shell holds the session awake.

use serde::Serialize;

/// One logind inhibitor blocking idle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IdleInhibitor {
    /// Free-text `who` passed to `Inhibit`, e.g. `"mpv"`; draw it as a label, never match it.
    pub who: String,
    /// Free-text `why`, e.g. `"Playing video"`, often empty.
    pub why: String,
}

/// `oblisk.idle` payload (ADR-0141).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IdleState {
    /// Any idle inhibitor is held, including this shell. While true, no threshold event reaches
    /// config, so its countdown must stop.
    pub inhibited: bool,
    /// Idle-inhibitor holders other than this shell.
    pub inhibitors: Vec<IdleInhibitor>,
}

/// One `Manager.ListInhibitors` row: `what`, `who`, `why`, `mode`, `uid`, `pid`.
pub(crate) type InhibitorRow = (String, String, String, String, u32, u32);

/// Rows that block idling, excluding this shell.
///
/// `delay` only requests grace before sleep; it does not stop seat idling, so only `block` counts.
/// `what` uses [`super::gate::blocks_idle`], keeping the drawn list and gate's decision identical.
///
/// Compare `who` with [`super::inhibit::INHIBIT_WHO`], not pid: the Supervisor holds the fd and
/// logind records this process's pid either way.
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
