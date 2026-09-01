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

/// Hands out [`ItemEntry::registered`]. Process-wide rather than per-registry, because it only has
/// to increase and one `TrayController` exists per session; two registries in one test process
/// still each see a monotonic sequence, which is all the sort needs.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

pub(super) struct ItemEntry {
    pub(super) item: StatusNotifierItemProxy<'static>,
    pub(super) menu: Option<DBusMenuProxy<'static>>,
    pub(super) last_known: TrayItem,
    /// When this item first registered, and the order [`ordered_items`] puts the strip in.
    ///
    /// The registry is a `HashMap`, so before this existed `tray.items` came out in whatever order
    /// the hash seed produced: a different sequence between two pushes over the same set, which
    /// reshuffled a bar's tray icons every time one unrelated item updated a property.
    registered: u64,
    properties_forwarder: JoinHandle<()>,
    menu_forwarder: Option<JoinHandle<()>>,
}

/// `tray.items`, oldest registration first.
///
/// Registration order rather than a sort on [`TrayItem::id`]: the id is a D-Bus unique name like
/// `"1.234"`, so sorting it lexicographically puts `1.100` before `1.20` and drops a newly started
/// app into the middle of the strip. Both orders are stable; only this one also appends.
pub(super) fn ordered_items(registry: &ItemRegistry) -> Vec<TrayItem> {
    let guard = registry.lock().expect("tray registry mutex poisoned");
    let mut entries: Vec<&ItemEntry> = guard.values().collect();
    entries.sort_by_key(|entry| entry.registered);
    entries.into_iter().map(|entry| entry.last_known.clone()).collect()
}

pub(super) type ItemKey = (OwnedUniqueName, OwnedObjectPath);
pub(super) type ItemRegistry = Arc<Mutex<HashMap<ItemKey, ItemEntry>>>;

/// Binds `unique_name`/`object_path` as a `StatusNotifierItem`, hydrates its full [`TrayItem`]
/// (including its menu tree, if it has one -- ADR-0031's "eager top-level fetch"), spawns its
/// signal forwarder(s), and inserts the resulting entry into `registry`. Aborts and replaces
/// a prior entry at the same key rather than leaking its forwarder tasks.
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

    let mut tray_item = fetch_tray_item_base(&item, &unique_name).await;

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

    // Narrow TOCTOU guard: every `.await` above (property reads, an optional GetLayout) is a
    // window in which the registering connection could have disconnected --
    // NameOwnerChanged-based cleanup only ever removes an entry that already exists, so a
    // disconnect landing in that window would otherwise plant an unreachable ghost entry no
    // later signal can ever remove. One more liveness check right here, before the insert
    // below (nothing else `.await`s between this and it), narrows that whole multi-await
    // window down to a single check-then-insert. Best-effort: a failure to even ask proceeds
    // with the insert rather than blocking a legitimate registration on an unrelated D-Bus
    // hiccup -- this narrows the race, it doesn't need to be perfect.
    if let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(connection).await {
        match dbus_proxy.name_has_owner(BusName::from(unique_name.clone())).await {
            Ok(false) => {
                eprintln!("tray: {unique_name} disconnected during registration; not inserting a registry entry");
                return Ok(());
            }
            Ok(true) => {}
            Err(err) => {
                eprintln!("tray: pre-insert liveness check for {unique_name} failed (proceeding anyway): {err}")
            }
        }
    }

    let properties_forwarder = spawn_item_signal_forwarder(
        item.clone(),
        unique_name.clone(),
        menu.clone(),
        key.clone(),
        registry.clone(),
        events.clone(),
    );
    let menu_forwarder =
        menu.clone().map(|menu| spawn_menu_signal_forwarder(menu, key.clone(), registry.clone(), events.clone()));

    let mut entry =
        ItemEntry { item, menu, last_known: tray_item, registered: 0, properties_forwarder, menu_forwarder };
    let previous = {
        let mut guard = registry.lock().unwrap();
        entry.registered = match guard.get(&key) {
            // The same key is the same item registering again, so it holds its place in the strip
            // rather than jumping to the end. An application that re-registers on its own restart
            // gets a new unique name and therefore a new key, which is a genuinely new item.
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

/// Runs until every `NewX` signal stream ends, re-fetching the full [`TrayItem`] (base
/// properties plus, if `menu` is `Some`, a full menu-tree refetch via the already-bound
/// proxy) on any of them and updating the registry entry in place -- no debounce, no
/// fine-grained per-property patching. One instance per tracked item; its `JoinHandle` lives
/// in the item's own [`ItemEntry`] and is aborted on unregistration.
fn spawn_item_signal_forwarder(
    item: StatusNotifierItemProxy<'static>,
    unique_name: OwnedUniqueName,
    menu: Option<DBusMenuProxy<'static>>,
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

            let mut refreshed = fetch_tray_item_base(&item, &unique_name).await;
            if let Some(menu) = &menu {
                refreshed.menu = fetch_menu_via(menu).await.ok();
            }

            let mut guard = registry.lock().unwrap();
            let Some(entry) = guard.get_mut(&key) else { break };
            entry.last_known = refreshed;
            drop(guard);

            if events.send(TraySignal::RegistryChanged).is_err() {
                break;
            }
        }
    })
}

