//! [`WorkspacesController`]: the `oblisk.workspaces` state owner and its one write action.
//! Split from `workspaces` -- see `workspaces/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::hardware::keyboard::layout::{CompositorKind, detect_compositor};

/// `oblisk.workspaces`'s full payload (§ 2.9). Field names are the JSON keys verbatim.
/// `active_client` is `Option` (§ 2.9: "or `nil` if none focused"), omitted rather than
/// serialized as `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkspacesState {
    pub outputs: Vec<OutputWorkspaces>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_client: Option<ActiveClient>,
}

/// One output's workspace state. `workspaces` is docs/adr/0056 decision 3's addition to
/// § 2.9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct OutputWorkspaces {
    pub name: String,
    pub active_workspace: u64,
    /// docs/adr/0056 decision 4: present only on the output that actually holds focus, so
    /// `out.focused_workspace ~= nil` is the "is this the focused monitor" test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_workspace: Option<u64>,
    pub workspaces: Vec<WorkspaceEntry>,
}

/// `id` is niri's stable, monitor-independent identity: what `active_workspace`/
/// `focused_workspace` refer to and what `workspaces:focus(id)` takes. `idx` is the 1-based
/// position on that output (what a keybind/button label means), not stable across a reorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkspaceEntry {
    pub id: u64,
    pub idx: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// § 2.9's `active_client`, minus `is_fullscreen` (docs/adr/0056 decision 5: niri-ipc 26.4.0's
/// `Window` has no such field, and a fabricated `false` would be wrong for fullscreen windows).
/// `class` is niri's `app_id`: X11's `WM_CLASS` has no Wayland equivalent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ActiveClient {
    pub title: String,
    pub class: String,
    pub is_floating: bool,
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacesSignal {
    Changed,
}

/// Folds niri's two event-stream state parts into § 2.9's payload. Pure, so the whole mapping
/// is unit tested without a compositor. Outputs are ordered by connector name and each
/// output's workspaces by `idx` -- `HashMap` iteration order is not an order. An output with
/// no active workspace is omitted rather than given a fabricated id (should be unreachable).
pub fn derive_state(
    workspaces: &HashMap<u64, niri_ipc::Workspace>,
    windows: &HashMap<u64, niri_ipc::Window>,
) -> WorkspacesState {
    let mut by_output: HashMap<&str, Vec<&niri_ipc::Workspace>> = HashMap::new();
    for workspace in workspaces.values() {
        let Some(output) = workspace.output.as_deref() else { continue };
        by_output.entry(output).or_default().push(workspace);
    }

    let mut outputs: Vec<OutputWorkspaces> = by_output
        .into_iter()
        .filter_map(|(name, mut group)| {
            group.sort_by_key(|workspace| workspace.idx);
            let active_workspace = group.iter().find(|workspace| workspace.is_active)?.id;
            let focused_workspace = group.iter().find(|workspace| workspace.is_focused).map(|workspace| workspace.id);
            let workspaces = group
                .into_iter()
                .map(|workspace| WorkspaceEntry { id: workspace.id, idx: workspace.idx, name: workspace.name.clone() })
                .collect();
            Some(OutputWorkspaces { name: name.to_string(), active_workspace, focused_workspace, workspaces })
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));

    let active_client = windows.values().find(|window| window.is_focused).map(|window| ActiveClient {
        title: window.title.clone().unwrap_or_default(),
        class: window.app_id.clone().unwrap_or_default(),
        is_floating: window.is_floating,
    });

    WorkspacesState { outputs, active_client }
}

/// `workspaces:focus(id)`'s `arguments: [id]`. Shape check only: whether the id names a
/// workspace that exists is niri's question, answered by doing nothing.
pub fn parse_focus_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `Clone` is deliberately absent: nothing here is handed to a spawned future.
/// [`WorkspacesController::focus`] spawns its own OS thread and moves only the id, because
/// niri's socket is a blocking `std::net::UnixStream`, not tokio-aware.
pub struct WorkspacesController {
    state: Arc<Mutex<WorkspacesState>>,
    compositor: Option<CompositorKind>,
}

