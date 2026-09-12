//! `workspaces`' second implementor: Hyprland over its two instance-directory sockets (ADR-0118).
//! Live against 0.56.2; the fixtures below still carry the shapes it was written to.
//!
//! 0.56 replaced the command socket's plain-text language with Lua: `dispatch <what>` is now
//! sugar for `return hl.dispatch(<what>)`, so the pre-0.56 `dispatch workspace 2` reaches the
//! parser as `hl.dispatch(workspace 2)` and dies on the space. Reads are untouched -- `j/` is
//! still the legacy path -- so a stale write syntax leaves a strip that tracks the compositor
//! perfectly and cannot drive it. Writes here target 0.56 and no earlier.
//!
//! Hyprland has no state event stream. `.socket2.sock` sends `event>>payload` lines; `.socket.sock`
//! returns the `hyprctl -j` JSON. On a workspace/monitor/window event, re-read
//! `workspaces`/`monitors`/`clients`/`activewindow`, reduce, and publish. Bursts re-read every
//! event (a window opening sends four or five); `StatePublisher` drops equal results, so four
//! local round trips per event remain until measured data justifies coalescing.
//!
//! How Hyprland's model lands on `WorkspaceRow`:
//!
//! - `id` is Hyprland's number and the `workspace` field of [`focus_command`], so focusing a new
//!   number creates it. It is also `idx`: Hyprland has no per-monitor position. `name` is set only when
//!   it differs from the number.
//! - Per-output active is `activeWorkspace`; focused is the active workspace of the monitor with
//!   `focused: true`, matching niri's `is_active`/`is_focused` split.
//! - `populated` is `windows`; `app_id` is the `class` of the lowest `focusHistoryID` (`0` focused,
//!   higher older), giving ADR-0117's "focused, else first" a real order.
//! - Focused window is `activewindow`, `{}` while a layer surface has focus. Using
//!   `clients[].focusHistoryID == 0` would incorrectly name the last toplevel.
//! - Drop ids <= 0. Specials (`special`/`special:` names, ids <= -99) become `special` (ADR-0119),
//!   shown on the monitor naming them in `specialWorkspace`; toggle with
//!   [`toggle_special_command`]. Other negative named workspaces
//!   drop because neither `u64` ids nor number focus can represent them. A shown special does not
//!   replace the focused monitor's regular workspace.
//! - `is_fullscreen` is active-window `fullscreen`: int since Hyprland 0.42 (`0` none, `1`
//!   maximized, `2` fullscreen), bool before; only the real value counts.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::Deserialize;

use super::controller::{FocusedWindow, SpecialWorkspace, StatePublisher, WorkspaceRow};
use crate::compositor::{hyprland_command, hyprland_request, hyprland_socket_path};

/// One `j/workspaces` entry. Hyprland's `windows` count identifies empty workspaces without a
/// client scan.
#[derive(Debug, Clone, Deserialize)]
struct HyprlandWorkspace {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    monitor: String,
    #[serde(default)]
    windows: u32,
}

/// One `j/monitors` entry: connector, shown workspace, and focus.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyprlandMonitor {
    name: String,
    active_workspace: WorkspaceRef,
    /// `{ "id": 0, "name": "" }` when no special is shown here.
    #[serde(default)]
    special_workspace: WorkspaceRef,
    #[serde(default)]
    focused: bool,
}

/// `{ "id": 3, "name": "3" }`, the monitor/client workspace reference.
#[derive(Debug, Clone, Default, Deserialize)]
struct WorkspaceRef {
    id: i64,
    #[serde(default)]
    name: String,
}

/// One `j/clients` entry or `j/activewindow`. `mapped` defaults to `true` because `activewindow`
/// omits it; an unmapped client has no surface to represent a workspace.
#[derive(Debug, Clone, Deserialize)]
struct HyprlandClient {
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    workspace: WorkspaceRef,
    #[serde(default)]
    floating: bool,
    #[serde(rename = "focusHistoryID", default)]
    focus_history_id: i64,
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default, deserialize_with = "fullscreen_flag")]
    fullscreen: bool,
}

fn yes() -> bool {
    true
}

