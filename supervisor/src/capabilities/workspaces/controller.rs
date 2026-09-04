//! [`WorkspacesController`]: the `oblisk.workspaces` state owner and its one write action, plus
//! the compositor-neutral reduction behind them. Split from `workspaces`, see `workspaces/mod.rs`
//! for the module-level doc.
//!
//! Nothing here names a compositor's own type: [`derive_state`] takes [`WorkspaceRow`]s and a
//! [`FocusedWindow`], the shape any compositor's IPC reduces to; `workspaces::niri` does the
//! reducing today.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{CompositorKind, detect_compositor, unsupported_session_report};

use super::niri;

/// `oblisk.workspaces`'s full payload (§ 2.9); field names are the JSON keys verbatim, and
/// `active_client` (`Option`, § 2.9's "or `nil` if none focused") is omitted, not `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkspacesState {
    /// One entry per output, keyed by connector name; empty until the compositor first answers.
    pub outputs: Vec<OutputWorkspaces>,
    /// The focused toplevel, or `nil` if none. One window per session, not per output: there is
    /// no way to ask what is focused on an unfocused monitor (ADR-0056 decision 4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_client: Option<ActiveClient>,
}

/// One output's workspace state; `workspaces` is ADR-0056 decision 3's addition to § 2.9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct OutputWorkspaces {
    /// The connector name, e.g. `"eDP-1"`. Matches an `oblisk.screens` entry's `name` and a
    /// surface's `monitor`.
    pub name: String,
    /// The [`WorkspaceEntry::id`] of the workspace visible on this output; every output has one.
    pub active_workspace: u64,
    /// ADR-0056 decision 4: present only on the output that actually holds focus, so
    /// `out.focused_workspace ~= nil` is the "is this the focused monitor" test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_workspace: Option<u64>,
    /// The workspaces on this output, ordered by [`WorkspaceEntry::idx`]. What a strip draws: the
    /// two ids above are opaque on their own and name nothing a user would recognise.
    pub workspaces: Vec<WorkspaceEntry>,
}

/// `id` is the compositor's stable, monitor-independent identity: what `active_workspace`/
/// `focused_workspace` refer to and what `workspaces:focus(id)` takes. `idx` is the 1-based
/// position on that output (what a keybind/button label means), not stable across a reorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkspaceEntry {
    /// Stable identity, independent of which output the workspace sits on. What the two ids on
    /// [`OutputWorkspaces`] refer to and what `workspaces:focus(id)` takes.
    pub id: u64,
    /// 1-based position on this output. Not stable: a reorder renumbers it, which is why it is the
    /// thing to draw and [`WorkspaceEntry::id`] is the thing to send.
    pub idx: u8,
    /// The compositor's own name for the workspace, or `nil` when it has none. Most do not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// At least one window sits on this workspace (ADR-0117). What a strip dims an empty
    /// workspace by.
    pub populated: bool,
    /// The Wayland `app_id` of the window that stands for this workspace: the focused one when
    /// focus is here, else the compositor's first. Absent when the workspace is empty or its
    /// windows report no id, so `nil` and "draw the number" are the same test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

/// § 2.9's `active_client`, minus `is_fullscreen` (ADR-0056 decision 5: niri-ipc 26.4.0's `Window`
/// has no such field, and a fabricated `false` would be wrong for fullscreen windows); `class` is
/// Wayland's `app_id`, since X11's `WM_CLASS` has no Wayland equivalent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ActiveClient {
    /// The window title, e.g. `"src/main.rs - Neovim"`. Empty string for a window that sets none.
    pub title: String,
    /// The Wayland `app_id`, e.g. `"firefox"`. Named `class` for the X11 habit, but a Wayland
    /// toplevel has no `WM_CLASS`. The key `applications.by_app_id` is built to be looked up by.
    pub class: String,
    /// The compositor has this window floating rather than tiled.
    pub is_floating: bool,
}

