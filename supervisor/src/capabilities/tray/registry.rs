//! Per-item registry: hydration + signal-forwarder tasks that keep each tracked
//! `StatusNotifierItem`'s [`super::item::TrayItem`] snapshot live.
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::zvariant::OwnedObjectPath;

use crate::capabilities::shm_icons;

use super::TraySignal;
use super::icon::SPOOL_SUBDIR;
use super::item::{TrayItem, fetch_tray_item_base};
use super::menu::fetch_menu_via;
use super::proxies::{DBusMenuProxy, StatusNotifierItemProxy, bind_dbusmenu, bind_item};
use super::registration::ResolvedRegistration;

/// Process-wide sequence for [`ItemEntry::registered`]. It only needs to increase; each registry
/// still sees a monotonic order in tests.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

pub(super) struct ItemEntry {
    pub(super) item: StatusNotifierItemProxy<'static>,
    pub(super) menu: Option<DBusMenuProxy<'static>>,
    pub(super) last_known: TrayItem,
    /// First-registration sequence used by [`ordered_items`]. Without it, `HashMap` iteration made
    /// two pushes of the same set differ and an unrelated property update reshuffled the tray.
    registered: u64,
    properties_forwarder: JoinHandle<()>,
    menu_forwarder: Option<JoinHandle<()>>,
}

/// `tray.items` in registration order. Sorting D-Bus ids lexicographically puts `1.100` before
/// `1.20` and inserts a new app mid-strip; registration order also appends.
pub(super) fn ordered_items(registry: &ItemRegistry) -> Vec<TrayItem> {
    let guard = registry.lock().expect("tray registry mutex poisoned");
    let mut entries: Vec<&ItemEntry> = guard.values().collect();
    entries.sort_by_key(|entry| entry.registered);
    entries.into_iter().map(|entry| entry.last_known.clone()).collect()
}

pub(super) type ItemKey = (OwnedUniqueName, OwnedObjectPath);
pub(super) type ItemRegistry = Arc<Mutex<HashMap<ItemKey, ItemEntry>>>;

