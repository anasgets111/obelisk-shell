//! [`TrayController`]: the `oblisk.tray` write-action dispatcher and state owner.
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use enumflags2::BitFlags;
use tokio::sync::mpsc::UnboundedSender;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::Value;

use super::item::TrayItem;
use super::menu::fetch_menu_via;
use super::proxies::{DBusMenuProxy, StatusNotifierItemProxy, StatusNotifierWatcherClientProxy};
use super::registration::{resolve_registration, sanitize_unique_name};
use super::registry::{ItemKey, ItemRegistry, ordered_items, register_item, spawn_name_owner_changed_forwarder};
use super::watcher::StatusNotifierWatcher;
use super::{
    TrayActionError, TraySignal, TrayState, WATCHER_BUS_NAME, WATCHER_OBJECT_PATH, should_call_activate,
    unix_timestamp_u32,
};

#[derive(Clone)]
pub struct TrayController {
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
}

impl TrayController {
    /// Requests `org.kde.StatusNotifierWatcher` without `ReplaceExisting`/`DoNotQueue` flags
    /// (ADR-0031). With `DoNotQueue` unset, zbus queues and returns `Ok(InQueue)`, so only a hard
    /// `Err` is failure, and it does not stop construction.
    ///
    /// Exports [`WATCHER_OBJECT_PATH`] regardless of name ownership, then calls
    /// `RegisterStatusNotifierHost` at the well-known name so D-Bus routes to the actual owner.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<TraySignal>) -> Self {
        match connection.request_name_with_flags(WATCHER_BUS_NAME, BitFlags::<RequestNameFlags>::empty()).await {
            Ok(reply) => eprintln!("tray: RequestName({WATCHER_BUS_NAME}) -> {reply}"),
            Err(err) => eprintln!("tray: RequestName({WATCHER_BUS_NAME}) failed: {err}"),
        }

        // Sweep before spooling; leftovers belong to a previous run and otherwise survive
        // (ADR-0074).
        crate::capabilities::shm_icons::sweep(super::icon::SPOOL_SUBDIR);

        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let host_registered = Arc::new(Mutex::new(false));
        let watcher = StatusNotifierWatcher {
            connection: connection.clone(),
            registry: registry.clone(),
            host_registered,
            events: events.clone(),
        };
        // Continue after export failure; tray setup must not abort the Supervisor.
        if let Err(err) = connection.object_server().at(WATCHER_OBJECT_PATH, watcher).await {
            eprintln!("tray: failed to export StatusNotifierWatcher at {WATCHER_OBJECT_PATH}: {err}");
        }

        match StatusNotifierWatcherClientProxy::new(&connection).await {
            Ok(watcher_client) => {
                let our_unique_name = connection.unique_name().map(|name| name.to_string()).unwrap_or_default();
                if let Err(err) = watcher_client.register_status_notifier_host(&our_unique_name).await {
                    eprintln!("tray: RegisterStatusNotifierHost failed: {err}");
                }
            }
            Err(err) => eprintln!(
                "tray: failed to bind the StatusNotifierWatcher client proxy for RegisterStatusNotifierHost: {err}"
            ),
        }