/// One workspace as a compositor reports it, reduced to the fields [`derive_state`] reads.
/// The input type of the reduction, so the reduction and its tests belong to no compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: u64,
    pub idx: u8,
    pub name: Option<String>,
    /// The connector this workspace sits on, or `None` when the compositor has no output to put
    /// it on at all (niri reports that with no monitor connected). Such a row is dropped.
    pub output: Option<String>,
    /// The workspace its own output is showing. Per output: every output has exactly one.
    pub is_active: bool,
    /// The workspace holding keyboard focus. Global: exactly one across the whole session, which
    /// is what makes `OutputWorkspaces::focused_workspace` optional (ADR-0056 decision 4).
    pub is_focused: bool,
    /// Whether any window sits here, and which one stands for the workspace (ADR-0117). Picked by
    /// the adaptor, which holds the window list this module never sees; an empty id is `None`.
    pub populated: bool,
    pub app_id: Option<String>,
}

/// The focused toplevel, reduced to the three fields § 2.9's `active_client` carries.
///
/// *Which* window holds focus is the adaptor's question, not this module's: niri flags it on each
/// window, another compositor may query it separately. What that window becomes in the payload is
/// this module's job, done by [`derive_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedWindow {
    pub title: String,
    /// Wayland's `app_id`, filling § 2.9's `class` (ADR-0056 decision 5).
    pub app_id: String,
    pub is_floating: bool,
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacesSignal {
    Changed,
}

/// Folds a compositor's rows into § 2.9's payload. Pure, so the whole mapping is unit tested
/// without a compositor. Outputs sort by connector name and workspaces by `idx`, since rows
/// arrive in whatever order the adaptor's map iterated, not an order; an output with no active
/// workspace is omitted rather than given a fabricated id (should be unreachable).
pub fn derive_state(workspaces: &[WorkspaceRow], focused: Option<&FocusedWindow>) -> WorkspacesState {
    let mut by_output: HashMap<&str, Vec<&WorkspaceRow>> = HashMap::new();
    for workspace in workspaces {
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
                .map(|workspace| WorkspaceEntry {
                    id: workspace.id,
                    idx: workspace.idx,
                    name: workspace.name.clone(),
                    populated: workspace.populated,
                    app_id: workspace.app_id.clone(),
                })
                .collect();
            Some(OutputWorkspaces { name: name.to_string(), active_workspace, focused_workspace, workspaces })
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));

    let active_client = focused.map(|window| ActiveClient {
        title: window.title.clone(),
        class: window.app_id.clone(),
        is_floating: window.is_floating,
    });

    WorkspacesState { outputs, active_client }
}

/// The half of a compositor reader that isn't about the compositor: reduce, drop a no-op update,
/// store, and wake `main.rs`, shared by every adaptor through [`StatePublisher::publish`] so the
/// logic is written, and agreed on, exactly once.
pub struct StatePublisher {
    state: Arc<Mutex<WorkspacesState>>,
    events: UnboundedSender<WorkspacesSignal>,
    previous: WorkspacesState,
}

impl StatePublisher {
    pub fn new(state: Arc<Mutex<WorkspacesState>>, events: UnboundedSender<WorkspacesSignal>) -> Self {
        Self { state, events, previous: WorkspacesState::default() }
    }

    /// `false` once nothing is listening, a reader loop's exit condition. Deliberately not
    /// debounced: a compositor replaying startup state as several events pushes several times,
    /// each real (niri sends workspaces and windows separately, so the first push predates any
    /// known window).
    pub fn publish(&mut self, workspaces: &[WorkspaceRow], focused: Option<&FocusedWindow>) -> bool {
        let current = derive_state(workspaces, focused);
        if current == self.previous {
            return true;
        }
        *self.state.lock().expect("workspaces state mutex poisoned") = current.clone();
        self.previous = current;
        self.events.send(WorkspacesSignal::Changed).is_ok()
    }
}

/// `workspaces:focus(id)`'s `arguments: [id]`. Shape check only: whether the id names a
/// workspace that exists is the compositor's question, answered by doing nothing.
pub fn parse_focus_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `Clone` is deliberately absent: nothing here is handed to a spawned future. The adaptor
/// spawns its own OS thread and moves only what it needs, because niri's socket is a blocking
/// `std::net::UnixStream`, not tokio-aware.
pub struct WorkspacesController {
    state: Arc<Mutex<WorkspacesState>>,
    compositor: Option<CompositorKind>,
}