impl WorkspacesController {
    /// Returns immediately. A session that is not niri leaves `compositor` at something this
    /// capability cannot read, never spawns the reader, and so never pushes at all
    /// (docs/adr/0056 decision 1).
    pub fn new(events: UnboundedSender<WorkspacesSignal>) -> Self {
        let state = Arc::new(Mutex::new(WorkspacesState::default()));
        let compositor = detect_compositor();
        match compositor {
            Some(CompositorKind::Niri) => spawn_niri_reader(Arc::clone(&state), events),
            Some(CompositorKind::Hyprland) => {
                eprintln!(
                    "workspaces: this session is Hyprland, which has no implementor yet (docs/adr/0056 decision 1); workspace reporting disabled for this run"
                );
            }
            None => {
                eprintln!("workspaces: no supported compositor detected; workspace reporting disabled for this run")
            }
        }
        Self { state, compositor }
    }

    pub fn snapshot(&self) -> WorkspacesState {
        self.state.lock().expect("workspaces state mutex poisoned").clone()
    }

    /// `workspaces:focus(id)`. A fresh connection per call: `read_events` consumes and shuts
    /// down the write half of the event-stream socket, so the reader's connection can't also
    /// send this write. `WorkspaceReferenceArg::Id`, not `Index`: `idx` shifts under a
    /// reorder, so addressing by index could focus the wrong workspace.
    pub fn focus(&self, id: u64) {
        if self.compositor != Some(CompositorKind::Niri) {
            eprintln!("workspaces: focus({id}) called but this session has no workspace implementor; ignored");
            return;
        }
        std::thread::spawn(move || {
            let mut socket = match niri_ipc::socket::Socket::connect() {
                Ok(socket) => socket,
                Err(err) => {
                    eprintln!("workspaces: failed to connect to the niri IPC socket for focus: {err}");
                    return;
                }
            };
            let request = niri_ipc::Request::Action(niri_ipc::Action::FocusWorkspace {
                reference: niri_ipc::WorkspaceReferenceArg::Id(id),
            });
            if let Err(err) = socket.send(request) {
                eprintln!("workspaces: niri FocusWorkspace({id}) request failed: {err}");
            }
        });
    }
}

