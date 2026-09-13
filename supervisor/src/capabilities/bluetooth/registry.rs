//! Device registry: per-object/adapter/ObjectManager D-Bus signal forwarders that keep
//! [`super::BluetoothState`]'s device lists in sync.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use zbus::zvariant::OwnedObjectPath;

use super::BluetoothSignal;
use super::proxies::{Adapter1Proxy, Battery1Proxy, Device1Proxy, bind_battery, bind_device};

/// One tracked `Device1`. Cache `mac` at registration so write actions resolve it with a
/// synchronous `HashMap` scan, without `.await` in the hot path. Abort `forwarder` on
/// `InterfacesRemoved` so it cannot poll a gone object.
pub(super) struct DeviceEntry {
    pub(super) mac: String,
    pub(super) device: Device1Proxy<'static>,
    pub(super) battery: Option<Battery1Proxy<'static>>,
    forwarder: JoinHandle<()>,
}

pub(super) type DeviceRegistry = Arc<Mutex<HashMap<OwnedObjectPath, DeviceEntry>>>;

/// Binds `path` as `Device1`, caches `Address`, optionally binds `Battery1`, starts its forwarder,
/// and inserts it into `devices`. Logs and skips any D-Bus failure without a partial entry.
pub(super) async fn register_device(
    connection: &zbus::Connection,
    devices: &DeviceRegistry,
    path: OwnedObjectPath,
    has_battery: bool,
    events: UnboundedSender<BluetoothSignal>,
) {
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
    // BlueZ can emit a second `InterfacesAdded` for an existing path when GATT adds `Battery1`.
    // Dropping a `JoinHandle` does not abort its task, so abort the prior forwarder or it fires
    // twice.
    let previous = devices.lock().unwrap().insert(path, DeviceEntry { mac, device, battery, forwarder });
    if let Some(previous) = previous {
        previous.forwarder.abort();
    }
}

/// Forwards `Connected`/`Paired`/`Name`/`Blocked` and, when present, `Battery1.Percentage` changes as
/// [`BluetoothSignal::DeviceRegistryChanged`] until the connection drops. One per device,
/// aborted on `InterfacesRemoved`.
///
/// The battery branch shares the same `select!`, so this returns the registry's single
/// `JoinHandle`. Absent `battery` uses `std::future::pending()` instead of unifying two stream
/// types behind an `Option<Stream>` guard.
fn spawn_device_signal_forwarder(
    device: Device1Proxy<'static>,
    battery: Option<Battery1Proxy<'static>>,
    events: UnboundedSender<BluetoothSignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut connected_changed = device.receive_connected_changed().await;
        let mut paired_changed = device.receive_paired_changed().await;
        let mut name_changed = device.receive_name_changed().await;
        let mut blocked_changed = device.receive_blocked_changed().await;
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
                Some(_) = blocked_changed.next() => {
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

/// Mutates `devices` until `added`/`removed` end, registering on `InterfacesAdded`, removing and
/// aborting on `InterfacesRemoved`, then forwarding [`BluetoothSignal::DeviceRegistryChanged`].
/// This task owns mutation so a write command's `mac` lookup sees an up-to-date registry.
///
/// Takes already-subscribed streams, not the bare proxy: subscription must finish before
/// `GetManagedObjects()` hydration, or a change in that window is permanently missed.
pub(super) fn spawn_object_manager_forwarder<A, R>(
    connection: zbus::Connection,
    mut added: A,
    mut removed: R,
    devices: DeviceRegistry,
    events: UnboundedSender<BluetoothSignal>,
) where
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
                        // The signal reports only newly added interfaces. BlueZ commonly emits
                        // `Device1` first on pair/connect, then `Battery1` after GATT discovery;
                        // re-registering binds both and aborts the old battery-less forwarder.
                        let path: OwnedObjectPath = args.object_path().to_owned().into();
                        let already_tracked = devices.lock().unwrap().contains_key(&path);
                        if already_tracked {
                            register_device(&connection, &devices, path, true, events.clone()).await;
                            if events.send(BluetoothSignal::DeviceRegistryChanged).is_err() { break; }
                        }
                        // A battery-only event without a prior `Device1` has nothing to attach to.
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

/// Forwards adapter `Powered`/`Discovering`/`Discoverable` changes as [`BluetoothSignal::AdapterChanged`], so
/// state follows BlueZ's own writes as well as this controller's.
pub(super) fn spawn_adapter_signal_forwarder(
    adapter: Adapter1Proxy<'static>,
    events: UnboundedSender<BluetoothSignal>,
) {
    tokio::spawn(async move {
        let mut powered_changed = adapter.receive_powered_changed().await;
        let mut discovering_changed = adapter.receive_discovering_changed().await;
        let mut discoverable_changed = adapter.receive_discoverable_changed().await;
        loop {
            tokio::select! {
                Some(_) = powered_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                Some(_) = discovering_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                Some(_) = discoverable_changed.next() => {
                    if events.send(BluetoothSignal::AdapterChanged).is_err() { break; }
                }
                else => break,
            }
        }
    });
}
