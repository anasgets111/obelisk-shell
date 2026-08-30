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

// ---------------------------------------------------------------------------------------------
// Device registry.
// ---------------------------------------------------------------------------------------------

/// One tracked `Device1` object. `mac` is cached at registration time (read once via
/// `Device1`'s own `Address` property) rather than re-read live on every lookup: resolving
/// `pair`/`connect`/`disconnect`/`forget`'s `mac` argument back to an object path only needs a
/// synchronous `HashMap` scan this way, with no `.await` in the hot path. `forwarder` is this
/// device's own signal-forwarder task, aborted on `InterfacesRemoved` so it doesn't keep
/// polling a D-Bus object that no longer exists.
pub(super) struct DeviceEntry {
    pub(super) mac: String,
    pub(super) device: Device1Proxy<'static>,
    pub(super) battery: Option<Battery1Proxy<'static>>,
    forwarder: JoinHandle<()>,
}

pub(super) type DeviceRegistry = Arc<Mutex<HashMap<OwnedObjectPath, DeviceEntry>>>;

/// Binds `path` as a `Device1`, caches its `Address`, optionally binds `Battery1` (only if
/// `has_battery`), spawns this device's own signal forwarder, and inserts the resulting entry
/// into `devices`. Used both by startup hydration (one call per `Device1`-bearing path
/// `GetManagedObjects` returns) and by the `ObjectManager` forwarder's `InterfacesAdded`
/// handler. Logs and skips (never registers a half-built entry) on any D-Bus failure.
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
    // `InterfacesAdded` for a path this registry already tracks (e.g. `Battery1` attaching to
    // an already-known `Device1` once GATT discovery finishes). Dropping a `JoinHandle` does
    // not abort its task, so without this the old forwarder would leak forever and every later
    // property change would fire `DeviceRegistryChanged` twice.
    let previous = devices.lock().unwrap().insert(path, DeviceEntry { mac, device, battery, forwarder });
    if let Some(previous) = previous {
        previous.forwarder.abort();
    }
}

/// Runs until `device`'s connection drops, forwarding `Connected`/`Paired`/`Name` property
/// changes -- and, only if `battery` is `Some`, `Battery1.Percentage` changes -- as
/// [`BluetoothSignal::DeviceRegistryChanged`]. One instance per tracked device; its
/// `JoinHandle` lives in the device's own [`DeviceEntry`] and is aborted on
/// `InterfacesRemoved`, not left to run its natural course.
///
/// The `Battery1.Percentage` branch is folded into the same `select!` as the other three
/// (rather than spawned as a second task) so this function returns exactly one `JoinHandle` --
/// the registry entry has room for only one. `battery`'s absence is modeled as a
/// `std::future::pending()` arm rather than an `Option<Stream>` `if`-guard: simpler than
/// unifying a `PropertyStream<bool>` and a `PropertyStream<u8>` behind one type.
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
/// [`DeviceEntry`] on `InterfacesAdded`, removing and aborting one on `InterfacesRemoved`) and
/// forwarding [`BluetoothSignal::DeviceRegistryChanged`] after each mutation. Unlike a task
/// that only forwards a tag and leaves rebuilding to `main.rs`, this one also owns the
/// registry mutation itself: resolving `pair`/`connect`/`disconnect`/`forget`'s `mac` argument
/// needs a registry that's already up to date the moment a command arrives.
///
/// Takes the already-subscribed `added`/`removed` streams rather than the bare
/// `ObjectManagerProxy`: the subscription must complete before [`BluetoothController::new`]'s
/// own `GetManagedObjects()` hydration call runs, not after this task gets scheduled --
/// otherwise a device added or removed on the bus in that window would be silently and
/// permanently missed.
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
                        // signal, not the path's full interface set -- BlueZ commonly emits
                        // `Device1` first (on pair/connect) and a second, `Battery1`-only
                        // `InterfacesAdded` on the same path once GATT battery-service
                        // discovery finishes. Re-running `register_device` here rebinds
                        // `Device1` (harmless), binds `Battery1`, and its `insert` aborts the
                        // old, battery-less forwarder.
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
/// changes as [`BluetoothSignal::AdapterChanged`] -- needed so `bluetooth.enabled`/
/// `bluetooth.discovering` stay correct after any change BlueZ makes on its own, not just
/// after this controller's own writes.
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