/// Connects, asks for the event stream, and folds every event into niri's own two state parts
/// on its own OS thread (blocking `std::net::UnixStream`). `EventStreamStatePart::apply`
/// returns the event back when its part ignored it, so one `if let` chains both parts.
///
/// ponytail: that reducer panics rather than degrading on two events, `WindowClosed` and
/// `WindowLayoutsChanged` naming a window it has never seen (both are a bare `.expect` in
/// `niri_ipc::state`). Those are niri's own invariants and this reader cannot violate them from
/// the outside: it feeds one stream, in order, starting from the full replay. If one ever does
/// fire, the panic kills this thread alone and workspaces silently stop updating for the rest of
/// the run, with a backtrace on stderr as the only clue. The upgrade path is a `catch_unwind`
/// around `apply` that resets both parts and re-requests the stream, and it is not built because
/// it would be error handling for a case with no observed instance and no way to reach it from
/// here.
fn spawn_niri_reader(state: Arc<Mutex<WorkspacesState>>, events: UnboundedSender<WorkspacesSignal>) {
    let mut socket = match niri_ipc::socket::Socket::connect() {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!(
                "workspaces: failed to connect to the niri IPC socket; workspace reporting disabled for this run: {err}"
            );
            return;
        }
    };
    match socket.send(niri_ipc::Request::EventStream) {
        Ok(Ok(niri_ipc::Response::Handled)) => {}
        Ok(Ok(_)) => {
            eprintln!(
                "workspaces: unexpected reply to the niri EventStream request; workspace reporting disabled for this run"
            );
            return;
        }
        Ok(Err(msg)) => {
            eprintln!("workspaces: niri EventStream request failed: {msg}");
            return;
        }
        Err(err) => {
            eprintln!("workspaces: failed to send the niri EventStream request: {err}");
            return;
        }
    }

    std::thread::spawn(move || {
        use niri_ipc::state::EventStreamStatePart;

        let mut read_event = socket.read_events();
        let mut niri_workspaces = niri_ipc::state::WorkspacesState::default();
        let mut niri_windows = niri_ipc::state::WindowsState::default();
        let mut previous = WorkspacesState::default();
        loop {
            let event = match read_event() {
                Ok(event) => event,
                Err(err) => {
                    eprintln!("workspaces: niri event stream ended; workspaces will no longer update: {err}");
                    return;
                }
            };
            if let Some(event) = niri_workspaces.apply(event) {
                niri_windows.apply(event);
            }

            // niri replays workspaces and windows as two separate startup events, so the first
            // push lands before any window is known. Deliberately not debounced.
            let current = derive_state(&niri_workspaces.workspaces, &niri_windows.windows);
            if current != previous {
                *state.lock().expect("workspaces state mutex poisoned") = current.clone();
                previous = current;
                if events.send(WorkspacesSignal::Changed).is_err() {
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both fixtures deserialize niri's own wire JSON rather than a struct literal, copied from
    /// a live `niri msg -j workspaces`/`-j windows` -- this would break if niri renamed a field.
    fn workspace(id: u64, idx: u8, output: &str, is_active: bool, is_focused: bool) -> niri_ipc::Workspace {
        serde_json::from_value(serde_json::json!({
            "id": id, "idx": idx, "name": null, "output": output,
            "is_urgent": false, "is_active": is_active, "is_focused": is_focused, "active_window_id": null
        }))
        .unwrap()
    }

    fn window(id: u64, title: &str, app_id: &str, is_focused: bool, is_floating: bool) -> niri_ipc::Window {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": title, "app_id": app_id, "pid": 1481, "workspace_id": 3,
            "is_focused": is_focused, "is_floating": is_floating, "is_urgent": false,
            "layout": {
                "pos_in_scrolling_layout": [3, 1], "tile_size": [1920.0, 1200.0], "window_size": [1920, 1200],
                "tile_pos_in_workspace_view": null, "window_offset_in_tile": [0.0, 0.0]
            },
            "focus_timestamp": null
        }))
        .unwrap()
    }

    fn map<T>(items: Vec<(u64, T)>) -> HashMap<u64, T> {
        items.into_iter().collect()
    }

    // ---- derive_state: grouping and ordering ----

    #[test]
    fn derive_state_groups_by_output_and_orders_outputs_and_workspaces_deterministically() {
        // Inserted out of order on purpose: `HashMap` iteration order is not an order.
        let workspaces = map(vec![
            (9, workspace(9, 3, "eDP-1", false, false)),
            (2, workspace(2, 1, "DP-2", true, false)),
            (5, workspace(5, 1, "eDP-1", true, true)),
            (7, workspace(7, 2, "eDP-1", false, false)),
        ]);

        let state = derive_state(&workspaces, &HashMap::new());

        assert_eq!(state.outputs.iter().map(|out| out.name.as_str()).collect::<Vec<_>>(), ["DP-2", "eDP-1"]);
        let edp = &state.outputs[1];
        assert_eq!(edp.workspaces.iter().map(|entry| entry.idx).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(edp.workspaces.iter().map(|entry| entry.id).collect::<Vec<_>>(), [5, 7, 9]);
    }

    #[test]
    fn derive_state_reports_the_active_workspace_by_id_not_by_index() {
        // `id` and `idx` deliberately disagree: a reorder moves `idx` and leaves `id` alone, so
        // a mapping that reached for the wrong one would still pass if they happened to agree.
        let workspaces = map(vec![(42, workspace(42, 1, "eDP-1", true, true))]);

        let state = derive_state(&workspaces, &HashMap::new());

        assert_eq!(state.outputs[0].active_workspace, 42);
        assert_eq!(state.outputs[0].workspaces[0].idx, 1);
    }

    #[test]
    fn derive_state_ignores_a_workspace_that_has_no_output() {
        // niri reports `output: null` when no outputs are connected at all.
        let mut orphan = workspace(1, 1, "eDP-1", true, true);
        orphan.output = None;
        let workspaces = map(vec![(1, orphan)]);

        assert_eq!(derive_state(&workspaces, &HashMap::new()).outputs, Vec::new());
    }

    #[test]
    fn derive_state_omits_an_output_with_no_active_workspace_rather_than_inventing_one() {
        let workspaces =
            map(vec![(1, workspace(1, 1, "eDP-1", false, false)), (2, workspace(2, 1, "DP-2", true, false))]);

        let state = derive_state(&workspaces, &HashMap::new());

        assert_eq!(state.outputs.len(), 1, "an output niri reports no active workspace for is not listed");
        assert_eq!(state.outputs[0].name, "DP-2");
    }

    // ---- derive_state: focus (docs/adr/0056 decision 4) ----

    #[test]
    fn derive_state_puts_focused_workspace_only_on_the_output_that_holds_focus() {
        let workspaces =
            map(vec![(1, workspace(1, 1, "eDP-1", true, false)), (2, workspace(2, 1, "DP-2", true, true))]);

        let state = derive_state(&workspaces, &HashMap::new());

        let dp = state.outputs.iter().find(|out| out.name == "DP-2").unwrap();
        let edp = state.outputs.iter().find(|out| out.name == "eDP-1").unwrap();
        assert_eq!(dp.focused_workspace, Some(2), "the focused output reports the id it is focused on");
        assert_eq!(edp.focused_workspace, None, "an output that does not hold focus must not claim it does");
    }

    #[test]
    fn an_unfocused_output_omits_focused_workspace_from_its_json_entirely() {
        let workspaces = map(vec![(1, workspace(1, 1, "eDP-1", true, false))]);

        let json = serde_json::to_value(derive_state(&workspaces, &HashMap::new())).unwrap();

        let output = &json["outputs"][0];
        assert!(
            output.get("focused_workspace").is_none(),
            "an absent key reads as nil in Lua; a `null` would too, but only an absent key matches every other optional field here"
        );
        assert_eq!(output["active_workspace"], 1);
    }

    // ---- derive_state: active_client (docs/adr/0056 decision 5) ----

    #[test]
    fn derive_state_maps_the_focused_window_onto_active_client_with_app_id_standing_in_for_class() {
        let windows = map(vec![
            (2, window(2, "src/main.rs - Neovim", "kitty", true, true)),
            (14, window(14, "Sign in | Slack", "slack", false, false)),
        ]);

        let client = derive_state(&HashMap::new(), &windows)
            .active_client
            .expect("a focused window must produce an active_client");

        assert_eq!(client.title, "src/main.rs - Neovim");
        assert_eq!(client.class, "kitty", "§ 2.9's `class` is niri's `app_id`; a Wayland toplevel has no WM_CLASS");
        assert!(client.is_floating);
    }

    #[test]
    fn derive_state_has_no_active_client_when_no_window_holds_focus() {
        // Real, not hypothetical: focusing a layer-shell surface leaves every toplevel unfocused.
        let windows = map(vec![(14, window(14, "Sign in | Slack", "slack", false, false))]);

        assert_eq!(derive_state(&HashMap::new(), &windows).active_client, None);
    }

    #[test]
    fn active_client_carries_no_is_fullscreen_key_at_all() {
        // Pins docs/adr/0056 decision 5: if a later niri gains `is_fullscreen`, this test says
        // the omission was a decision.
        let windows = map(vec![(2, window(2, "a title", "kitty", true, false))]);

        let json = serde_json::to_value(derive_state(&HashMap::new(), &windows)).unwrap();

        let client = &json["active_client"];
        assert_eq!(client["title"], "a title");
        assert!(client.get("is_fullscreen").is_none());
    }

    #[test]
    fn a_state_with_nothing_focused_omits_active_client_rather_than_nulling_it() {
        let json = serde_json::to_value(WorkspacesState::default()).unwrap();

        assert!(json.get("active_client").is_none());
        assert_eq!(json["outputs"], serde_json::json!([]));
    }

    // ---- parse_focus_args ----

    #[test]
    fn parse_focus_args_reads_the_first_argument_as_a_workspace_id() {
        assert_eq!(parse_focus_args(&[serde_json::json!(11)]), Some(11));
    }

    #[test]
    fn parse_focus_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_focus_args(&[]), None);
        assert_eq!(parse_focus_args(&[serde_json::json!("3")]), None);
        assert_eq!(parse_focus_args(&[serde_json::json!(-1)]), None);
    }
}
