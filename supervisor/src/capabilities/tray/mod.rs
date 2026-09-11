//! System tray host (`obelisk.tray`, docs/services.md §2;
//! docs/lua-api.md §2.14; ADR-0031).
//!
//! Hosts `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` and handles registered
//! `org.kde.StatusNotifierItem`s and optional `com.canonical.dbusmenu` menus. Hand-written proxies
//! avoid `system-tray`'s source-verified pixmap-squaring bug (ADR-0031); a keyed registry and one
//! abortable forwarder per item keep snapshots live.
//!
//! SNI has no unregister signal. `NameOwnerChanged` supplies liveness; one global forwarder removes
//! every entry for a unique name when it drops off the bus.
//!
//! ponytail: `TrayController::new` never fails outright (ADR-0030). No other tray host, common on
//! niri/sway, is not an error; losing `RequestName` to Plasma/GNOME is expected (ADR-0031).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use item::TrayItem;

/// Well-known bus name and object path for `org.kde.StatusNotifierWatcher` (§2).
const WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";
/// Fixed item path when `RegisterStatusNotifierItem`'s `service` is a bus name (ADR-0031).
const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";
/// Maximum accepted ARGB pixmap dimension (§2.1, ADR-0031).
const MAX_PIXMAP_DIMENSION: i32 = 128;

/// Cap on every string an item supplies: `Title`, `Id`, `Status`, both `ToolTip` halves, the three
/// `IconName`s, and each DBusMenu node's `label`, `type`, `icon-name` and `toggle-type`.
///
/// SNI and DBusMenu bound none of these, and a tray item is any application on the session bus, so
/// without a cap one `GetLayout` reply or one `Title` change sizes an allocation in a Supervisor
/// that never restarts. `notifications` already caps all seven of its own client-supplied fields
/// (`MAX_SUMMARY_BYTES` and neighbours); this is the same rule for the same reason, one number
/// because tray strings are all short display text rather than seven distinct kinds.
///
/// Truncating rather than dropping is safe even for the identifiers: an `IconName` cut short names
/// no icon and draws nothing, which is what dropping it would do anyway.
const MAX_TRAY_TEXT_BYTES: usize = 256;

/// Total [`menu::MenuItem`]s one `GetLayout` reply may produce.
///
/// `menu::MAX_MENU_DEPTH` bounds nesting but not breadth, so a single level of a million
/// siblings is within it. Real menus hold tens of entries; a thousand is already far past any
/// application's intent, and past it the reply is truncated rather than allocated.
const MAX_MENU_NODES: usize = 1024;

/// `IconPixmap`'s D-Bus wire shape (`a(iiay)`): width, height, raw ARGB32 bytes.
type RawIconPixmap = (i32, i32, Vec<u8>);
/// `ToolTip`'s D-Bus wire shape (`(sa(iiay)ss)`): icon name, icon pixmaps, title, text.
type RawToolTip = (String, Vec<RawIconPixmap>, String, String);

pub mod controller;
pub mod icon;
pub mod item;
pub mod menu;
pub mod proxies;
pub mod registration;
pub mod registry;
pub mod watcher;

pub use controller::TrayController;

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TrayState {
    /// Registered items, oldest first. New items append; property updates do not move them, so no
    /// sorting is needed. Registration order avoids lexicographic D-Bus id order, where `1.100`
    /// precedes `1.20` and a new app lands mid-strip.
    pub items: Vec<TrayItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraySignal {
    RegistryChanged,
}

#[derive(Debug)]
enum TrayActionError {
    UnknownItem,
    NoMenu,
}

impl std::fmt::Display for TrayActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownItem => write!(f, "no tray item with that id has been registered"),
            Self::NoMenu => write!(f, "that tray item has no registered dbusmenu"),
        }
    }
}

/// `tray:activate` gate (ADR-0031): `ItemIsMenu == true` means show the menu, not `Activate`,
/// enforced here once and centrally rather than trusted to every `shell.lua` author.
fn should_call_activate(item_is_menu: bool) -> bool {
    !item_is_menu
}

fn unix_timestamp_u32() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// `tray:activate(id, x, y)`'s `arguments: [id, x, y]`.
pub fn parse_activate_args(arguments: &[serde_json::Value]) -> Option<(String, i32, i32)> {
    let id = arguments.first()?.as_str()?.to_string();
    let x = arguments.get(1)?.as_i64()? as i32;
    let y = arguments.get(2)?.as_i64()? as i32;
    Some((id, x, y))
}

/// `tray:scroll(id, delta, orientation)`'s `[id, delta, orientation]` (ADR-0074). Separate from
/// [`parse_activate_args`] because the third argument is a string, not a coordinate.
pub fn parse_scroll_args(arguments: &[serde_json::Value]) -> Option<(String, i32, String)> {
    let id = arguments.first()?.as_str()?.to_string();
    let delta = arguments.get(1)?.as_i64()? as i32;
    let orientation = arguments.get(2)?.as_str()?.to_string();
    Some((id, delta, orientation))
}