/// Runs until `LayoutUpdated` stops firing, re-fetching `menu`'s full layout and updating the
/// registry entry's `menu` field in place on every occurrence (ADR-0031: "`GetLayout`... is
/// re-fetched on `LayoutUpdated`"). One instance per tracked item that has a menu; aborted
/// alongside [`spawn_item_signal_forwarder`]'s handle on unregistration.
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

/// Runs until the underlying signal stream ends, removing every registry entry whose unique name
/// just dropped off the bus (`new_owner` empty) -- the base SNI spec has no
/// `UnregisterStatusNotifierItem` signal, so this is the only liveness signal a host has
/// (ADR-0031's module doc comment). One global subscription, not per-item.
pub(super) fn spawn_name_owner_changed_forwarder(
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

            let removed: Vec<ItemEntry> = {
                let mut guard = registry.lock().unwrap();
                let stale_keys: Vec<ItemKey> =
                    guard.keys().filter(|(unique_name, _)| unique_name.as_str() == dropped_name).cloned().collect();
                stale_keys.into_iter().filter_map(|key| guard.remove(&key)).collect()
            };
            if removed.is_empty() {
                continue;
            }
            for entry in removed {
                entry.properties_forwarder.abort();
                if let Some(handle) = entry.menu_forwarder {
                    handle.abort();
                }
                // The item's own spooled pixmaps, gone with it. All three variants, since each
                // spools to its own filename (docs/adr/0074). `/dev/shm` outlives this process, so
                // without this every application restart leaves more PNGs resident until reboot:
                // the filename is built from the connection's unique name, and a reconnecting
                // application never gets the same one back.
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
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    /// An entry with everything but the two fields the ordering depends on stubbed out. The proxy
    /// is bound against a p2p pair rather than mocked: binding makes no call, so it needs no peer
    /// that answers.
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

    /// The whole point: a `HashMap`'s iteration order is seeded per process, so this used to be
    /// whatever the seed said, and a bar's tray reshuffled when one unrelated item updated.
    #[tokio::test]
    async fn the_strip_is_in_registration_order_whatever_the_map_says() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Built before the lock: `entry` is async, and holding a `std::sync::Mutex` guard across an
        // await is the thing `clippy::await_holding_lock` exists to stop.
        let third = entry(&connection, "third", 2).await;
        let first = entry(&connection, "first", 0).await;
        let second = entry(&connection, "second", 1).await;
        // Inserted in an order that is not the registration order, which is what a `HashMap` is
        // free to hand back.
        {
            let mut guard = registry.lock().unwrap();
            guard.insert(key(":1.30"), third);
            guard.insert(key(":1.10"), first);
            guard.insert(key(":1.20"), second);
        }
        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["first", "second", "third"]);
    }

    /// Registration order, not id order. These ids sort lexicographically the other way, which is
    /// exactly the trap a sort on `TrayItem::id` would fall into with real D-Bus unique names.
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

    /// An item updating in place must not move, which is the failure a config actually sees.
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
        // What `register_item` does on a repeat registration at a live key: keep the old sequence.
        replacement.registered = registry.lock().unwrap().get(&key(":1.10")).expect("just inserted").registered;
        registry.lock().unwrap().insert(key(":1.10"), replacement);

        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["first-again", "second"], "a re-registered item must not jump to the end");
    }
}
