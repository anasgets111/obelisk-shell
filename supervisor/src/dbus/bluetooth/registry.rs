//! Device registry: per-object/adapter/ObjectManager D-Bus signal forwarders that keep
//! [`super::BluetoothState`]'s device lists in sync.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use zbus::zvariant::OwnedObjectPath;

use super::proxies::{Adapter1Proxy, Battery1Proxy, Device1Proxy, bind_battery, bind_device};
use super::BluetoothSignal;

/// One tracked `Device1` object. `mac` is cached at registration time rather than re-read
/// live: resolving `pair`/`connect`/`disconnect`/`forget`'s `mac` argument to an object path
/// only needs a synchronous `HashMap` scan this way, no `.await` in the hot path.
/// `forwarder` is aborted on `InterfacesRemoved` so it doesn't keep polling a gone object.
pub(super) struct DeviceEntry {
    pub(super) mac: String,
    pub(super) device: Device1Proxy<'static>,
    pub(super) battery: Option<Battery1Proxy<'static>>,
    forwarder: JoinHandle<()>,
}

pub(super) type DeviceRegistry = Arc<Mutex<HashMap<OwnedObjectPath, DeviceEntry>>>;

/// Binds `path` as a `Device1`, caches its `Address`, optionally binds `Battery1` (only if
/// `has_battery`), spawns this device's own signal forwarder, and inserts the entry into
/// `devices`. Logs and skips (never registers a half-built entry) on any D-Bus failure.
pub(super) async fn register_device(connection: &zbus::Connection, devices: &DeviceRegistry, path: OwnedObjectPath, has_battery: bool, events: UnboundedSender<BluetoothSignal>) {
    let device = match bind_device(connection, path.clone()).await {
        Ok(device) => device,
        Err(err) => {
            eprintln!("bluetooth: failed to bind device {path}: {err}");
            return;
        }
    };
    let mac = match device.address().await {
        Ok(mac) => mac,
        Err(err) => {
            eprintln!("bluetooth: failed to read Address for device {path}: {err}");
            return;
        }
    };
    let battery = if has_battery {
        match bind_battery(connection, path.clone()).await {
            Ok(battery) => Some(battery),
            Err(err) => {
                eprintln!("bluetooth: failed to bind Battery1 for device {path} ({mac}): {err}");
                None
            }
        }
    } else {
        None
    };
    let forwarder = spawn_device_signal_forwarder(device.clone(), battery.clone(), events);
    // `insert` returns the prior value at this key, if any -- real BlueZ can emit a second
    // `InterfacesAdded` for a path already tracked (e.g. `Battery1` attaching to an
    // already-known `Device1` once GATT discovery finishes). Dropping a `JoinHandle` doesn't
    // abort its task, so without this the old forwarder would leak and fire twice.
    let previous = devices.lock().unwrap().insert(path, DeviceEntry { mac, device, battery, forwarder });
    if let Some(previous) = previous {
        previous.forwarder.abort();
    }
}

/// Runs until `device`'s connection drops, forwarding `Connected`/`Paired`/`Name` property
/// changes -- and, only if `battery` is `Some`, `Battery1.Percentage` changes -- as
/// [`BluetoothSignal::DeviceRegistryChanged`]. One instance per tracked device, aborted on
/// `InterfacesRemoved`.
///
/// The `Battery1.Percentage` branch is folded into the same `select!` as the other three so
/// this function returns exactly one `JoinHandle` -- the registry entry has room for only
/// one. `battery`'s absence is modeled as `std::future::pending()` rather than an
/// `Option<Stream>` guard: simpler than unifying two different stream types behind one.
fn spawn_device_signal_forwarder(device: Device1Proxy<'static>, battery: Option<Battery1Proxy<'static>>, events: UnboundedSender<BluetoothSignal>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut connected_changed = device.receive_connected_changed().await;
        let mut paired_changed = device.receive_paired_changed().await;
        let mut name_changed = device.receive_name_changed().await;
        let mut percentage_changed = match &battery {
            Some(battery) => Some(battery.receive_percentage_changed().await),
            None => None,
        };

        loop {
            tokio::select! {
                Some(_) = connected_changed.next() => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                Some(_) = paired_changed.next() => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                Some(_) = name_changed.next() => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                Some(_) = async {
                    match &mut percentage_changed {
                        Some(stream) => stream.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                }
                else => break,
            }
        }
    })
}