/// `tray:activate_menu_item(id, menu_item_id)`'s `arguments: [id, menu_item_id]`.
/// Also `tray:menu_will_show(id, submenu_id)`: the wire shape is the same `[id, i32]` pair.
pub fn parse_activate_menu_item_args(arguments: &[serde_json::Value]) -> Option<(String, i32)> {
    let id = arguments.first()?.as_str()?.to_string();
    let menu_item_id = arguments.get(1)?.as_i64()? as i32;
    Some((id, menu_item_id))
}

/// Actions accepted by `obelisk.tray:invoke(...)`; `dispatch` keeps the table compiler-checked.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrayAction {
    Activate,
    SecondaryActivate,
    Scroll,
    ActivateMenuItem,
    MenuWillShow,
}

/// `obelisk.tray` dispatch (ADR-0037): matches, parses, and spawns every write action
/// (ADR-0031).
pub fn dispatch(controller: &TrayController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<TrayAction>(params) else { return };
    match action {
        TrayAction::Activate => match parse_activate_args(&params.arguments) {
            Some((id, x, y)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.activate(&id, x, y).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        // Same `[id, x, y]` shape as `Activate` (ADR-0074).
        TrayAction::SecondaryActivate => match parse_activate_args(&params.arguments) {
            Some((id, x, y)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.secondary_activate(&id, x, y).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        TrayAction::Scroll => match parse_scroll_args(&params.arguments) {
            Some((id, delta, orientation)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.scroll(&id, delta, &orientation).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        TrayAction::ActivateMenuItem => match parse_activate_menu_item_args(&params.arguments) {
            Some((id, menu_item_id)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.activate_menu_item(&id, menu_item_id).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        TrayAction::MenuWillShow => match parse_activate_menu_item_args(&params.arguments) {
            Some((id, submenu_id)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.menu_will_show(&id, submenu_id).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- should_call_activate ----

    #[test]
    fn should_call_activate_is_true_when_item_is_not_a_menu() {
        assert!(should_call_activate(false));
    }

    #[test]
    fn should_call_activate_is_false_when_item_is_a_menu() {
        assert!(!should_call_activate(true));
    }

    // ---- arg parsers ----

    #[test]
    fn parse_scroll_args_reads_id_delta_orientation() {
        let args = vec![serde_json::json!("1.42"), serde_json::json!(-120), serde_json::json!("vertical")];
        assert_eq!(parse_scroll_args(&args), Some(("1.42".to_string(), -120, "vertical".to_string())));
    }

    #[test]
    fn parse_scroll_args_refuses_a_numeric_orientation() {
        // The orientation is a string; the coordinate parser would drop valid scrolls.
        let args = vec![serde_json::json!("1.42"), serde_json::json!(-120), serde_json::json!(3)];
        assert_eq!(parse_scroll_args(&args), None);
    }

    #[test]
    fn every_tray_action_the_idl_names_parses() {
        // `secondary_activate` and `scroll` are new (ADR-0074); this checks their serde spellings.
        for name in ["activate", "secondary_activate", "scroll", "activate_menu_item", "menu_will_show"] {
            let action: Result<TrayAction, _> = serde_json::from_value(serde_json::json!(name));
            assert!(action.is_ok(), "{name} must deserialize into a TrayAction");
        }
    }

    #[test]
    fn parse_activate_args_reads_id_x_y() {
        assert_eq!(
            parse_activate_args(&[serde_json::json!("1.42"), serde_json::json!(10), serde_json::json!(20)]),
            Some(("1.42".to_string(), 10, 20))
        );
    }

    #[test]
    fn parse_activate_args_rejects_a_malformed_shape() {
        assert_eq!(parse_activate_args(&[]), None, "missing every element");
        assert_eq!(
            parse_activate_args(&[serde_json::json!(1), serde_json::json!(10), serde_json::json!(20)]),
            None,
            "id is not a string"
        );
        assert_eq!(
            parse_activate_args(&[serde_json::json!("1.42"), serde_json::json!("x"), serde_json::json!(20)]),
            None,
            "x is not a number"
        );
    }

    #[test]
    fn parse_activate_menu_item_args_reads_id_and_menu_item_id() {
        assert_eq!(
            parse_activate_menu_item_args(&[serde_json::json!("1.42"), serde_json::json!(7)]),
            Some(("1.42".to_string(), 7))
        );
    }

    #[test]
    fn parse_activate_menu_item_args_rejects_a_malformed_shape() {
        assert_eq!(parse_activate_menu_item_args(&[]), None);
        assert_eq!(parse_activate_menu_item_args(&[serde_json::json!("1.42")]), None, "missing menu_item_id");
    }

    #[test]
    fn parse_activate_menu_item_args_reads_id_and_submenu_id() {
        assert_eq!(
            parse_activate_menu_item_args(&[serde_json::json!("1.42"), serde_json::json!(3)]),
            Some(("1.42".to_string(), 3))
        );
    }
}
