//! `workspaces`' second implementor: Hyprland, over the two sockets in its instance directory
//! (ADR-0118). Built to Hyprland's documented IPC and not live-tested, the way `keyboard`'s
//! `HyprlandLink` was: this dev machine runs niri.
//!
//! Hyprland has no event stream carrying state. `.socket2.sock` pushes `event>>payload` lines
//! that say *that* something changed, and the state is read back over `.socket.sock` with the
//! same JSON `hyprctl -j` prints. So the loop is: wait for a line naming a workspace, monitor or
//! window event, re-read `workspaces`, `monitors`, `clients` and `activewindow`, reduce, publish.
//! Every event in a burst re-reads (a window opening sends four or five), and `StatePublisher`
//! drops the equal results; four local socket round trips per event is not worth a coalescing
//! timer until someone measures it.
//!
//! How Hyprland's model lands on `WorkspaceRow`:
//!
//! - `id` is Hyprland's workspace number, which is also what `dispatch workspace N` takes, so
//!   `workspaces:focus(id)` keeps its one meaning and focusing a number no workspace has yet
//!   creates it. That number is `idx` too: Hyprland has no per-monitor position, the number *is*
//!   the slot a user's keybind means, and a strip labelling `idx` reads right. `name` is set only
//!   when Hyprland's differs from the number, which for a numbered workspace it never does.
//! - Active per output is the monitor's `activeWorkspace`; focused is the active workspace of the
//!   one monitor with `focused: true`. That is niri's `is_active`/`is_focused` split exactly.
//! - `populated` is the workspace's own `windows` count. `app_id` is the `class` of the client
//!   with the lowest `focusHistoryID` there (`0` is the focused window, higher is older), which
//!   is ADR-0117's "focused, else first" with a real order behind "first".
//! - The focused window is `activewindow`, which is `{}` while a layer surface holds focus.
//!   `clients[].focusHistoryID == 0` would name the last toplevel instead, wrongly.
//! - Workspaces with an id of zero or below are dropped: specials (`special:` names, ids from
//!   -99 down) and named workspaces (the negatives above them). Neither fits a `u64` id or a
//!   number-keyed focus, and neither is modelled yet (§ 2.9's second gap). While a special is
//!   shown on the focused monitor the focused row is still that monitor's regular workspace.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::Deserialize;

use super::controller::{FocusedWindow, StatePublisher, WorkspaceRow};
use crate::compositor::hyprland_socket_path;

/// One entry of `j/workspaces`. `windows` is Hyprland's own count, so an empty workspace needs
/// no client scan.
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

/// One entry of `j/monitors`: the connector name, what it shows, and whether it holds focus.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyprlandMonitor {
    name: String,
    active_workspace: WorkspaceRef,
    #[serde(default)]
    focused: bool,
}

/// `{ "id": 3, "name": "3" }`, how a monitor and a client name the workspace they are on.
#[derive(Debug, Clone, Deserialize)]
struct WorkspaceRef {
    id: i64,
}

/// One entry of `j/clients`, and the whole of `j/activewindow`. `mapped` defaults to `true`
/// because `activewindow` does not print it; a client that is not mapped yet has no surface and
/// cannot stand for a workspace.
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
}

fn yes() -> bool {
    true
}

/// The three lists reduced to the reduction's input. See the module doc for each mapping.
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
            let app_id = clients
                .iter()
                .filter(|client| client.mapped && client.workspace.id == workspace.id && !client.class.is_empty())
                .min_by_key(|client| client.focus_history_id)
                .map(|client| client.class.clone());
            let number = workspace.id.to_string();
            WorkspaceRow {
                // A `u64` in the payload because ids are; the filter above keeps this positive.
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

/// `j/activewindow`'s reply, `{}` when no toplevel holds focus. A reply that is neither a client
/// nor empty is a protocol change, said once per occurrence rather than swallowed as "nothing
/// focused".
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
        Ok(client) => Some(FocusedWindow { title: client.title, app_id: client.class, is_floating: client.floating }),
        Err(err) => {
            eprintln!(
                "workspaces: Hyprland's activewindow reply has an unexpected shape; treating as no focused window: {err}"
            );
            None
        }
    }
}

