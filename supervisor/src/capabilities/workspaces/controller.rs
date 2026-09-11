//! [`WorkspacesController`]: `obelisk.workspaces` state owner, write action, and compositor-neutral
//! reduction. See `workspaces/mod.rs`.
//!
//! [`derive_state`] knows no compositor type. It consumes [`WorkspaceRow`]s and a
//! [`FocusedWindow`]; `niri` and `hyprland` reduce their IPC into those rows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{CompositorKind, detect_compositor, unsupported_session_report};

use super::{hyprland, niri};

/// `obelisk.workspaces` payload (§ 2.9). Field names are JSON keys; absent `active_client` is
/// omitted, not `null` (§ 2.9 says `nil` when unfocused).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkspacesState {
    /// Source compositor, `"niri"` or `"hyprland"` (ADR-0119). Hyprland creates a numbered
    /// workspace on focus, so strips pad empty slots there; niri keeps its trailing empty one.
    pub compositor: String,
    /// One entry per output, keyed by connector name; empty until the first compositor answer.
    pub outputs: Vec<OutputWorkspaces>,
    /// Focused toplevel, or `nil` if none. One window per session, not per output; an unfocused
    /// monitor cannot be queried (ADR-0056 decision 4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_client: Option<ActiveClient>,
    /// Compositor special workspaces, Hyprland's scratchpads, ordered by name (ADR-0119). Absent
    /// when unsupported (`special == nil`); an empty list means supported but none exist. Hyprland
    /// lists a special only while it holds a window or is shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub special: Option<Vec<SpecialWorkspace>>,
}

/// One special workspace (ADR-0119), identified by `name`, the argument to
/// `workspaces:toggle_special(name)`; Hyprland uses names and negative ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SpecialWorkspace {
    /// Full compositor name, `"special:scratch"` or unnamed `"special"`.
    pub name: String,
    /// Whether at least one window sits on it.
    pub populated: bool,
    /// `app_id` of its standing window, chosen as [`WorkspaceEntry::app_id`] is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Connector currently showing it, absent while hidden; a special shows on one output at a
    /// time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shown_on: Option<String>,
}

/// One output's workspace state; `workspaces` is ADR-0056 decision 3's addition to § 2.9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct OutputWorkspaces {
    /// Connector name, e.g. `"eDP-1"`; matches `obelisk.screens.name` and a surface's `monitor`.
    pub name: String,
    /// [`WorkspaceEntry::id`] visible on this output; every output has one.
    pub active_workspace: u64,
    /// Present only on the focused output (ADR-0056 decision 4); `out.focused_workspace ~= nil`
    /// tests whether this is the focused monitor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_workspace: Option<u64>,
    /// Workspaces on this output, ordered by [`WorkspaceEntry::idx`]; the strip draws these because
    /// the two ids above are opaque.
    pub workspaces: Vec<WorkspaceEntry>,
}

/// `id` is the stable, monitor-independent identity used by `active_workspace`,
/// `focused_workspace`, and `workspaces:focus(id)`. `idx` is the output-local 1-based position,
/// useful for labels but unstable across reorders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkspaceEntry {
    /// Stable identity independent of output; the ids on [`OutputWorkspaces`] and
    /// `workspaces:focus(id)` use it.
    pub id: u64,
    /// 1-based position on this output. Reorders renumber it, so draw `idx` but send `id`.
    pub idx: u8,
    /// Compositor name, or `nil` when it has none; most do not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether a window sits here (ADR-0117); strips dim empty workspaces.
    pub populated: bool,
    /// Wayland `app_id` of its representative window: focused when focused, otherwise the
    /// compositor's first. Absent for empty workspaces or windows without an id; `nil` means draw
    /// the number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

