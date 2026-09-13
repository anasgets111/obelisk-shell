//! `workspaces`' niri implementor, reached over `$NIRI_SOCKET`.
//!
//! The only file naming `niri_ipc`. `controller.rs` owns payload, reduction, and publish in terms
//! of `WorkspaceRow`/`FocusedWindow`; this maps niri types and drives the loop.
//!
//! ADR-0056 decision 1 still applies: one implementor needs no trait. A second compositor is a
//! sibling plus two `WorkspacesController` arms, inheriting `derive_state`, `StatePublisher`, and
//! their tests instead of predicting the payload from one implementation.

use std::collections::HashMap;

use super::controller::{FocusedWindow, StatePublisher, WorkspaceRow};

/// niri workspaces reduced to the common input. Clone `name` and `output` per event; a session has
/// only a handful of workspaces.
///
/// `populated`/`app_id` (ADR-0117) use `Window.workspace_id`: focused window id when focused
/// there, otherwise the lowest id because the map has no order and the compositor's first tile is
/// not on the wire. Empty `app_id` is `None`.
fn workspace_rows(
    workspaces: &HashMap<u64, niri_ipc::Workspace>,
    windows: &HashMap<u64, niri_ipc::Window>,
) -> Vec<WorkspaceRow> {
    workspaces
        .values()
        .map(|workspace| {
            let standing = windows
                .values()
                .filter(|window| window.workspace_id == Some(workspace.id))
                .min_by_key(|window| (!window.is_focused, window.id));
            WorkspaceRow {
                id: workspace.id,
                idx: workspace.idx,
                name: workspace.name.clone(),
                output: workspace.output.clone(),
                is_active: workspace.is_active,
                is_focused: workspace.is_focused,
                populated: standing.is_some(),
                app_id: standing.and_then(|window| window.app_id.clone()).filter(|id| !id.is_empty()),
            }
        })
        .collect()
}

/// niri flags focus on each window, so search here rather than in `derive_state`. Clone only the
/// winner; even a fifty-window session builds one `FocusedWindow` per event.
///
/// Wire `title`/`app_id` are `Option` but the payload makes them non-nullable, so default to empty. A
/// window reporting neither is still a real toplevel.
fn focused_window(windows: &HashMap<u64, niri_ipc::Window>) -> Option<FocusedWindow> {
    windows.values().find(|window| window.is_focused).map(|window| FocusedWindow {
        title: window.title.clone().unwrap_or_default(),
        app_id: window.app_id.clone().unwrap_or_default(),
        is_floating: window.is_floating,
        // niri-ipc 26.4.0 has no fullscreen field (ADR-0056 decision 5); absent, not `false`.
        is_fullscreen: None,
    })
}

/// Connects, requests the event stream, and folds events into niri's two state parts on an OS
/// thread (`std::net::UnixStream`). `EventStreamStatePart::apply` returns ignored events, so one
/// `if let` chains both parts.
///
/// Uses a second event-stream connection; `keyboard` already owns one for `KeyboardLayoutsChanged`
/// (ADR-0056 decision 2 weighs this against sharing).
///
/// ponytail: `niri_ipc::state` panics on `WindowClosed` or `WindowLayoutsChanged` for an unknown
/// window (`.expect`). The reader feeds one ordered stream from the full replay, so it cannot
/// violate those invariants externally. If one fires, only this thread dies; workspaces stop for
/// the run and stderr gets the backtrace. Upgrade with `catch_unwind` around `apply`, resetting
/// both parts and re-requesting the stream; no instance has been observed and this code cannot
/// trigger the case.
pub fn spawn_reader(mut publisher: StatePublisher) {
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

            let rows = workspace_rows(&niri_workspaces.workspaces, &niri_windows.windows);
            let focused = focused_window(&niri_windows.windows);
            if !publisher.publish(&rows, focused.as_ref(), None) {
                return;
            }
        }
    });
}