impl WorkspacesController {
    /// Returns immediately: a session with no implementor never spawns a reader and so never
    /// pushes (ADR-0056 decision 1). The match is exhaustive, not defaulted, so a new
    /// `CompositorKind` fails this build at the arm this file exists to add.
    pub fn new(events: UnboundedSender<WorkspacesSignal>) -> Self {
        let state = Arc::new(Mutex::new(WorkspacesState::default()));
        let compositor = detect_compositor();
        match compositor {
            Some(CompositorKind::Niri) => niri::spawn_reader(StatePublisher::new(Arc::clone(&state), events)),
            Some(CompositorKind::Hyprland) => eprintln!(
                "workspaces: this session is Hyprland, which has no implementor yet (ADR-0056 decision 1); workspace reporting disabled for this run"
            ),
            None => {
                eprintln!("workspaces: {}; workspace reporting disabled for this run", unsupported_session_report())
            }
        }
        Self { state, compositor }
    }

    pub fn snapshot(&self) -> WorkspacesState {
        self.state.lock().expect("workspaces state mutex poisoned").clone()
    }

    /// `workspaces:focus(id)`, routed to whichever adaptor is live. Exhaustive for the same
    /// reason [`WorkspacesController::new`] is.
    pub fn focus(&self, id: u64) {
        match self.compositor {
            Some(CompositorKind::Niri) => niri::focus(id),
            Some(CompositorKind::Hyprland) | None => {
                eprintln!("workspaces: focus({id}) called but this session has no workspace implementor; ignored")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: u64, idx: u8, output: &str, is_active: bool, is_focused: bool) -> WorkspaceRow {
        WorkspaceRow {
            id,
            idx,
            name: None,
            output: Some(output.to_string()),
            is_active,
            is_focused,
            populated: false,
            app_id: None,
        }
    }

    fn window(title: &str, app_id: &str, is_floating: bool) -> FocusedWindow {
        FocusedWindow { title: title.to_string(), app_id: app_id.to_string(), is_floating }
    }

    // ---- derive_state: grouping and ordering ----

    #[test]
    fn derive_state_groups_by_output_and_orders_outputs_and_workspaces_deterministically() {
        // Passed out of order on purpose: an adaptor folds a map, and map iteration is not an order.
        let workspaces = [
            workspace(9, 3, "eDP-1", false, false),
            workspace(2, 1, "DP-2", true, false),
            workspace(5, 1, "eDP-1", true, true),
            workspace(7, 2, "eDP-1", false, false),
        ];

        let state = derive_state(&workspaces, None);

        assert_eq!(state.outputs.iter().map(|out| out.name.as_str()).collect::<Vec<_>>(), ["DP-2", "eDP-1"]);
        let edp = &state.outputs[1];
        assert_eq!(edp.workspaces.iter().map(|entry| entry.idx).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(edp.workspaces.iter().map(|entry| entry.id).collect::<Vec<_>>(), [5, 7, 9]);
    }

    #[test]
    fn derive_state_carries_populated_and_app_id_through_and_omits_an_absent_app_id() {
        let mut busy = workspace(5, 1, "eDP-1", true, true);
        busy.populated = true;
        busy.app_id = Some("firefox".to_string());
        let workspaces = [busy, workspace(7, 2, "eDP-1", false, false)];

        let state = derive_state(&workspaces, None);
        let entries = &state.outputs[0].workspaces;

        assert!(entries[0].populated);
        assert_eq!(entries[0].app_id.as_deref(), Some("firefox"));
        assert!(!entries[1].populated);
        let json = serde_json::to_value(&entries[1]).unwrap();
        assert!(json.get("app_id").is_none(), "an empty workspace has no app_id key: {json}");
        assert_eq!(json["populated"], false);
    }

    #[test]
    fn derive_state_reports_the_active_workspace_by_id_not_by_index() {
        // `id` and `idx` deliberately disagree: a reorder moves `idx` and leaves `id` alone, so
        // a mapping that reached for the wrong one would still pass if they happened to agree.
        let state = derive_state(&[workspace(42, 1, "eDP-1", true, true)], None);

        assert_eq!(state.outputs[0].active_workspace, 42);
        assert_eq!(state.outputs[0].workspaces[0].idx, 1);
    }

    #[test]
    fn derive_state_ignores_a_workspace_that_has_no_output() {
        // Real: a compositor reports no output for a workspace when none are connected at all.
        let orphan = WorkspaceRow { output: None, ..workspace(1, 1, "eDP-1", true, true) };

        assert_eq!(derive_state(&[orphan], None).outputs, Vec::new());
    }

    #[test]
    fn derive_state_omits_an_output_with_no_active_workspace_rather_than_inventing_one() {
        let workspaces = [workspace(1, 1, "eDP-1", false, false), workspace(2, 1, "DP-2", true, false)];

        let state = derive_state(&workspaces, None);

        assert_eq!(state.outputs.len(), 1, "an output with no active workspace reported is not listed");
        assert_eq!(state.outputs[0].name, "DP-2");
    }

    // ---- derive_state: focus (ADR-0056 decision 4) ----

    #[test]
    fn derive_state_puts_focused_workspace_only_on_the_output_that_holds_focus() {
        let workspaces = [workspace(1, 1, "eDP-1", true, false), workspace(2, 1, "DP-2", true, true)];

        let state = derive_state(&workspaces, None);

        let dp = state.outputs.iter().find(|out| out.name == "DP-2").unwrap();
        let edp = state.outputs.iter().find(|out| out.name == "eDP-1").unwrap();
        assert_eq!(dp.focused_workspace, Some(2), "the focused output reports the id it is focused on");
        assert_eq!(edp.focused_workspace, None, "an output that does not hold focus must not claim it does");
    }

    #[test]
    fn an_unfocused_output_omits_focused_workspace_from_its_json_entirely() {
        let json = serde_json::to_value(derive_state(&[workspace(1, 1, "eDP-1", true, false)], None)).unwrap();

        let output = &json["outputs"][0];
        assert!(
            output.get("focused_workspace").is_none(),
            "an absent key reads as nil in Lua; a `null` would too, but only an absent key matches every other optional field here"
        );
        assert_eq!(output["active_workspace"], 1);
    }

    // ---- derive_state: active_client (ADR-0056 decision 5) ----

    #[test]
    fn derive_state_maps_the_focused_window_onto_active_client_with_app_id_standing_in_for_class() {
        let focused = window("src/main.rs - Neovim", "kitty", true);

        let client = derive_state(&[], Some(&focused)).active_client.expect("a focused window produces active_client");

        assert_eq!(client.title, "src/main.rs - Neovim");
        assert_eq!(client.class, "kitty", "§ 2.9's `class` is Wayland's `app_id`; a Wayland toplevel has no WM_CLASS");
        assert!(client.is_floating);
    }

    #[test]
    fn derive_state_has_no_active_client_when_no_window_holds_focus() {
        // Real, not hypothetical: focusing a layer-shell surface leaves every toplevel unfocused.
        assert_eq!(derive_state(&[workspace(1, 1, "eDP-1", true, true)], None).active_client, None);
    }

    #[test]
    fn active_client_carries_no_is_fullscreen_key_at_all() {
        // Pins ADR-0056 decision 5: if a later compositor gains `is_fullscreen`, this test
        // says the omission was a decision.
        let json = serde_json::to_value(derive_state(&[], Some(&window("a title", "kitty", false)))).unwrap();

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

    // ---- StatePublisher ----

    fn publisher() -> (StatePublisher, tokio::sync::mpsc::UnboundedReceiver<WorkspacesSignal>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StatePublisher::new(Arc::new(Mutex::new(WorkspacesState::default())), tx), rx)
    }

    #[test]
    fn publish_stores_the_state_and_signals_once_per_real_change() {
        let (mut publisher, mut rx) = publisher();
        let workspaces = [workspace(5, 1, "eDP-1", true, true)];

        assert!(publisher.publish(&workspaces, None));
        assert!(publisher.publish(&workspaces, None), "an event that changes nothing is not a change");
        assert!(publisher.publish(&workspaces, Some(&window("a title", "kitty", false))));

        assert_eq!(publisher.state.lock().unwrap().active_client.as_ref().unwrap().class, "kitty");
        let signals = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(signals, 2, "the repeated middle publish must not wake main.rs");
    }

    #[test]
    fn publish_reports_false_once_nothing_is_listening_so_a_reader_loop_can_stop() {
        let (mut publisher, rx) = publisher();
        drop(rx);

        assert!(!publisher.publish(&[workspace(5, 1, "eDP-1", true, true)], None));
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