        match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(dbus_proxy) => {
                adopt_existing_items(&connection, &dbus_proxy, &registry, &events).await;
                spawn_name_owner_changed_forwarder(dbus_proxy, registry.clone(), events.clone());
            }
            Err(err) => eprintln!("tray: failed to bind org.freedesktop.DBus for NameOwnerChanged tracking: {err}"),
        }

        Self { registry, events }
    }

    /// Empty registry, no forwarders, and nothing exported. Used when the tray session-bus
    /// connection cannot be established; actions behave like a live controller with no items.
    pub fn inert(events: UnboundedSender<TraySignal>) -> Self {
        Self { registry: Arc::new(Mutex::new(HashMap::new())), events }
    }

    /// Re-derives `tray.items` from the registry. Synchronous because forwarders update
    /// `last_known` before sending [`TraySignal`].
    pub fn build_state(&self) -> TrayState {
        TrayState { items: ordered_items(&self.registry) }
    }

    fn find_item_id(&self, id: &str) -> Option<(ItemKey, TrayItem)> {
        let guard = self.registry.lock().unwrap();
        guard
            .iter()
            .find(|(key, _)| sanitize_unique_name(key.0.as_str()) == id)
            .map(|(key, entry)| (key.clone(), entry.last_known.clone()))
    }

    fn find_item_proxy(&self, key: &ItemKey) -> Option<StatusNotifierItemProxy<'static>> {
        self.registry.lock().unwrap().get(key).map(|entry| entry.item.clone())
    }

    fn find_menu_proxy(&self, key: &ItemKey) -> Option<DBusMenuProxy<'static>> {
        self.registry.lock().unwrap().get(key).and_then(|entry| entry.menu.clone())
    }

    /// `tray:activate(id, x, y)`. Skips `Activate` when `ItemIsMenu` is true, per SNI semantics
    /// (ADR-0031, [`should_call_activate`]).
    pub async fn activate(&self, id: &str, x: i32, y: i32) {
        let Some((key, tray_item)) = self.find_item_id(id) else {
            eprintln!("tray: activate({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        if !should_call_activate(tray_item.item_is_menu) {
            return;
        }
        let Some(item) = self.find_item_proxy(&key) else {
            eprintln!("tray: activate({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        if let Err(err) = item.activate(x, y).await {
            eprintln!("tray: activate({id:?}) failed: {err}");
        }
    }

    /// `tray:secondary_activate(id, x, y)`: §2.5 middle-click (ADR-0074). No
    /// `should_call_activate` gate: `ItemIsMenu` constrains primary clicks only.
    pub async fn secondary_activate(&self, id: &str, x: i32, y: i32) {
        let Some((key, _)) = self.find_item_id(id) else {
            eprintln!("tray: secondary_activate({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        let Some(item) = self.find_item_proxy(&key) else {
            eprintln!("tray: secondary_activate({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        if let Err(err) = item.secondary_activate(x, y).await {
            eprintln!("tray: secondary_activate({id:?}) failed: {err}");
        }
    }

    /// `tray:scroll(id, delta, orientation)`: §2.5 icon scroll (ADR-0074). Passes `orientation`
    /// verbatim; the application interprets it, including values beyond the two named orientations.
    pub async fn scroll(&self, id: &str, delta: i32, orientation: &str) {
        let Some((key, _)) = self.find_item_id(id) else {
            eprintln!("tray: scroll({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        let Some(item) = self.find_item_proxy(&key) else {
            eprintln!("tray: scroll({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        if let Err(err) = item.scroll(delta, orientation).await {
            eprintln!("tray: scroll({id:?}) failed: {err}");
        }
    }

    /// `tray:activate_menu_item(id, menu_item_id)` sends `DBusMenu.Event(id, "clicked", 0,
    /// timestamp)` (ADR-0031).
    pub async fn activate_menu_item(&self, id: &str, menu_item_id: i32) {
        let Some((key, _)) = self.find_item_id(id) else {
            eprintln!("tray: activate_menu_item({id:?}, {menu_item_id}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        let Some(menu) = self.find_menu_proxy(&key) else {
            eprintln!("tray: activate_menu_item({id:?}, {menu_item_id}) failed: {}", TrayActionError::NoMenu);
            return;
        };
        let data = Value::I32(0);
        if let Err(err) = menu.event(menu_item_id, "clicked", &data, unix_timestamp_u32()).await {
            eprintln!("tray: activate_menu_item({id:?}, {menu_item_id}) failed: {err}");
        }
    }

    /// `tray:menu_will_show(id, submenu_id)` calls DBusMenu `AboutToShow(submenu_id)`, its
    /// lazy-population signal, then re-fetches and pushes the entire menu tree (ADR-0031). Full
    /// refetch is adequate for
    /// human-scale trees.
    pub async fn menu_will_show(&self, id: &str, submenu_id: i32) {
        let Some((key, _)) = self.find_item_id(id) else {
            eprintln!("tray: menu_will_show({id:?}, {submenu_id}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        let Some(menu) = self.find_menu_proxy(&key) else {
            eprintln!("tray: menu_will_show({id:?}, {submenu_id}) failed: {}", TrayActionError::NoMenu);
            return;
        };
        if let Err(err) = menu.about_to_show(submenu_id).await {
            eprintln!("tray: menu_will_show({id:?}, {submenu_id}) AboutToShow failed: {err}");
        }
        match fetch_menu_via(&menu).await {
            Ok(items) => {
                let mut guard = self.registry.lock().unwrap();
                if let Some(entry) = guard.get_mut(&key) {
                    entry.last_known.menu = Some(items);
                }
                drop(guard);
                let _ = self.events.send(TraySignal::RegistryChanged);
            }
            Err(err) => eprintln!("tray: menu_will_show({id:?}, {submenu_id}) GetLayout failed: {err}"),
        }
    }
}

/// Whether `name` is an item's well-known `org.{kde,freedesktop}.StatusNotifierItem-PID-N` name
/// claimed before `RegisterStatusNotifierItem` (ADR-0073).
///
/// Accepts both KDE and Chromium spellings. The trailing `-` excludes the watcher and names that
/// merely share the prefix.
fn is_item_bus_name(name: &str) -> bool {
    ["org.kde.StatusNotifierItem-", "org.freedesktop.StatusNotifierItem-"].iter().any(|prefix| name.starts_with(prefix))
}

/// Registers tray items already on the bus when this host starts (ADR-0073). The spec expects
/// clients to re-register after `StatusNotifierHostRegistered`, but Slack does not; without this
/// bus scan, restarting the shell lost Slack until Slack restarted.
///
/// Serial because each `register_item` reads properties and optional `GetLayout`, while a session
/// has only a handful of items. Duplicates are harmless: `(unique_name, object_path)` is the key,
/// so re-registration overwrites the entry.
///
/// ponytail: finds only items that claimed a well-known name. An item registering only
/// `RegisterStatusNotifierItem("/some/object/path")` is invisible without introspecting every
/// session-bus connection. Vesktop has that shape but re-registers on the signal. Upgrade path:
/// introspection, costing dozens of startup round trips for a rare case.
async fn adopt_existing_items(
    connection: &zbus::Connection,
    dbus_proxy: &zbus::fdo::DBusProxy<'_>,
    registry: &ItemRegistry,
    events: &UnboundedSender<TraySignal>,
) {
    let names = match dbus_proxy.list_names().await {
        Ok(names) => names,
        Err(err) => {
            eprintln!("tray: ListNames failed, so no already-running item is adopted this run: {err}");
            return;
        }
    };
    for name in names.iter().filter(|name| is_item_bus_name(name.as_str())) {
        // No sender: this well-known branch does not need one, and no call supplies it.
        let resolved = match resolve_registration(connection, name.as_str(), None).await {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("tray: {name} looks like an item but could not be resolved: {err}");
                continue;
            }
        };
        match register_item(connection, registry, events, resolved).await {
            Ok(()) => eprintln!("tray: adopted {name}, registered before this host started"),
            Err(err) => eprintln!("tray: failed to adopt {name}: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_name_is_recognized_in_both_spellings() {
        assert!(is_item_bus_name("org.kde.StatusNotifierItem-1240273-1"));
        assert!(is_item_bus_name("org.freedesktop.StatusNotifierItem-1240273-1"));
    }

    #[test]
    fn the_watcher_is_not_an_item() {
        // This Supervisor's own name; adopting it would register the watcher as an icon.
        assert!(!is_item_bus_name("org.kde.StatusNotifierWatcher"));
        assert!(!is_item_bus_name("org.kde.StatusNotifierHost-1234"));
    }

    #[test]
    fn a_name_that_only_starts_the_same_way_is_not_an_item() {
        // The trailing `-` excludes these prefix-only names.
        assert!(!is_item_bus_name("org.kde.StatusNotifierItemRegistry"));
        assert!(!is_item_bus_name("org.kde.StatusNotifierItem"));
        assert!(!is_item_bus_name("com.example.org.kde.StatusNotifierItem-1-1"));
    }
}