/// Runs until `added`/`removed` end, mutating `devices` directly (registering a fresh
/// [`DeviceEntry`] on `InterfacesAdded`, removing and aborting one on `InterfacesRemoved`)
/// and forwarding [`BluetoothSignal::DeviceRegistryChanged`] after each mutation -- this task
/// owns the registry mutation itself since resolving a write command's `mac` argument needs
/// a registry that's already up to date the moment the command arrives.
///
/// Takes the already-subscribed `added`/`removed` streams rather than the bare
/// `ObjectManagerProxy`: the subscription must complete before `GetManagedObjects()`'s own
/// hydration call runs, not after this task gets scheduled, or a device added/removed in
/// that window would be silently and permanently missed.
pub(super) fn spawn_object_manager_forwarder<A, R>(connection: zbus::Connection, mut added: A, mut removed: R, devices: DeviceRegistry, events: UnboundedSender<BluetoothSignal>)
where
    A: tokio_stream::Stream<Item = zbus::fdo::InterfacesAdded> + Unpin + Send + 'static,
    R: tokio_stream::Stream<Item = zbus::fdo::InterfacesRemoved> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(signal) = added.next() => {
                    let Ok(args) = signal.args() else { continue; };
                    let has_device = args.interfaces_and_properties().keys().any(|k| k.as_str() == "org.bluez.Device1");
                    let has_battery = args.interfaces_and_properties().keys().any(|k| k.as_str() == "org.bluez.Battery1");
                    if has_device {
                        register_device(&connection, &devices, args.object_path().to_owned().into(), has_battery, events.clone()).await;
                        if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                    } else if has_battery {
                        // `InterfacesAdded` reports only interfaces newly present at this exact
                        // signal, not the path's full set -- BlueZ commonly emits `Device1`
                        // first (on pair/connect) and a second, `Battery1`-only
                        // `InterfacesAdded` once GATT battery discovery finishes. Re-running
                        // `register_device` rebinds `Device1` (harmless), binds `Battery1`,
                        // and its `insert` aborts the old, battery-less forwarder.
                        let path: OwnedObjectPath = args.object_path().to_owned().into();
                        let already_tracked = devices.lock().unwrap().contains_key(&path);
                        if already_tracked {
                            register_device(&connection, &devices, path, true, events.clone()).await;
                            if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                        }
                        // else: a `Battery1`-only event with no prior `Device1` -- shouldn't
                        // normally happen, but there's nothing to attach it to yet, so skip it.
                    }
                }
                Some(signal) = removed.next() => {
                    let Ok(args) = signal.args() else { continue; };
                    let has_device = args.interfaces().iter().any(|i| i.as_str() == "org.bluez.Device1");
                    if has_device {
                        let path: OwnedObjectPath = args.object_path().to_owned().into();
                        let removed_entry = devices.lock().unwrap().remove(&path);
                        if let Some(entry) = removed_entry {
                            entry.forwarder.abort();
                        }
                        if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                    }
                }
                else => break,
            }
        }
    });
}

/// Runs until `adapter`'s connection drops, forwarding `Powered`/`Discovering` property
/// changes as [`BluetoothSignal::AdapterChanged`] -- keeps `bluetooth.enabled`/`discovering`
/// correct after any change BlueZ makes on its own, not just this controller's own writes.
pub(super) fn spawn_adapter_signal_forwarder(adapter: Adapter1Proxy<'static>, events: UnboundedSender<BluetoothSignal>) {
    tokio::spawn(async move {
        let mut powered_changed = adapter.receive_powered_changed().await;
        let mut discovering_changed = adapter.receive_discovering_changed().await;
        loop {
            tokio::select! {
                Some(_) = powered_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                Some(_) = discovering_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                else => break,
            }
        }
    });
}

