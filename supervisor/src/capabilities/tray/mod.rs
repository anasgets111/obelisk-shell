//! System tray host (`oblisk.tray`, docs/oblisk-supervisor-services-dbus.md §2;
//! docs/oblisk-idl-api-specs.md §2.14; docs/adr/0031).
//!
//! Hosts `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` and client-handles every
//! registered `org.kde.StatusNotifierItem` (plus its optional `com.canonical.dbusmenu` menu).
//! Hand-written `#[zbus::proxy]` traits (ADR-0031: no crate reuse -- `system-tray` has a
//! source-verified pixmap-squaring bug), a `*Controller` struct holding the write-action
//! proxies, a `HashMap<Key, Entry>` dynamic per-object registry kept live via per-item
//! forwarder tasks (one `JoinHandle` per tracked object, aborted on removal), and pure
//! pretty-printable/parsing helpers unit-testable without a live D-Bus connection.
//!
//! The base SNI spec has no signal telling a host when a client unregisters -- liveness is
//! tracked via `org.freedesktop.DBus.NameOwnerChanged`: one global forwarder task removes
//! every registry entry for a unique name the instant that name drops off the bus (see
//! [`registry::spawn_name_owner_changed_forwarder`]).
//!
//! ponytail: `TrayController::new` never fails outright, same reasoning as
//! `BluetoothController::new` (docs/adr/0030) -- a session with no other tray host running (the
//! overwhelmingly common case for niri/sway) is not an error, and `RequestName` losing the race to
//! an already-running DE tray (Plasma/GNOME) is an expected, handled outcome (ADR-0031's "dual-role
//! dance"), not a startup failure either.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use item::TrayItem;

/// Well-known bus name and object path this controller hosts `org.kde.StatusNotifierWatcher` at
/// (docs/oblisk-supervisor-services-dbus.md §2's literal path).
const WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";
/// `RegisterStatusNotifierItem`'s `service` argument, when it names a bus name rather than an
/// object path, always means this fixed item object path (ADR-0031).
const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";
/// ARGB pixmaps are rejected above this size (docs/oblisk-supervisor-services-dbus.md §2.1,
/// ADR-0031: "largest available pixmap capped at the spec's 128px limit").
const MAX_PIXMAP_DIMENSION: i32 = 128;

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
    /// Every registered `StatusNotifierItem`, in no order at all: `build_state` collects a
    /// `HashMap`'s values, so the sequence can differ between two pushes that registered the same
    /// items. A strip that should not reshuffle has to sort, and [`TrayItem::id`] is the stable key.
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

impl std::error::Error for TrayActionError {}

/// `tray:activate(id, x, y)`'s click-vs-menu gate (ADR-0031): an item with `ItemIsMenu == true`
/// must show its menu instead of activating -- enforced here, once, centrally, rather than
/// trusted to every `shell.lua` author.
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

/// `tray:scroll(id, delta, orientation)`'s `arguments: [id, delta, orientation]` (docs/adr/0074).
///
/// Its own parser rather than a reuse of [`parse_activate_args`]: the shapes look alike, but the
/// third argument is a string here and reusing the coordinate parser would silently drop every
/// scroll whose orientation was spelled correctly.
pub fn parse_scroll_args(arguments: &[serde_json::Value]) -> Option<(String, i32, String)> {
    let id = arguments.first()?.as_str()?.to_string();
    let delta = arguments.get(1)?.as_i64()? as i32;
    let orientation = arguments.get(2)?.as_str()?.to_string();
    Some((id, delta, orientation))
}

/// `tray:activate_menu_item(id, menu_item_id)`'s `arguments: [id, menu_item_id]`.
pub fn parse_activate_menu_item_args(arguments: &[serde_json::Value]) -> Option<(String, i32)> {
    let id = arguments.first()?.as_str()?.to_string();
    let menu_item_id = arguments.get(1)?.as_i64()? as i32;
    Some((id, menu_item_id))
}

/// `tray:menu_will_show(id, submenu_id)`'s `arguments: [id, submenu_id]` -- same shape as
/// [`parse_activate_menu_item_args`], kept as a distinct function so each write action's
/// parser matches its own command name at the call site.
pub fn parse_menu_will_show_args(arguments: &[serde_json::Value]) -> Option<(String, i32)> {
    parse_activate_menu_item_args(arguments)
}

/// Every action `oblisk.tray:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrayAction {
    Activate,
    SecondaryActivate,
    Scroll,
    ActivateMenuItem,
    MenuWillShow,
}

/// `oblisk.tray`'s action dispatch (ADR-0037): owns the action match, argument parse, and
/// write-action spawn for every `tray` `CommandEnvelope`. Write actions are `tokio::spawn`ed
/// rather than awaited inline (ADR-0031).
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
        // Same `[id, x, y]` shape as `Activate`, so the same parser (docs/adr/0074).
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
        TrayAction::MenuWillShow => match parse_menu_will_show_args(&params.arguments) {
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

/// Shared by every submodule's own `#[cfg(test)]` -- see [`p2p_pair`] for why this lives here
/// instead of being copied into each one.
#[cfg(test)]
mod test_support {
    use tokio::net::UnixStream;

    /// A connected pair of p2p zbus connections, no bus daemon involved, with one addition:
    /// the server side gets a short `method_timeout`. `register_item`'s real code path calls
    /// out from this side to a peer that never registers any object server handler for some
    /// calls -- with zbus's default timeout, each would hang until it lapses instead of
    /// erroring quickly, and `register_item` awaits several sequentially.
    pub(super) async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder = zbus::connection::Builder::unix_stream(a)
            .server(guid)
            .expect("p2p server builder setup")
            .p2p()
            .method_timeout(std::time::Duration::from_millis(200));
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
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
        // The reason this is not `parse_activate_args`: the shapes look alike, and reusing that one
        // would read the orientation as a coordinate and drop every correctly spelled scroll.
        let args = vec![serde_json::json!("1.42"), serde_json::json!(-120), serde_json::json!(3)];
        assert_eq!(parse_scroll_args(&args), None);
    }

    #[test]
    fn every_tray_action_the_idl_names_parses() {
        // `secondary_activate` and `scroll` are new (docs/adr/0074); serde owns the mapping, so a
        // rename that misses the stub fails here rather than at a config author's keyboard.
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
    fn parse_menu_will_show_args_reads_id_and_submenu_id() {
        assert_eq!(
            parse_menu_will_show_args(&[serde_json::json!("1.42"), serde_json::json!(3)]),
            Some(("1.42".to_string(), 3))
        );
    }
}
