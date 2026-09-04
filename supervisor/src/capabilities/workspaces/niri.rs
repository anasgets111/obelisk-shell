//! `workspaces`' one implementor: niri's IPC event stream, reached over `$NIRI_SOCKET`.
//!
//! This is the only file in the capability that names `niri_ipc`. `controller.rs` owns the
//! payload, the reduction and the publish contract in terms of `WorkspaceRow`/`FocusedWindow`;
//! this maps niri's own types onto those and drives the loop.
//!
//! ADR-0056 decision 1 stands: there is still no trait, because there is still one implementor.
//! What changed is where the seam sits. A second compositor is a sibling module plus two arms in
//! `WorkspacesController`, and it inherits `derive_state`, `StatePublisher` and their tests
//! instead of re-deriving § 2.9's shape -- which is the part ADR-0056 said should be taken from
//! two real implementors rather than predicted from one.

use std::collections::HashMap;

use super::controller::{FocusedWindow, StatePublisher, WorkspaceRow};

/// niri's workspaces reduced to the reduction's input. `name` and `output` are cloned per event;
/// a session has a handful of workspaces, so this is not the place to avoid an allocation.
///
/// `populated`/`app_id` (ADR-0117) come from `Window.workspace_id`: the focused window's id when
/// focus is on that workspace, else the window with the lowest id, since the map has no order and
/// the tile the compositor calls first is not on the wire. An empty `app_id` is `None`.
fn workspace_rows(
    workspaces: &HashMap<u64, niri_ipc::Workspace>,
    windows: &HashMap<u64, niri_ipc::Window>,
) -> Vec<WorkspaceRow> {
    workspaces
        .values()
        .map(|workspace| {
            let mut here: Vec<&niri_ipc::Window> =
                windows.values().filter(|window| window.workspace_id == Some(workspace.id)).collect();
            here.sort_by_key(|window| (!window.is_focused, window.id));
            WorkspaceRow {
                id: workspace.id,
                idx: workspace.idx,
                name: workspace.name.clone(),
                output: workspace.output.clone(),
                is_active: workspace.is_active,
                is_focused: workspace.is_focused,
                populated: !here.is_empty(),
                app_id: here.first().and_then(|window| window.app_id.clone()).filter(|id| !id.is_empty()),
            }
        })
        .collect()
}

/// niri answers "which window has focus" with a flag on each window, so the search is here
/// rather than in `derive_state` -- and only the winner is cloned, so a session with fifty
/// windows still builds one `FocusedWindow` per event.
///
/// `title`/`app_id` are `Option` on the wire and default to empty: § 2.9 declares both
/// non-nullable, and a window that reports neither is a real (if odd) toplevel, not an absence
/// of one.
fn focused_window(windows: &HashMap<u64, niri_ipc::Window>) -> Option<FocusedWindow> {
    windows.values().find(|window| window.is_focused).map(|window| FocusedWindow {
        title: window.title.clone().unwrap_or_default(),
        app_id: window.app_id.clone().unwrap_or_default(),
        is_floating: window.is_floating,
        // niri-ipc 26.4.0 has no fullscreen field (ADR-0056 decision 5); absent, not `false`.
        is_fullscreen: None,
    })
}

/// Connects, asks for the event stream, and folds every event into niri's own two state parts
/// on its own OS thread (blocking `std::net::UnixStream`). `EventStreamStatePart::apply` returns
/// the event back when its part ignored it, so one `if let` chains both parts.
///
/// A second event-stream connection to the same compositor: `keyboard` already holds one for
/// `KeyboardLayoutsChanged` (ADR-0056 decision 2 weighs that against sharing it).
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

/// `workspaces:focus(id)`. A fresh connection per call: `read_events` consumes and shuts down
/// the write half of the event-stream socket, so the reader's connection can't also send this
/// write. `WorkspaceReferenceArg::Id`, not `Index`: `idx` shifts under a reorder, so addressing
/// by index could focus the wrong workspace.
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

    /// Both fixtures deserialize niri's own wire JSON rather than a struct literal, copied from
    /// a live `niri msg -j workspaces`/`-j windows` -- this would break if niri renamed a field.
    /// That guarantee is why these tests live with the adaptor: the wire contract is the only
    /// thing in this capability that a niri upgrade can break.
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
        // Dropping it is `derive_state`'s call, not the adaptor's: the adaptor reports what niri
        // said. niri sets `output: null` when no outputs are connected at all.
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
        // Real, not hypothetical: focusing a layer-shell surface leaves every toplevel unfocused.
        let windows = map(vec![(14, window(14, "Sign in | Slack", "slack", false, false))]);

        assert_eq!(focused_window(&windows), None);
    }

    #[test]
    fn focused_window_defaults_a_null_title_or_app_id_to_empty_rather_than_dropping_the_window() {
        // § 2.9 declares both non-nullable, and both are `Option` on niri's wire.
        let mut bare = window(2, "", "", true, false);
        bare.title = None;
        bare.app_id = None;

        let focused = focused_window(&map(vec![(2, bare)])).expect("a titleless window is still focused");

        assert_eq!((focused.title.as_str(), focused.app_id.as_str()), ("", ""));
    }
}