/// § 2.9's `active_client`. `is_fullscreen` is present only when reported (ADR-0056 decision 5
/// rejects fabricated `false`; ADR-0119 lets Hyprland provide it). `class` is Wayland `app_id`;
/// Wayland has no X11 `WM_CLASS` equivalent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ActiveClient {
    /// Window title, e.g. `"src/main.rs - Neovim"`; empty when unset.
    pub title: String,
    /// Wayland `app_id`, e.g. `"firefox"`. Named `class` for X11 familiarity; use it with
    /// `applications.by_app_id`.
    pub class: String,
    /// Whether the compositor floats this window rather than tiles it.
    pub is_floating: bool,
    /// Whether the window covers its whole output. Absent when unreported (niri-ipc has no field,
    /// ADR-0056 decision 5); Hyprland reports it (ADR-0119).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_fullscreen: Option<bool>,
}

/// One compositor workspace reduced to [`derive_state`]'s input fields; owned by neither adaptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: u64,
    pub idx: u8,
    pub name: Option<String>,
    /// Connector, or `None` when no output exists (niri reports this with no monitor); dropped.
    pub output: Option<String>,
    /// Workspace shown on its own output; every output has exactly one.
    pub is_active: bool,
    /// Workspace holding keyboard focus. Exactly one is global, making
    /// `OutputWorkspaces::focused_workspace` optional (ADR-0056 decision 4).
    pub is_focused: bool,
    /// Whether a window sits here and which `app_id` represents it (ADR-0117). The adaptor chooses
    /// it from its private window list; an empty id is `None`.
    pub populated: bool,
    pub app_id: Option<String>,
}

/// The focused toplevel reduced to § 2.9's three `active_client` fields.
///
/// The adaptor decides which window is focused (niri flags each one); [`derive_state`] maps the
/// winner into the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedWindow {
    pub title: String,
    /// Wayland `app_id`, filling § 2.9's `class` (ADR-0056 decision 5).
    pub app_id: String,
    pub is_floating: bool,
    /// `None` when unreported; it stays absent in the payload.
    pub is_fullscreen: Option<bool>,
}

/// Shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacesSignal {
    Changed,
}

/// Folds rows into § 2.9's payload. Pure and unit-tested without a compositor. Sorts outputs by
/// connector and workspaces by `idx`; omits an output with no active workspace rather than
/// fabricating an id (should be unreachable).
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
        is_fullscreen: window.is_fullscreen,
    });

    WorkspacesState { compositor: String::new(), outputs, active_client, special: None }
}

/// Compositor-neutral reader half: reduce, drop equal updates, store, and wake `main.rs`, shared
/// through [`StatePublisher::publish`].
pub struct StatePublisher {
    state: Arc<Mutex<WorkspacesState>>,
    events: UnboundedSender<WorkspacesSignal>,
    compositor: CompositorKind,
    previous: WorkspacesState,
}

impl StatePublisher {
    pub fn new(
        state: Arc<Mutex<WorkspacesState>>,
        events: UnboundedSender<WorkspacesSignal>,
        compositor: CompositorKind,
    ) -> Self {
        Self { state, events, compositor, previous: WorkspacesState::default() }
    }

    /// `false` once no one listens, ending the reader loop. Not debounced: startup replays are
    /// separate real events (niri sends workspaces and windows separately, so the first push has
    /// no known window).
    ///
    /// `special == None` means unsupported; `Some` including empty means supported (ADR-0119).
    /// Sort by name because the adaptor's list is wire-ordered.
    pub fn publish(
        &mut self,
        workspaces: &[WorkspaceRow],
        focused: Option<&FocusedWindow>,
        special: Option<&[SpecialWorkspace]>,
    ) -> bool {
        let mut current = derive_state(workspaces, focused);
        current.compositor = self.compositor.name().to_string();
        current.special = special.map(|list| {
            let mut list = list.to_vec();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list
        });
        if current == self.previous {
            return true;
        }
        *self.state.lock().expect("workspaces state mutex poisoned") = current.clone();
        self.previous = current;
        self.events.send(WorkspacesSignal::Changed).is_ok()
    }
}