/// `fullscreen` in either wire shape: bool as itself, int as "is `2`".
fn fullscreen_flag<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Bool(flag) => flag,
        serde_json::Value::Number(mode) => mode.as_i64() == Some(2),
        _ => false,
    })
}

/// Workspace representative: mapped, classed, and most recently focused.
fn standing_app_id(clients: &[HyprlandClient], workspace_id: i64) -> Option<String> {
    clients
        .iter()
        .filter(|client| client.mapped && client.workspace.id == workspace_id && !client.class.is_empty())
        .min_by_key(|client| client.focus_history_id)
        .map(|client| client.class.clone())
}

/// The three lists reduced to the input rows; see the module doc for mappings.
fn workspace_rows(
    workspaces: &[HyprlandWorkspace],
    monitors: &[HyprlandMonitor],
    clients: &[HyprlandClient],
) -> Vec<WorkspaceRow> {
    workspaces
        .iter()
        .filter(|workspace| workspace.id > 0)
        .map(|workspace| {
            let monitor = monitors.iter().find(|monitor| monitor.name == workspace.monitor);
            let is_active = monitor.is_some_and(|monitor| monitor.active_workspace.id == workspace.id);
            let is_focused = is_active && monitor.is_some_and(|monitor| monitor.focused);
            let app_id = standing_app_id(clients, workspace.id);
            let number = workspace.id.to_string();
            WorkspaceRow {
                // Payload ids are `u64`; the filter above keeps this positive.
                id: workspace.id as u64,
                idx: u8::try_from(workspace.id).unwrap_or(u8::MAX),
                name: (workspace.name != number && !workspace.name.is_empty()).then(|| workspace.name.clone()),
                output: (!workspace.monitor.is_empty()).then(|| workspace.monitor.clone()),
                is_active,
                is_focused,
                populated: workspace.windows > 0,
                app_id,
            }
        })
        .collect()
}

/// Specials by name. Hyprland marks them by name and gives them ids <= -99; name is the stable
/// test used by the rest of the adaptor.
fn special_list(
    workspaces: &[HyprlandWorkspace],
    monitors: &[HyprlandMonitor],
    clients: &[HyprlandClient],
) -> Vec<SpecialWorkspace> {
    workspaces
        .iter()
        .filter(|workspace| workspace.id <= 0 && workspace.name.starts_with("special"))
        .map(|workspace| SpecialWorkspace {
            name: workspace.name.clone(),
            populated: workspace.windows > 0,
            app_id: standing_app_id(clients, workspace.id),
            shown_on: monitors
                .iter()
                .find(|monitor| monitor.special_workspace.name == workspace.name)
                .map(|monitor| monitor.name.clone()),
        })
        .collect()
}

/// `j/activewindow` reply: `{}` without a focused toplevel. Any other non-client shape is a
/// protocol change, logged once per occurrence rather than treated as no focus.
fn focused_window(json: &str) -> Option<FocusedWindow> {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("workspaces: Hyprland's activewindow reply is not JSON; treating as no focused window: {err}");
            return None;
        }
    };
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        return None;
    }
    match serde_json::from_value::<HyprlandClient>(value) {
        Ok(client) => Some(FocusedWindow {
            title: client.title,
            app_id: client.class,
            is_floating: client.floating,
            is_fullscreen: Some(client.fullscreen),
        }),
        Err(err) => {
            eprintln!(
                "workspaces: Hyprland's activewindow reply has an unexpected shape; treating as no focused window: {err}"
            );
            None
        }
    }
}

/// Event names that trigger a state read: prefix before `>>`, with `v2` removed because each v2
/// ships beside v1. Excludes `activelayout`, `submap`, `screencast`, and similar events that do
/// not move workspaces; `keyboard` already handles its own state.
const TRIGGERS: &[&str] = &[
    "workspace",
    "focusedmon",
    "createworkspace",
    "destroyworkspace",
    "moveworkspace",
    "renameworkspace",
    "activespecial",
    "openwindow",
    "closewindow",
    "movewindow",
    "activewindow",
    "changefloatingmode",
    "windowtitle",
    "monitoradded",
    "monitorremoved",
];

