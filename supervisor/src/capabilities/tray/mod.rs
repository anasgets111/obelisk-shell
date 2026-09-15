//! System tray host (`obelisk.tray`, ADR-0031).
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

/// Well-known bus name and object path for `org.kde.StatusNotifierWatcher`.
const WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";
/// Fixed item path when `RegisterStatusNotifierItem`'s `service` is a bus name (ADR-0031).
const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";
/// Maximum accepted ARGB pixmap dimension (ADR-0031).
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

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
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

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TrayAction {
    /// Left-click activation at screen coordinates.
    Activate { id: String, x: i32, y: i32 },
    /// Middle-click activation at screen coordinates (ADR-0074).
    SecondaryActivate { id: String, x: i32, y: i32 },
    /// Passes `"vertical"` or `"horizontal"` verbatim (ADR-0074).
    Scroll { id: String, delta: i32, orientation: String },
    /// Clicks a `MenuItem.id`.
    ActivateMenuItem { id: String, menu_item_id: i32 },
    /// Tells the application a submenu is opening.
    MenuWillShow { id: String, submenu_id: i32 },
}

/// `obelisk.tray` dispatch (ADR-0037): spawns every write action (ADR-0031).
pub fn dispatch(controller: &TrayController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<TrayAction>(&envelope.params) else { return };
    let controller = controller.clone();
    tokio::spawn(async move {
        match action {
            TrayAction::Activate { id, x, y } => controller.activate(&id, x, y).await,
            TrayAction::SecondaryActivate { id, x, y } => controller.secondary_activate(&id, x, y).await,
            TrayAction::Scroll { id, delta, orientation } => controller.scroll(&id, delta, &orientation).await,
            TrayAction::ActivateMenuItem { id, menu_item_id } => controller.activate_menu_item(&id, menu_item_id).await,
            TrayAction::MenuWillShow { id, submenu_id } => controller.menu_will_show(&id, submenu_id).await,
        }
    });
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
}