/// The events after which the state is re-read, by name (the part before `>>`, with a `v2`
/// suffix dropped since every v2 event ships beside its v1). A table, not "every event": the
/// socket also carries `activelayout`, `submap`, `screencast` and the like, none of which move a
/// workspace, and `keyboard` already answers the first.
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

/// One command over `.socket.sock`: Hyprland answers a single request per connection and closes.
fn request(socket_path: &PathBuf, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(command.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// The four reads, parsed. `None` logs which one failed and leaves the previous publish standing,
/// so one dropped request under a burst costs a stale frame, not the rest of the run.
fn read_state(socket_path: &PathBuf) -> Option<(Vec<WorkspaceRow>, Option<FocusedWindow>)> {
    fn read<T: for<'de> Deserialize<'de>>(socket_path: &PathBuf, name: &str) -> Option<T> {
        let reply = match request(socket_path, &format!("j/{name}")) {
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
    let active = match request(socket_path, "j/activewindow") {
        Ok(reply) => reply,
        Err(err) => {
            eprintln!("workspaces: Hyprland `activewindow` request failed; skipping this update: {err}");
            return None;
        }
    };
    Some((workspace_rows(&workspaces, &monitors, &clients), focused_window(&active)))
}

fn signature() -> Option<String> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    (!signature.is_empty()).then_some(signature)
}

/// Connects to the event socket first and reads the state second, so a change between the two
/// is a line still to come rather than one missed. Then one OS thread (blocking reads, as the
/// niri reader) re-reads after every trigger line until the socket ends or nobody listens.
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
        if let Some((rows, focused)) = read_state(&command_path)
            && !publisher.publish(&rows, focused.as_ref())
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
            if let Some((rows, focused)) = read_state(&command_path)
                && !publisher.publish(&rows, focused.as_ref())
            {
                return;
            }
        }
        eprintln!("workspaces: Hyprland event socket closed; workspaces will no longer update");
    });
}

/// `workspaces:focus(id)` as `dispatch workspace N`: the id is the number (module doc), and a
/// number with no workspace yet makes one, which is how an empty slot a strip pads in is entered.
/// Hyprland answers `ok`, or a sentence saying why not; anything but `ok` is printed.
pub fn focus(id: u64) {
    let Some(signature) = signature() else {
        eprintln!("workspaces: focus({id}) called but HYPRLAND_INSTANCE_SIGNATURE is unset; ignored");
        return;
    };
    std::thread::spawn(move || {
        let socket_path = hyprland_socket_path(&signature, ".socket.sock");
        match request(&socket_path, &format!("dispatch workspace {id}")) {
            Ok(reply) if reply.trim() == "ok" => {}
            Ok(reply) => eprintln!("workspaces: Hyprland refused `dispatch workspace {id}`: {}", reply.trim()),
            Err(err) => eprintln!("workspaces: Hyprland `dispatch workspace {id}` request failed: {err}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures follow the shape `hyprctl -j` documents (the wiki's "Using hyprctl" page) with
    /// the fields this adaptor reads and a sample of the ones it ignores, written by hand: no
    /// Hyprland session was available to capture from. A field Hyprland renames breaks these
    /// tests only once a capture replaces them, which is the first thing to do on a Hyprland
    /// machine.
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
        // Workspaces 1 and 7 with nothing between: `idx` must read 7, not "second on this
        // monitor", because 7 is what the user's keybind and `dispatch workspace 7` mean.
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
            FocusedWindow { title: "~ - fish".to_string(), app_id: "kitty".to_string(), is_floating: true }
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
}