fn is_trigger(line: &str) -> bool {
    let name = line.split_once(">>").map_or(line, |(name, _)| name);
    let name = name.strip_suffix("v2").unwrap_or(name);
    TRIGGERS.contains(&name)
}

/// Four parsed reads. `None` logs the failed read and leaves the previous publish, so one dropped
/// request costs a stale frame, not the run.
type State = (Vec<WorkspaceRow>, Option<FocusedWindow>, Vec<SpecialWorkspace>);

fn read_state(socket_path: &Path) -> Option<State> {
    fn read<T: for<'de> Deserialize<'de>>(socket_path: &Path, name: &str) -> Option<T> {
        let reply = match hyprland_request(socket_path, &format!("j/{name}")) {
            Ok(reply) => reply,
            Err(err) => {
                eprintln!("workspaces: Hyprland `{name}` request failed; skipping this update: {err}");
                return None;
            }
        };
        match serde_json::from_str(&reply) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                eprintln!("workspaces: Hyprland `{name}` reply did not parse; skipping this update: {err}");
                None
            }
        }
    }

    let workspaces: Vec<HyprlandWorkspace> = read(socket_path, "workspaces")?;
    let monitors: Vec<HyprlandMonitor> = read(socket_path, "monitors")?;
    let clients: Vec<HyprlandClient> = read(socket_path, "clients")?;
    let active = match hyprland_request(socket_path, "j/activewindow") {
        Ok(reply) => reply,
        Err(err) => {
            eprintln!("workspaces: Hyprland `activewindow` request failed; skipping this update: {err}");
            return None;
        }
    };
    Some((
        workspace_rows(&workspaces, &monitors, &clients),
        focused_window(&active),
        special_list(&workspaces, &monitors, &clients),
    ))
}

fn signature() -> Option<String> {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|signature| !signature.is_empty())
}

/// Connects to the event socket before the first state read, so an intervening change remains a
/// line to process. One OS thread then re-reads after every trigger until socket end or no
/// listener.
pub fn spawn_reader(mut publisher: StatePublisher) {
    let Some(signature) = signature() else {
        eprintln!(
            "workspaces: HYPRLAND_INSTANCE_SIGNATURE is unset or empty; workspace reporting disabled for this run"
        );
        return;
    };
    let events_path = hyprland_socket_path(&signature, ".socket2.sock");
    let command_path = hyprland_socket_path(&signature, ".socket.sock");
    let stream = match UnixStream::connect(&events_path) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "workspaces: failed to connect to Hyprland's event socket at {}; workspace reporting disabled for this run: {err}",
                events_path.display()
            );
            return;
        }
    };

    std::thread::spawn(move || {
        if let Some((rows, focused, special)) = read_state(&command_path)
            && !publisher.publish(&rows, focused.as_ref(), Some(&special))
        {
            return;
        }
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                eprintln!("workspaces: Hyprland event socket read failed; workspaces will no longer update");
                return;
            };
            if !is_trigger(&line) {
                continue;
            }
            if let Some((rows, focused, special)) = read_state(&command_path)
                && !publisher.publish(&rows, focused.as_ref(), Some(&special))
            {
                return;
            }
        }
        eprintln!("workspaces: Hyprland event socket closed; workspaces will no longer update");
    });
}

/// One `dispatch` on its own thread. Hyprland answers `ok` or a reason; anything else is printed
/// with the command.
fn dispatch(what: String) {
    let Some(signature) = signature() else {
        eprintln!("workspaces: `dispatch {what}` requested but HYPRLAND_INSTANCE_SIGNATURE is unset; ignored");
        return;
    };
    std::thread::spawn(move || {
        let socket_path = hyprland_socket_path(&signature, ".socket.sock");
        hyprland_command(&socket_path, &format!("dispatch {what}"), "workspaces");
    });
}

/// `workspaces:focus(id)`; a new number creates the empty slot a strip can pad into.
pub fn focus(id: u64) {
    dispatch(focus_command(id));
}

/// The table form, not `hl.dsp.focus(N)`: `focus` takes one table and reads the field, the same
/// call that moves focus by `direction`. Parenthesised because Hyprland appends a "syntax might need
/// to be updated" note to errors from a command with no `(` in it.
fn focus_command(id: u64) -> String {
    format!("hl.dsp.focus({{ workspace = {id} }})")
}