/// `workspaces:focus(id)`'s `arguments: [id]`; only shape is checked. The compositor decides
/// whether the workspace exists and does nothing otherwise.
pub fn parse_focus_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `workspaces:toggle_special(name)`'s `arguments: [name]`, the `special` entry name. Hyprland
/// refuses unknown names or creates one, which also opens a scratchpad from a keybind.
pub fn parse_toggle_special_args(arguments: &[serde_json::Value]) -> Option<&str> {
    arguments.first()?.as_str().filter(|name| !name.is_empty())
}

/// No `Clone`: adaptors spawn OS threads and move only what they need because niri's socket is a
/// blocking `std::net::UnixStream`, not tokio-aware.
pub struct WorkspacesController {
    state: Arc<Mutex<WorkspacesState>>,
    compositor: Option<CompositorKind>,
}

impl WorkspacesController {
    /// Returns immediately without a reader when no compositor implements the session, so nothing
    /// pushes (ADR-0056 decision 1). Exhaustive matching makes a new `CompositorKind` fail here.
    pub fn new(events: UnboundedSender<WorkspacesSignal>) -> Self {
        let state = Arc::new(Mutex::new(WorkspacesState::default()));
        let compositor = detect_compositor();
        match compositor {
            Some(kind) => {
                let publisher = StatePublisher::new(Arc::clone(&state), events, kind);
                match kind {
                    CompositorKind::Niri => niri::spawn_reader(publisher),
                    CompositorKind::Hyprland => hyprland::spawn_reader(publisher),
                }
            }
            None => {
                eprintln!("workspaces: {}; workspace reporting disabled for this run", unsupported_session_report())
            }
        }
        Self { state, compositor }
    }

    pub fn snapshot(&self) -> WorkspacesState {
        self.state.lock().expect("workspaces state mutex poisoned").clone()
    }

    /// `workspaces:focus(id)`, routed to the live adaptor; exhaustive like [`Self::new`].
    pub fn focus(&self, id: u64) {
        match self.compositor {
            Some(CompositorKind::Niri) => niri::focus(id),
            Some(CompositorKind::Hyprland) => hyprland::focus(id),
            None => eprintln!("workspaces: focus({id}) called but this session has no workspace implementor; ignored"),
        }
    }

