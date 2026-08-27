//! Per-item registry: hydration + signal-forwarder tasks that keep each tracked
//! `StatusNotifierItem`'s [`super::item::TrayItem`] snapshot live.
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::zvariant::OwnedObjectPath;

use super::TraySignal;
use super::item::{TrayItem, fetch_tray_item_base};
use super::menu::fetch_menu_via;
use super::proxies::{DBusMenuProxy, StatusNotifierItemProxy, bind_dbusmenu, bind_item};

// -------------------------------------------------------------------------------------------
// Registry.
// -------------------------------------------------------------------------------------------

pub(super) struct ItemEntry {
    pub(super) item: StatusNotifierItemProxy<'static>,
    pub(super) menu: Option<DBusMenuProxy<'static>>,
    pub(super) last_known: TrayItem,
    properties_forwarder: JoinHandle<()>,
    menu_forwarder: Option<JoinHandle<()>>,
}

pub(super) type ItemKey = (OwnedUniqueName, OwnedObjectPath);
pub(super) type ItemRegistry = Arc<Mutex<HashMap<ItemKey, ItemEntry>>>;

/// Binds `unique_name`/`object_path` as a `StatusNotifierItem`, hydrates its full [`TrayItem`]
/// (including its menu tree, if it has one -- ADR-0031's "eager top-level fetch"), spawns its
/// signal forwarder(s), and inserts the resulting entry into `registry`. Used both by
/// `RegisterStatusNotifierItem` and, in principle, by any future re-registration path. Aborts and
/// replaces a prior entry at the same key rather than leaking its forwarder tasks (mirrors
/// `dbus::bluetooth::register_device`'s own `insert`-returns-previous handling).
pub(super) async fn register_item(
    connection: &zbus::Connection,
    registry: &ItemRegistry,
    events: &UnboundedSender<TraySignal>,
    unique_name: OwnedUniqueName,
    object_path: OwnedObjectPath,
) -> Result<(), String> {
    let item = bind_item(connection, &unique_name, &object_path).await.map_err(|err| format!("failed to bind StatusNotifierItem: {err}"))?;

    let mut tray_item = fetch_tray_item_base(&item, &unique_name).await;

    let menu_path = item.menu().await.ok();
    let menu = match &menu_path {
        Some(path) if !path.as_str().is_empty() && path.as_str() != "/" => match bind_dbusmenu(connection, &unique_name, path).await {
            Ok(menu) => Some(menu),
            Err(err) => {
                eprintln!("tray: failed to bind DBusMenu for {unique_name} at {path}: {err}");
                None
            }
        },
        _ => None,
    };
    if let Some(menu) = &menu {
        match fetch_menu_via(menu).await {
            Ok(items) => tray_item.menu = Some(items),
            Err(err) => eprintln!("tray: GetLayout failed for {unique_name}: {err}"),
        }
    }

    let key: ItemKey = (unique_name.clone(), object_path.clone());

    // Narrow TOCTOU guard (Correctness review): every `.await` above (property reads, an
    // optional GetLayout) is a window in which the registering connection could have
    // disconnected -- NameOwnerChanged-based cleanup (spawn_name_owner_changed_forwarder) only
    // ever removes an entry that already exists, so a disconnect landing in that window would
    // otherwise plant an unreachable ghost entry no later signal can ever remove (a narrower
    // version of the fabricated-unique-name bug resolve_registration's UniqueName branch now
    // rejects -- but for a connection that legitimately existed and then genuinely disconnected
    // mid-registration, not a fabricated one). One more liveness check, right here before the
    // insert below (nothing else `.await`s between this and it), narrows that whole multi-await
    // window down to a single check-then-insert. Reuses the same org.freedesktop.DBus mechanism
    // resolve_registration's WellKnownName branch already uses for GetNameOwner. Best-effort: a
    // failure to even ask (proxy bind or the call itself erroring) proceeds with the insert rather
    // than blocking a legitimate registration on an unrelated D-Bus hiccup -- this narrows the
    // race, it doesn't need to be perfect.
    if let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(connection).await {
        match dbus_proxy.name_has_owner(BusName::from(unique_name.clone())).await {
            Ok(false) => {
                eprintln!("tray: {unique_name} disconnected during registration; not inserting a registry entry");
                return Ok(());
            }
            Ok(true) => {}
            Err(err) => eprintln!("tray: pre-insert liveness check for {unique_name} failed (proceeding anyway): {err}"),
        }
    }

    let properties_forwarder = spawn_item_signal_forwarder(item.clone(), unique_name.clone(), menu.clone(), key.clone(), registry.clone(), events.clone());
    let menu_forwarder = menu.clone().map(|menu| spawn_menu_signal_forwarder(menu, key.clone(), registry.clone(), events.clone()));

    let entry = ItemEntry { item, menu, last_known: tray_item, properties_forwarder, menu_forwarder };
    let previous = registry.lock().unwrap().insert(key, entry);
    if let Some(previous) = previous {
        previous.properties_forwarder.abort();
        if let Some(handle) = previous.menu_forwarder {
            handle.abort();
        }
    }
    let _ = events.send(TraySignal::RegistryChanged);
    Ok(())
}

/// Runs until every `NewX` signal stream ends, re-fetching the full [`TrayItem`] (base properties
/// plus, if `menu` is `Some`, a full menu-tree refetch via the already-bound proxy) on any of
/// them and updating the registry entry in place -- no debounce, no fine-grained per-property
/// patching (mirrors `dbus::bluetooth`/`dbus::network`'s established "full re-derivation on any
/// relevant event" discipline). One instance per tracked item; its `JoinHandle` lives in the
/// item's own [`ItemEntry`] and is aborted on unregistration.
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
fn spawn_menu_signal_forwarder(menu: DBusMenuProxy<'static>, key: ItemKey, registry: ItemRegistry, events: UnboundedSender<TraySignal>) -> JoinHandle<()> {
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
pub(super) fn spawn_name_owner_changed_forwarder(dbus_proxy: zbus::fdo::DBusProxy<'static>, registry: ItemRegistry, events: UnboundedSender<TraySignal>) -> JoinHandle<()> {
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
                let stale_keys: Vec<ItemKey> = guard.keys().filter(|(unique_name, _)| unique_name.as_str() == dropped_name).cloned().collect();
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
            }
            if events.send(TraySignal::RegistryChanged).is_err() {
                break;
            }
        }
    })
}