/// `workspaces:toggle_special(name)`.
pub fn toggle_special(name: &str) {
    dispatch(toggle_special_command(name));
}

/// Strip `special:`; the unnamed `special` passes the empty string, which the dispatcher reads as
/// the unnamed one. The dispatcher adds the prefix, so a full name would become
/// `special:special:term`.
fn toggle_special_command(name: &str) -> String {
    format!("hl.dsp.workspace.toggle_special(\"{}\")", lua_escape(special_argument(name)))
}

fn special_argument(name: &str) -> &str {
    name.strip_prefix("special:").unwrap_or(if name == "special" { "" } else { name })
}

/// The command is Lua source now, and a quote in a name -- from a config or the compositor's own
/// list -- would close the string and run the rest. `\\ddd` is padded to three digits: Lua reads up
/// to three, so an unpadded escape swallows a following digit.
fn lua_escape(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            c if c.is_ascii_control() => format!("\\{:03}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handwritten fixtures follow the wiki's "Using hyprctl" page and documented `hyprctl -j`
    /// shapes, with read fields and ignored samples; no Hyprland session was available. On
    /// Hyprland, replace them with a capture first if a renamed field breaks the tests.
    fn workspaces(json: serde_json::Value) -> Vec<HyprlandWorkspace> {
        serde_json::from_value(json).unwrap()
    }

    fn monitors(json: serde_json::Value) -> Vec<HyprlandMonitor> {
        serde_json::from_value(json).unwrap()
    }

    fn clients(json: serde_json::Value) -> Vec<HyprlandClient> {
        serde_json::from_value(json).unwrap()
    }

    fn workspace(id: i64, name: &str, monitor: &str, windows: u32) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": name, "monitor": monitor, "monitorID": 0, "windows": windows,
            "hasfullscreen": false, "lastwindow": "0x0", "lastwindowtitle": "", "ispersistent": false
        })
    }

    fn monitor(name: &str, active: i64, focused: bool) -> serde_json::Value {
        serde_json::json!({
            "id": 0, "name": name, "description": "Some Vendor Model", "width": 2560, "height": 1440,
            "activeWorkspace": { "id": active, "name": active.to_string() },
            "specialWorkspace": { "id": 0, "name": "" },
            "focused": focused, "disabled": false
        })
    }

    fn showing_special(mut monitor: serde_json::Value, id: i64, name: &str) -> serde_json::Value {
        monitor["specialWorkspace"] = serde_json::json!({ "id": id, "name": name });
        monitor
    }

    fn client(class: &str, title: &str, workspace: i64, focus_history_id: i64, floating: bool) -> serde_json::Value {
        serde_json::json!({
            "address": "0x55d1c0a3b2c0", "mapped": true, "hidden": false, "at": [0, 0], "size": [1280, 1440],
            "workspace": { "id": workspace, "name": workspace.to_string() },
            "floating": floating, "pseudo": false, "monitor": 0, "class": class, "title": title,
            "initialClass": class, "initialTitle": title, "pid": 4242, "xwayland": false, "pinned": false,
            "fullscreen": 0, "fullscreenClient": 0, "grouped": [], "tags": [], "swallowing": "0x0",
            "focusHistoryID": focus_history_id, "inhibitingIdle": false
        })
    }

    #[test]
    fn workspace_rows_carry_every_field_the_reduction_reads() {
        let rows = workspace_rows(
            &workspaces(serde_json::json!([workspace(3, "3", "DP-1", 2)])),
            &monitors(serde_json::json!([monitor("DP-1", 3, true)])),
            &clients(serde_json::json!([
                client("firefox", "Hyprland Wiki", 3, 1, false),
                client("kitty", "~", 3, 0, true),
            ])),
        );

        assert_eq!(
            rows,
            vec![WorkspaceRow {
                id: 3,
                idx: 3,
                name: None,
                output: Some("DP-1".to_string()),
                is_active: true,
                is_focused: true,
                populated: true,
                app_id: Some("kitty".to_string()),
            }]
        );
    }

    #[test]
    fn the_number_is_the_id_and_the_idx_and_a_numbered_workspace_has_no_name() {
        // Workspaces 1 and 7: `idx` is 7, not second-on-monitor, because keybinds dispatch 7.
        let rows = workspace_rows(
            &workspaces(serde_json::json!([workspace(7, "7", "DP-1", 0), workspace(1, "1", "DP-1", 1)])),
            &monitors(serde_json::json!([monitor("DP-1", 1, true)])),
            &[],
        );

        let seven = rows.iter().find(|row| row.id == 7).unwrap();
        assert_eq!(seven.idx, 7);
        assert_eq!(seven.name, None);
        assert!(!seven.populated);
        assert_eq!(seven.app_id, None);
    }

    #[test]
    fn a_workspace_named_something_other_than_its_number_keeps_the_name() {
        let rows = workspace_rows(
            &workspaces(serde_json::json!([workspace(2, "mail", "DP-1", 1)])),
            &monitors(serde_json::json!([monitor("DP-1", 2, true)])),
            &[],
        );

        assert_eq!(rows[0].name.as_deref(), Some("mail"));
    }

    #[test]
    fn active_is_per_monitor_and_focused_is_the_active_workspace_of_the_focused_monitor() {
        let rows = workspace_rows(
            &workspaces(serde_json::json!([
                workspace(1, "1", "DP-1", 1),
                workspace(2, "2", "DP-1", 0),
                workspace(5, "5", "HDMI-A-1", 1),
            ])),
            &monitors(serde_json::json!([monitor("DP-1", 2, false), monitor("HDMI-A-1", 5, true)])),
            &[],
        );
        let row = |id: u64| rows.iter().find(|row| row.id == id).unwrap();

        assert!(row(2).is_active && !row(2).is_focused, "shown on an unfocused monitor");
        assert!(!row(1).is_active && !row(1).is_focused);
        assert!(row(5).is_active && row(5).is_focused, "shown on the focused monitor");
        assert_eq!(rows.iter().filter(|row| row.is_focused).count(), 1);
    }

    #[test]
    fn the_app_id_is_the_most_recently_focused_mapped_client_with_a_class() {
        let mut unmapped = client("steam", "Steam", 1, 3, false);
        unmapped["mapped"] = serde_json::json!(false);
        let rows = workspace_rows(
            &workspaces(serde_json::json!([workspace(1, "1", "DP-1", 3)])),
            &monitors(serde_json::json!([monitor("DP-1", 1, true)])),
            &clients(serde_json::json!([
                client("slack", "Slack", 1, 4, false),
                client("", "untitled", 1, 2, false),
                unmapped,
                client("thunderbird", "Inbox", 2, 0, false),
            ])),
        );

        assert_eq!(
            rows[0].app_id.as_deref(),
            Some("slack"),
            "the classless and unmapped ones lose, the other workspace's does not count"
        );
    }

    #[test]
    fn specials_and_named_negatives_are_dropped_and_a_workspace_off_every_monitor_has_no_output() {
        let rows = workspace_rows(
            &workspaces(serde_json::json!([
                workspace(-99, "special:scratch", "DP-1", 1),
                workspace(-1, "notes", "DP-1", 1),
                workspace(1, "1", "DP-1", 1),
                workspace(4, "4", "", 0),
            ])),
            &monitors(serde_json::json!([monitor("DP-1", 1, true)])),
            &[],
        );

        assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), [1, 4]);
        assert_eq!(rows[1].output, None, "`derive_state` drops a row with no output");
        assert!(!rows[1].is_active);
    }

    #[test]
    fn focused_window_reads_activewindow_and_treats_the_empty_object_as_nothing_focused() {
        let focused = focused_window(&client("kitty", "~ - fish", 1, 0, true).to_string()).unwrap();
        assert_eq!(
            focused,
            FocusedWindow {
                title: "~ - fish".to_string(),
                app_id: "kitty".to_string(),
                is_floating: true,
                is_fullscreen: Some(false)
            }
        );

        assert_eq!(focused_window("{}"), None, "a layer surface holds focus");
        assert_eq!(focused_window("not json"), None);
    }

    #[test]
    fn a_trigger_is_matched_by_event_name_with_or_without_its_v2_suffix() {
        assert!(is_trigger("workspace>>3"));
        assert!(is_trigger("workspacev2>>3,3"));
        assert!(is_trigger("activewindowv2>>55d1c0a3b2c0"));
        assert!(is_trigger("openwindow>>55d1c0a3b2c0,3,kitty,~"));
        assert!(!is_trigger("activelayout>>at-translated-set-2-keyboard,English (US)"));
        assert!(!is_trigger("submap>>resize"));
        assert!(!is_trigger("urgent>>55d1c0a3b2c0"));
    }

    #[test]
    fn is_fullscreen_reads_the_int_mode_and_the_older_bool_and_counts_only_real_fullscreen() {
        let mut window = client("mpv", "film.mkv", 1, 0, false);
        for (wire, expected) in [
            (serde_json::json!(2), Some(true)),
            (serde_json::json!(1), Some(false)),
            (serde_json::json!(0), Some(false)),
            (serde_json::json!(true), Some(true)),
            (serde_json::json!(false), Some(false)),
        ] {
            window["fullscreen"] = wire.clone();
            assert_eq!(focused_window(&window.to_string()).unwrap().is_fullscreen, expected, "{wire}");
        }
    }

    #[test]
    fn special_list_names_each_special_with_its_standing_app_and_the_monitor_showing_it() {
        let specials = special_list(
            &workspaces(serde_json::json!([
                workspace(1, "1", "DP-1", 1),
                workspace(-1, "notes", "DP-1", 1),
                workspace(-99, "special:term", "DP-1", 2),
                workspace(-98, "special", "HDMI-A-1", 0),
            ])),
            &monitors(serde_json::json!([
                showing_special(monitor("DP-1", 1, true), -99, "special:term"),
                monitor("HDMI-A-1", 2, false),
            ])),
            &clients(serde_json::json!([client("kitty", "~", -99, 0, true), client("btop", "btop", -99, 3, true)])),
        );

        assert_eq!(
            specials.iter().map(|special| special.name.as_str()).collect::<Vec<_>>(),
            ["special:term", "special"]
        );
        assert_eq!(specials[0].app_id.as_deref(), Some("kitty"));
        assert!(specials[0].populated);
        assert_eq!(specials[0].shown_on.as_deref(), Some("DP-1"));
        assert!(!specials[1].populated);
        assert_eq!(specials[1].shown_on, None, "exists but is hidden");
    }

    #[test]
    fn the_toggle_argument_is_the_name_without_its_prefix_and_nothing_for_the_unnamed_special() {
        assert_eq!(special_argument("special:term"), "term");
        assert_eq!(special_argument("special"), "");
        assert_eq!(special_argument("term"), "term", "a config passing the short name is not punished");
    }

    /// 0.56 reads the command as Lua, so both writes are checked as source. Captured from a live
    /// 0.56.2 socket, which answers `ok` to each of these and a parse error to what they replaced.
    #[test]
    fn both_writes_are_the_lua_dispatchers_0_56_accepts() {
        assert_eq!(focus_command(3), "hl.dsp.focus({ workspace = 3 })");
        assert_eq!(toggle_special_command("special:term"), r#"hl.dsp.workspace.toggle_special("term")"#);
        assert_eq!(
            toggle_special_command("special"),
            r#"hl.dsp.workspace.toggle_special("")"#,
            "the unnamed special is the empty argument, not a missing one"
        );
        assert_eq!(
            toggle_special_command(r#"special:a") hl.dsp.exit(--"#),
            r#"hl.dsp.workspace.toggle_special("a\") hl.dsp.exit(--")"#,
            "a quote in a name stays inside the string"
        );
        assert_eq!(
            toggle_special_command("special:a\n5"),
            r#"hl.dsp.workspace.toggle_special("a\0105")"#,
            "a control byte is escaped, padded so the digit after it stays a digit"
        );
        assert_eq!(
            toggle_special_command("special:é"),
            r#"hl.dsp.workspace.toggle_special("é")"#,
            "a non-ASCII name reaches the socket as itself, not as its bytes read as codepoints"
        );
    }
}