/// Binds and hydrates a `StatusNotifierItem` and its menu (ADR-0031 eager fetch), spawns
/// forwarders, and inserts it into `registry`. Replaces the same key and aborts its old forwarders.
pub(super) async fn register_item(
    connection: &zbus::Connection,
    registry: &ItemRegistry,
    events: &UnboundedSender<TraySignal>,
    resolved: ResolvedRegistration,
) -> Result<(), String> {
    let ResolvedRegistration { unique_name, destination, object_path } = resolved;
    let item = bind_item(connection, &destination, &object_path)
        .await
        .map_err(|err| format!("failed to bind StatusNotifierItem: {err}"))?;

    // An object that answers nothing is not an item. `bind_item` performs no I/O and
    // `fetch_tray_item_base` falls back to a default for every property, so without this probe a
    // path nobody exports still produced a blank `TrayItem` and got inserted.
    //
    // That is not hypothetical: startup adoption (ADR-0073) has no registration string to read a
    // path out of and can only guess `DEFAULT_ITEM_OBJECT_PATH`, while Chromium exports its item
    // one level down (ADR-0168). Slack was therefore registered twice from one connection -- the
    // phantom at the guessed path and the real one at `/StatusNotifierItem/1` -- and since
    // `TrayItem::id` is the unique name alone, both reached Lua as one id. A keyed `list` refused
    // the duplicate key and every re-resolve was dropped, freezing the surface.
    //
    // `Status` because SNI makes it mandatory and it is the one property whose absence is
    // unambiguous: a live item always answers it.
    if let Err(err) = item.status().await {
        return Err(format!("{destination} exports no StatusNotifierItem at {object_path}: {err}"));
    }

    let mut tray_item = fetch_tray_item_base(&item, &unique_name, &object_path).await;

    let menu_path = item.menu().await.ok();
    let menu = match &menu_path {
        Some(path) if !path.as_str().is_empty() && path.as_str() != "/" => {
            match bind_dbusmenu(connection, &destination, path).await {
                Ok(menu) => Some(menu),
                Err(err) => {
                    eprintln!("tray: failed to bind DBusMenu for {unique_name} at {path}: {err}");
                    None
                }
            }
        }
        _ => None,
    };
    if let Some(menu) = &menu {
        match fetch_menu_via(menu).await {
            Ok(items) => tray_item.menu = Some(items),
            Err(err) => eprintln!("tray: GetLayout failed for {unique_name}: {err}"),
        }
    }

    let key: ItemKey = (unique_name.clone(), object_path.clone());

    // TOCTOU guard: the property/GetLayout awaits can outlive the connection, while
    // NameOwnerChanged only removes entries that already exist. Check liveness immediately before
    // insertion, with no await after it. Best-effort failures proceed; this narrows, not
    // eliminates, the race.
    if let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(connection).await {
        match dbus_proxy.name_has_owner(BusName::from(unique_name.clone())).await {
            Ok(false) => return Err(format!("{unique_name} disconnected during registration")),
            Ok(true) => {}
            Err(err) => {
                eprintln!("tray: pre-insert liveness check for {unique_name} failed (proceeding anyway): {err}")
            }
        }
    }

    let properties_forwarder =
        spawn_item_signal_forwarder(item.clone(), unique_name.clone(), key.clone(), registry.clone(), events.clone());
    let menu_forwarder =
        menu.clone().map(|menu| spawn_menu_signal_forwarder(menu, key.clone(), registry.clone(), events.clone()));

    let mut entry =
        ItemEntry { item, menu, last_known: tray_item, registered: 0, properties_forwarder, menu_forwarder };
    let previous = {
        let mut guard = registry.lock().unwrap();
        entry.registered = match guard.get(&key) {
            // Same key means re-registration, so keep its place. A restart gets a new unique name
            // and key, so it is a new item.
            Some(existing) => existing.registered,
            None => NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed),
        };
        guard.insert(key, entry)
    };
    if let Some(previous) = previous {
        previous.properties_forwarder.abort();
        if let Some(handle) = previous.menu_forwarder {
            handle.abort();
        }
    }
    let _ = events.send(TraySignal::RegistryChanged);
    Ok(())
}

/// Stores refreshed SNI properties under the menu the entry already holds.
///
/// `fetch_tray_item_base` reads properties only and leaves `menu` unset, so the menu has to move
/// across; assigning `refreshed` on its own blanks the menu on every title or icon change.
fn keep_menu_across(entry: &mut ItemEntry, mut refreshed: TrayItem) {
    refreshed.menu = entry.last_known.menu.take();
    entry.last_known = refreshed;
}

/// Re-fetches the [`TrayItem`] properties on every `NewX` signal and updates the entry in place
/// without debounce or per-property patching. One task per item; its handle lives in
/// [`ItemEntry`] and is aborted on unregistration.
///
/// The menu is carried across rather than refetched: `spawn_menu_signal_forwarder` refreshes it on
/// `LayoutUpdated` and `controller::menu_will_show` refreshes it on open. Fetching it here too
/// would put a full `GetLayout` round trip behind every frame of an animated icon.
fn spawn_item_signal_forwarder(
    item: StatusNotifierItemProxy<'static>,
    unique_name: OwnedUniqueName,
    key: ItemKey,
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut new_title) = item.receive_new_title().await else { return };
        let Ok(mut new_icon) = item.receive_new_icon().await else { return };
        let Ok(mut new_attention_icon) = item.receive_new_attention_icon().await else { return };
        let Ok(mut new_overlay_icon) = item.receive_new_overlay_icon().await else { return };
        let Ok(mut new_tool_tip) = item.receive_new_tool_tip().await else { return };
        let Ok(mut new_status) = item.receive_new_status().await else { return };

        loop {
            let fired = tokio::select! {
                Some(_) = new_title.next() => true,
                Some(_) = new_icon.next() => true,
                Some(_) = new_attention_icon.next() => true,
                Some(_) = new_overlay_icon.next() => true,
                Some(_) = new_tool_tip.next() => true,
                Some(_) = new_status.next() => true,
                else => false,
            };
            if !fired {
                break;
            }

            let refreshed = fetch_tray_item_base(&item, &unique_name, &key.1).await;

            let mut guard = registry.lock().unwrap();
            let Some(entry) = guard.get_mut(&key) else { break };
            keep_menu_across(entry, refreshed);
            drop(guard);

            if events.send(TraySignal::RegistryChanged).is_err() {
                break;
            }
        }
    })
}