    /// `workspaces:toggle_special(name)`. Only Hyprland has specials; niri lacks the `special` key
    /// so configs can feature-test it.
    pub fn toggle_special(&self, name: &str) {
        match self.compositor {
            Some(CompositorKind::Hyprland) => hyprland::toggle_special(name),
            Some(CompositorKind::Niri) | None => {
                eprintln!(
                    "workspaces: toggle_special({name:?}) called but this session's compositor has no special workspaces; ignored"
                )
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
        FocusedWindow { title: title.to_string(), app_id: app_id.to_string(), is_floating, is_fullscreen: None }
    }

    // ---- derive_state: grouping and ordering ----

    #[test]
    fn derive_state_groups_by_output_and_orders_outputs_and_workspaces_deterministically() {
        // Deliberately out of order: adaptors fold maps, whose iteration is unordered.
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
        // `id` and `idx` disagree: reorder moves `idx` but not `id`, catching swapped fields.
        let state = derive_state(&[workspace(42, 1, "eDP-1", true, true)], None);

        assert_eq!(state.outputs[0].active_workspace, 42);
        assert_eq!(state.outputs[0].workspaces[0].idx, 1);
    }

    #[test]
    fn derive_state_ignores_a_workspace_that_has_no_output() {
        // Real: a compositor reports no output when none are connected.
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
        // Real: focusing a layer-shell surface leaves every toplevel unfocused.
        assert_eq!(derive_state(&[workspace(1, 1, "eDP-1", true, true)], None).active_client, None);
    }

    #[test]
    fn active_client_carries_is_fullscreen_only_when_the_compositor_said() {
        // ADR-0056 decision 5 keeps the key absent rather than fabricating `false`; ADR-0119 lets
        // a compositor that knows provide it. Absent, not `null`, otherwise.
        let json = serde_json::to_value(derive_state(&[], Some(&window("a title", "kitty", false)))).unwrap();
        let client = &json["active_client"];
        assert_eq!(client["title"], "a title");
        assert!(client.get("is_fullscreen").is_none());

        let mut known = window("a title", "mpv", false);
        known.is_fullscreen = Some(true);
        let json = serde_json::to_value(derive_state(&[], Some(&known))).unwrap();
        assert_eq!(json["active_client"]["is_fullscreen"], true);
    }

    #[test]
    fn a_state_with_nothing_focused_omits_active_client_rather_than_nulling_it() {
        let json = serde_json::to_value(WorkspacesState::default()).unwrap();

        assert!(json.get("active_client").is_none());
        assert!(json.get("special").is_none());
        assert_eq!(json["outputs"], serde_json::json!([]));
    }

    // ---- StatePublisher ----

    fn publisher() -> (StatePublisher, tokio::sync::mpsc::UnboundedReceiver<WorkspacesSignal>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StatePublisher::new(Arc::new(Mutex::new(WorkspacesState::default())), tx, CompositorKind::Niri), rx)
    }

    fn special(name: &str, shown_on: Option<&str>) -> SpecialWorkspace {
        SpecialWorkspace {
            name: name.to_string(),
            populated: true,
            app_id: None,
            shown_on: shown_on.map(str::to_string),
        }
    }

    #[test]
    fn publish_stores_the_state_and_signals_once_per_real_change() {
        let (mut publisher, mut rx) = publisher();
        let workspaces = [workspace(5, 1, "eDP-1", true, true)];

        assert!(publisher.publish(&workspaces, None, None));
        assert!(publisher.publish(&workspaces, None, None), "an event that changes nothing is not a change");
        assert!(publisher.publish(&workspaces, Some(&window("a title", "kitty", false)), None));

        assert_eq!(publisher.state.lock().unwrap().active_client.as_ref().unwrap().class, "kitty");
        let signals = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(signals, 2, "the repeated middle publish must not wake main.rs");
    }

    #[test]
    fn publish_reports_false_once_nothing_is_listening_so_a_reader_loop_can_stop() {
        let (mut publisher, rx) = publisher();
        drop(rx);

        assert!(!publisher.publish(&[workspace(5, 1, "eDP-1", true, true)], None, None));
    }

    #[test]
    fn publish_stamps_the_compositor_and_sorts_specials_and_keeps_the_key_out_when_there_are_none_to_have() {
        let (mut publisher, _rx) = publisher();
        let workspaces = [workspace(5, 1, "eDP-1", true, true)];

        assert!(publisher.publish(&workspaces, None, None));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["compositor"], "niri");
        assert!(json.get("special").is_none(), "no key at all: `special == nil` is the feature test");

        assert!(publisher.publish(
            &workspaces,
            None,
            Some(&[special("special:term", Some("eDP-1")), special("special", None)])
        ));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["special"][0]["name"], "special");
        assert_eq!(json["special"][1]["name"], "special:term");
        assert_eq!(json["special"][1]["shown_on"], "eDP-1");
        assert!(json["special"][0].get("shown_on").is_none());

        assert!(publisher.publish(&workspaces, None, Some(&[])));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["special"], serde_json::json!([]), "the compositor has specials and none exist right now");
    }

    #[test]
    fn parse_toggle_special_args_takes_a_non_empty_name() {
        assert_eq!(parse_toggle_special_args(&[serde_json::json!("special:term")]), Some("special:term"));
        assert_eq!(parse_toggle_special_args(&[serde_json::json!("")]), None);
        assert_eq!(parse_toggle_special_args(&[serde_json::json!(3)]), None);
        assert_eq!(parse_toggle_special_args(&[]), None);
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
