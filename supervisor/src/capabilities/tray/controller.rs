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
use super::registry::{ItemKey, ItemRegistry, register_item, spawn_name_owner_changed_forwarder};
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
    /// Requests `org.kde.StatusNotifierWatcher` with no `ReplaceExisting`/`DoNotQueue` flags
    /// (ADR-0031's "dual-role dance"): with `DoNotQueue` unset, zbus never returns
    /// `Err(NameTaken)` for this case -- it queues instead, returning `Ok(InQueue)`. So every
    /// `Ok` reply here is a real success path; only a hard `Err` is logged as a genuine
    /// failure, and even that doesn't stop construction.
    ///
    /// The `StatusNotifierWatcher` object is attached at [`WATCHER_OBJECT_PATH`] regardless
    /// of who owns the name, then `RegisterStatusNotifierHost` is called against the
    /// well-known name (not a resolved unique name) -- D-Bus routing delivers that call to
    /// whichever process actually owns it.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<TraySignal>) -> Self {
        match connection.request_name_with_flags(WATCHER_BUS_NAME, BitFlags::<RequestNameFlags>::empty()).await {
            Ok(reply) => eprintln!("tray: RequestName({WATCHER_BUS_NAME}) -> {reply}"),
            Err(err) => eprintln!("tray: RequestName({WATCHER_BUS_NAME}) failed: {err}"),
        }

        // Before anything can spool: a fresh Supervisor owns nothing in there, so whatever is left
        // is a previous run's and nothing will ever delete it otherwise (docs/adr/0074).
        crate::capabilities::shm_icons::sweep(super::icon::SPOOL_SUBDIR);

        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let host_registered = Arc::new(Mutex::new(false));
        let watcher = StatusNotifierWatcher {
            connection: connection.clone(),
            registry: registry.clone(),
            host_registered,
            events: events.clone(),
        };
        // Logged-and-continue, not `?`-propagated: an export failure here must not abort the
        // whole Supervisor -- this controller still constructs either way.
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

    /// Fully inert controller: empty registry, no forwarder tasks, nothing exported. Used
    /// when a dedicated session-bus connection for the tray host couldn't be established.
    /// Every read/write action behaves as it would against a live controller with no items
    /// registered yet.
    pub fn inert(events: UnboundedSender<TraySignal>) -> Self {
        Self { registry: Arc::new(Mutex::new(HashMap::new())), events }
    }

    /// Full, live re-derivation of `tray.items` from the entire tracked registry.
    /// Synchronous: every registry entry's `last_known` is already up to date (the forwarder
    /// tasks recompute it before ever sending a [`TraySignal`]).
    pub fn build_state(&self) -> TrayState {
        TrayState { items: self.registry.lock().unwrap().values().map(|entry| entry.last_known.clone()).collect() }
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

    /// `tray:activate(id, x, y)`. No-ops (does not call the real `Activate`) when the item's
    /// `ItemIsMenu` is `true` -- SNI's own documented semantics, enforced centrally
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

    /// `tray:secondary_activate(id, x, y)`: § 2.5's middle-click (docs/adr/0074).
    ///
    /// No `should_call_activate` gate, unlike [`Self::activate`]: `ItemIsMenu` says a *primary*
    /// click must open the menu instead of activating, and says nothing about the secondary one.
    /// An application that wants nothing to happen exports a method that does nothing.
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

    /// `tray:scroll(id, delta, orientation)`: § 2.5's scroll over the icon (docs/adr/0074).
    ///
    /// `orientation` reaches the application verbatim. The spec names `"vertical"` and
    /// `"horizontal"` and this does not police it, because the string is the application's to
    /// interpret and refusing a third value here would only turn a shrug into a dropped command.
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

    /// `tray:activate_menu_item(id, menu_item_id)`: `DBusMenu.Event(menu_item_id, "clicked",
    /// &Value::I32(0), timestamp)` (ADR-0031).
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

    /// `tray:menu_will_show(id, submenu_id)`: calls `AboutToShow(submenu_id)` (DBusMenu's
    /// lazy-population signal, ADR-0031), then re-fetches and re-pushes the item's entire
    /// menu tree. A full re-fetch, not an in-place splice: menu trees are human-scale
    /// (ADR-0031), so the extra round trip costs nothing a user would notice.
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

/// Whether `name` is an item's own well-known bus name, the
/// `org.{kde,freedesktop}.StatusNotifierItem-PID-N` an application claims before it calls
/// `RegisterStatusNotifierItem` (docs/adr/0073).
///
/// Both spellings, because both are in the wild: KDE's is the de-facto one and Chromium claims the
/// `org.freedesktop` one. The trailing `-` is what keeps this off `org.kde.StatusNotifierWatcher`
/// and off any name that merely starts the same way.
fn is_item_bus_name(name: &str) -> bool {
    ["org.kde.StatusNotifierItem-", "org.freedesktop.StatusNotifierItem-"].iter().any(|prefix| name.starts_with(prefix))
}

/// Registers every tray item already on the bus when this host starts (docs/adr/0073).
///
/// The spec's answer to a host starting late is `StatusNotifierHostRegistered`, which this watcher
/// emits and which an application is meant to re-register on. Slack does not, so restarting the
/// shell lost its icon until Slack itself was restarted. This asks the bus rather than waiting to be
/// told.
///
/// Serial rather than joined: each `register_item` reads a handful of properties and an optional
/// `GetLayout`, and a session has a handful of tray items, so the whole scan is a few round trips
/// against a Supervisor that has already made several. A duplicate is harmless, since the registry
/// is keyed by `(unique_name, object_path)` and an application that does re-register overwrites its
/// own entry rather than adding a second.
///
/// ponytail: this finds only items that claimed a well-known name. One that called
/// `RegisterStatusNotifierItem("/some/object/path")` and owns no `StatusNotifierItem-*` name is
/// invisible to it, because nothing on the bus says which connections export the interface without
/// asking each one. Vesktop is that shape and does not need this, since it re-registers on the
/// signal. The upgrade path is introspecting every connection on the session bus, which is dozens of
/// round trips at startup to look for something usually not there.
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
        // No sender: `resolve_registration`'s well-known branch never reads one, and there is no
        // calling message here to take it from.
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
        // The name this Supervisor owns itself. Adopting it would have the host register its own
        // watcher as a tray icon.
        assert!(!is_item_bus_name("org.kde.StatusNotifierWatcher"));
        assert!(!is_item_bus_name("org.kde.StatusNotifierHost-1234"));
    }

    #[test]
    fn a_name_that_only_starts_the_same_way_is_not_an_item() {
        // The trailing `-` is the whole guard: without it every one of these matches.
        assert!(!is_item_bus_name("org.kde.StatusNotifierItemRegistry"));
        assert!(!is_item_bus_name("org.kde.StatusNotifierItem"));
        assert!(!is_item_bus_name("com.example.org.kde.StatusNotifierItem-1-1"));
    }
}