/// `workspaces:focus(id)`. Use a fresh connection: `read_events` consumes and shuts down the
/// event-stream socket's write half. Use `WorkspaceReferenceArg::Id`, not `Index`; `idx` shifts
/// on reorder and could focus the wrong workspace.
pub fn focus(id: u64) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures deserialize niri wire JSON, copied from live `niri msg -j workspaces`/`-j windows`
    /// rather than struct literals. A renamed field breaks them; the wire contract is the niri
    /// upgrade boundary, so tests stay with the adaptor.
    fn workspace(id: u64, idx: u8, output: &str, is_active: bool, is_focused: bool) -> niri_ipc::Workspace {
        serde_json::from_value(serde_json::json!({
            "id": id, "idx": idx, "name": null, "output": output,
            "is_urgent": false, "is_active": is_active, "is_focused": is_focused, "active_window_id": null
        }))
        .unwrap()
    }

    fn window(id: u64, title: &str, app_id: &str, is_focused: bool, is_floating: bool) -> niri_ipc::Window {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": title, "app_id": app_id, "pid": 1481, "workspace_id": 5,
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

    #[test]
    fn workspace_rows_carry_every_field_the_reduction_reads() {
        let rows = workspace_rows(
            &map(vec![(5, workspace(5, 2, "eDP-1", true, true))]),
            &map(vec![(2, window(2, "src/main.rs - Neovim", "kitty", true, true))]),
        );

        assert_eq!(
            rows,
            vec![WorkspaceRow {
                id: 5,
                idx: 2,
                name: None,
                output: Some("eDP-1".to_string()),
                is_active: true,
                is_focused: true,
                populated: true,
                app_id: Some("kitty".to_string()),
            }]
        );
    }

    #[test]
    fn workspace_rows_stand_a_workspace_in_by_its_focused_window_else_its_lowest_id() {
        let workspaces =
            map(vec![(5, workspace(5, 1, "eDP-1", true, true)), (6, workspace(6, 2, "eDP-1", false, false))]);
        let mut elsewhere = window(30, "Sign in | Slack", "slack", false, false);
        elsewhere.workspace_id = Some(6);
        let windows = map(vec![
            (14, window(14, "Inbox", "thunderbird", false, false)),
            (2, window(2, "src/main.rs - Neovim", "kitty", true, true)),
            (30, elsewhere),
        ]);

        let rows = workspace_rows(&workspaces, &windows);
        let app_of = |id: u64| rows.iter().find(|row| row.id == id).unwrap().app_id.clone();

        assert_eq!(app_of(5).as_deref(), Some("kitty"), "focus wins over a lower id");
        assert_eq!(app_of(6).as_deref(), Some("slack"));

        let mut unfocused = windows.clone();
        unfocused.get_mut(&2).unwrap().is_focused = false;
        let rows = workspace_rows(&workspaces, &unfocused);
        assert_eq!(rows.iter().find(|row| row.id == 5).unwrap().app_id.as_deref(), Some("kitty"), "lowest id");
    }

    #[test]
    fn workspace_rows_mark_an_empty_workspace_unpopulated_with_no_app_id() {
        let mut nameless = window(2, "", "", false, false);
        nameless.app_id = None;
        let rows = workspace_rows(
            &map(vec![(5, workspace(5, 1, "eDP-1", true, true)), (6, workspace(6, 2, "eDP-1", false, false))]),
            &map(vec![(2, nameless)]),
        );
        let row_of = |id: u64| rows.iter().find(|row| row.id == id).unwrap();

        assert_eq!(
            (row_of(5).populated, row_of(5).app_id.as_deref()),
            (true, None),
            "a window with no id still populates"
        );
        assert_eq!((row_of(6).populated, row_of(6).app_id.as_deref()), (false, None));
    }

    #[test]
    fn workspace_rows_keep_a_workspace_niri_reports_no_output_for() {
        // `derive_state` drops it; the adaptor reports niri's value. niri sets `output: null` with
        // no connected outputs.
        let mut orphan = workspace(1, 1, "eDP-1", true, true);
        orphan.output = None;

        assert_eq!(workspace_rows(&map(vec![(1, orphan)]), &HashMap::new())[0].output, None);
    }

    #[test]
    fn focused_window_picks_the_window_niri_flags_and_renames_app_id_to_the_class_slot() {
        let windows = map(vec![
            (2, window(2, "src/main.rs - Neovim", "kitty", true, true)),
            (14, window(14, "Sign in | Slack", "slack", false, false)),
        ]);

        let focused = focused_window(&windows).expect("a flagged window is the focused one");

        assert_eq!(focused.title, "src/main.rs - Neovim");
        assert_eq!(focused.app_id, "kitty");
        assert!(focused.is_floating);
    }

    #[test]
    fn focused_window_is_none_when_niri_flags_nothing() {
        // Real: focusing a layer-shell surface leaves every toplevel unfocused.
        let windows = map(vec![(14, window(14, "Sign in | Slack", "slack", false, false))]);

        assert_eq!(focused_window(&windows), None);
    }

    #[test]
    fn focused_window_defaults_a_null_title_or_app_id_to_empty_rather_than_dropping_the_window() {
        // The payload declares both non-nullable, while niri's wire uses `Option` for both.
        let mut bare = window(2, "", "", true, false);
        bare.title = None;
        bare.app_id = None;

        let focused = focused_window(&map(vec![(2, bare)])).expect("a titleless window is still focused");

        assert_eq!((focused.title.as_str(), focused.app_id.as_str()), ("", ""));
    }
}