/// Refetches the full menu on each `LayoutUpdated` and updates `menu` in place (ADR-0031). One
/// task per menu-bearing item, aborted with the item task on unregistration.
fn spawn_menu_signal_forwarder(
    menu: DBusMenuProxy<'static>,
    key: ItemKey,
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut layout_updated) = menu.receive_layout_updated().await else { return };
        while layout_updated.next().await.is_some() {
            match fetch_menu_via(&menu).await {
                Ok(items) => {
                    let mut guard = registry.lock().unwrap();
                    let Some(entry) = guard.get_mut(&key) else { break };
                    entry.last_known.menu = Some(items);
                    drop(guard);
                    if events.send(TraySignal::RegistryChanged).is_err() {
                        break;
                    }
                }
                Err(err) => eprintln!("tray: GetLayout (LayoutUpdated refresh) failed: {err}"),
            }
        }
    })
}

/// Removes entries whose unique name drops off the bus (`new_owner` empty). SNI has no
/// `UnregisterStatusNotifierItem` signal, so this one global subscription supplies liveness
/// (ADR-0031).
/// `connection` is here only to emit `StatusNotifierItemUnregistered`. Registration announced
/// itself from the day it was written and departure never did, so another host on the bus kept
/// every item that ever left. Obelisk's own tray reads this registry rather than the signal, which
/// is why nothing here noticed.
pub(super) fn spawn_name_owner_changed_forwarder(
    connection: zbus::Connection,
    dbus_proxy: zbus::fdo::DBusProxy<'static>,
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut stream) = dbus_proxy.receive_name_owner_changed().await else { return };
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.new_owner.is_some() {
                continue;
            }
            let dropped_name = args.name.to_string();

            let removed: Vec<(ItemKey, ItemEntry)> = {
                let mut guard = registry.lock().unwrap();
                let stale_keys: Vec<ItemKey> =
                    guard.keys().filter(|(unique_name, _)| unique_name.as_str() == dropped_name).cloned().collect();
                stale_keys.into_iter().filter_map(|key| guard.remove(&key).map(|entry| (key, entry))).collect()
            };
            if removed.is_empty() {
                continue;
            }
            let mut departed = Vec::with_capacity(removed.len());
            for ((unique_name, object_path), entry) in removed {
                // The same `service + path` spelling `register_item` announces (ADR-0172).
                departed.push(format!("{}{}", unique_name.as_str(), object_path.as_str()));
                entry.properties_forwarder.abort();
                if let Some(handle) = entry.menu_forwarder {
                    handle.abort();
                }
                // Remove all three variant files (ADR-0074). Reconnecting apps get a new unique
                // name, so stale PNGs otherwise pile up for the rest of the session (logind clears
                // $XDG_RUNTIME_DIR only when the user's last session ends).
                for path in [
                    &entry.last_known.icon_path,
                    &entry.last_known.attention_icon_path,
                    &entry.last_known.overlay_icon_path,
                ]
                .into_iter()
                .flatten()
                {
                    shm_icons::remove_png(SPOOL_SUBDIR, path);
                }
            }
            if events.send(TraySignal::RegistryChanged).is_err() {
                break;
            }
            // Emitted last, and never between the cleanup steps: a stalled D-Bus write would
            // otherwise hold up aborting the forwarders, reaping the spooled PNGs and telling our
            // own config the registry moved. Nothing here depends on the signal landing.
            if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&connection, super::WATCHER_OBJECT_PATH) {
                for id in departed {
                    let _ =
                        super::watcher::StatusNotifierWatcher::status_notifier_item_unregistered(&emitter, &id).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;
    use crate::capabilities::tray::menu::MenuItem;

    /// Minimal entry for ordering tests. A p2p proxy bind makes no call, so no answering peer is
    /// needed.
    async fn entry(connection: &zbus::Connection, id: &str, registered: u64) -> ItemEntry {
        let destination = zbus::names::OwnedBusName::try_from("org.example.Item").expect("a valid bus name");
        let path = OwnedObjectPath::try_from("/StatusNotifierItem").expect("a valid object path");
        ItemEntry {
            item: bind_item(connection, &destination, &path).await.expect("binding makes no call"),
            menu: None,
            last_known: TrayItem { id: id.to_string(), ..TrayItem::default() },
            registered,
            properties_forwarder: tokio::spawn(std::future::ready(())),
            menu_forwarder: None,
        }
    }

    fn key(unique: &str) -> ItemKey {
        (
            OwnedUniqueName::try_from(unique).expect("a valid unique name"),
            OwnedObjectPath::try_from("/StatusNotifierItem").expect("a valid object path"),
        )
    }

    /// `HashMap` iteration is process-seeded; without registration order, an unrelated update
    /// reshuffled the tray.
    #[tokio::test]
    async fn the_strip_is_in_registration_order_whatever_the_map_says() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Build entries before locking; the helper awaits and a std mutex guard must not cross it.
        let third = entry(&connection, "third", 2).await;
        let first = entry(&connection, "first", 0).await;
        let second = entry(&connection, "second", 1).await;
        // Insert out of registration order; a `HashMap` may return that order.
        {
            let mut guard = registry.lock().unwrap();
            guard.insert(key(":1.30"), third);
            guard.insert(key(":1.10"), first);
            guard.insert(key(":1.20"), second);
        }
        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["first", "second", "third"]);
    }

    /// Registration order, not lexicographic id order.
    #[tokio::test]
    async fn a_later_registration_appends_even_when_its_id_sorts_first() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let older = entry(&connection, "1.9", 0).await;
        let newer = entry(&connection, "1.100", 1).await;
        {
            let mut guard = registry.lock().unwrap();
            guard.insert(key(":1.9"), older);
            guard.insert(key(":1.100"), newer);
        }
        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["1.9", "1.100"], "a lexicographic sort would put 1.100 first");
    }

    /// A property signal carries no menu, so assigning the refreshed item on its own would blank a
    /// menu that only `LayoutUpdated` and opening the menu ever refill.
    #[tokio::test]
    async fn refreshing_properties_keeps_the_menu_the_entry_already_has() {
        let (connection, _peer) = p2p_pair().await;
        let mut entry = entry(&connection, "item", 0).await;
        entry.last_known.menu = Some(vec![MenuItem { label: Some("Quit".to_string()), ..MenuItem::default() }]);

        keep_menu_across(
            &mut entry,
            TrayItem { id: "item".to_string(), name: "renamed".to_string(), ..TrayItem::default() },
        );

        assert_eq!(entry.last_known.name, "renamed", "the refreshed properties must land");
        let menu = entry.last_known.menu.as_ref().expect("a property refresh must not blank the menu");
        assert_eq!(menu[0].label.as_deref(), Some("Quit"));
    }

    /// In-place updates must not move an item.
    #[tokio::test]
    async fn an_item_that_re_registers_holds_its_place() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let first = entry(&connection, "first", 0).await;
        let second = entry(&connection, "second", 1).await;
        {
            let mut guard = registry.lock().unwrap();
            guard.insert(key(":1.10"), first);
            guard.insert(key(":1.20"), second);
        }
        let mut replacement = entry(&connection, "first-again", 999).await;
        // Repeat registration at a live key keeps the old sequence.
        replacement.registered = registry.lock().unwrap().get(&key(":1.10")).expect("just inserted").registered;
        registry.lock().unwrap().insert(key(":1.10"), replacement);

        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["first-again", "second"], "a re-registered item must not jump to the end");
    }
}
