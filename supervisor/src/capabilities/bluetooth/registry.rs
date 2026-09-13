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
use super::proxies::{Adapter1Proxy, Battery1Proxy, Device1Proxy};
use crate::capabilities::bind;

/// One tracked `Device1`. Cache `mac` at registration so write actions resolve it with a
/// synchronous `HashMap` scan, without `.await` in the hot path. Abort `forwarder` on
/// `InterfacesRemoved` so it cannot poll a gone object.
pub(super) struct DeviceEntry {
    pub(super) mac: String,
    pub(super) device: Device1Proxy<'static>,
    pub(super) battery: Option<Battery1Proxy<'static>>,
    pub(super) forwarder: JoinHandle<()>,
}

pub(super) type DeviceRegistry = Arc<Mutex<HashMap<OwnedObjectPath, DeviceEntry>>>;

/// The adapter in use and the task watching its properties.
pub(super) struct BoundAdapter {
    path: OwnedObjectPath,
    pub(super) proxy: Adapter1Proxy<'static>,
    forwarder: tokio::task::AbortHandle,
}

/// The one adapter slot, filled and emptied by the `ObjectManager` forwarder.
pub(super) type AdapterSlot = Arc<Mutex<Option<BoundAdapter>>>;

/// Binds `path` as the adapter unless one is already in use, starts its property forwarder, and
/// pushes [`BluetoothSignal::AdapterChanged`]. The first adapter wins, as it did at startup.
async fn adopt_adapter(
    connection: &zbus::Connection,
    slot: &AdapterSlot,
    path: OwnedObjectPath,
    events: &UnboundedSender<BluetoothSignal>,
) {
    if slot.lock().unwrap().is_some() {
        return;
    }
    let proxy = match bind::<Adapter1Proxy>(connection, path.clone()).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("bluetooth: failed to bind adapter {path}: {err}");
            return;
        }
    };
    eprintln!("bluetooth: using adapter {path}");
    let forwarder = spawn_adapter_signal_forwarder(proxy.clone(), events.clone()).abort_handle();
    *slot.lock().unwrap() = Some(BoundAdapter { path, proxy, forwarder });
    let _ = events.send(BluetoothSignal::AdapterChanged);
}

/// Empties the slot when BlueZ removes the adapter in use, stops its forwarder, and pushes
/// [`BluetoothSignal::AdapterChanged`]. Removing any other adapter changes nothing.
///
/// ponytail: a second adapter already present does not take over; it is adopted only when BlueZ
/// adds it again. Upgrade path: rerun `GetManagedObjects` here and adopt the first `Adapter1`.
fn release_adapter(slot: &AdapterSlot, path: &OwnedObjectPath, events: &UnboundedSender<BluetoothSignal>) {
    let released = {
        let mut bound = slot.lock().unwrap();
        if bound.as_ref().is_some_and(|adapter| &adapter.path == path) { bound.take() } else { None }
    };
    if let Some(adapter) = released {
        adapter.forwarder.abort();
        eprintln!("bluetooth: adapter {path} was removed");
        let _ = events.send(BluetoothSignal::AdapterChanged);
    }
}

/// Binds `path` as `Device1`, caches `Address`, optionally binds `Battery1`, starts its forwarder,
/// and inserts it into `devices`. Logs and skips any D-Bus failure without a partial entry.
async fn register_device(
    connection: &zbus::Connection,
    devices: &DeviceRegistry,
    path: OwnedObjectPath,
    has_battery: bool,
    events: UnboundedSender<BluetoothSignal>,
) {
    let device = match bind::<Device1Proxy>(connection, path.clone()).await {
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
        match bind::<Battery1Proxy>(connection, path.clone()).await {
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

/// Adopts an `Adapter1` and registers a `Device1` at `path`, given what `has` says BlueZ reports
/// there. Returns whether the device registry changed. Serves both `GetManagedObjects` hydration
/// and `InterfacesAdded`, which reports only newly added interfaces: BlueZ commonly adds `Device1`
/// on pair/connect and `Battery1` after GATT discovery, and re-registering a tracked device binds
/// both and aborts its battery-less forwarder.
pub(super) async fn track_interfaces(
    connection: &zbus::Connection,
    devices: &DeviceRegistry,
    adapter: &AdapterSlot,
    path: OwnedObjectPath,
    has: impl Fn(&str) -> bool,
    events: &UnboundedSender<BluetoothSignal>,
) -> bool {
    if has("org.bluez.Adapter1") {
        adopt_adapter(connection, adapter, path.clone(), events).await;
    }
    let has_battery = has("org.bluez.Battery1");
    // A battery-only event without a prior `Device1` has nothing to attach to.
    let tracked = has_battery && devices.lock().unwrap().contains_key(&path);
    if !has("org.bluez.Device1") && !tracked {
        return false;
    }
    register_device(connection, devices, path, has_battery, events.clone()).await;
    true
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

/// Mutates `devices` and `adapter` until `added`/`removed` end. Devices register on
/// `InterfacesAdded` and are removed and aborted on `InterfacesRemoved`, forwarding
/// [`BluetoothSignal::DeviceRegistryChanged`]; an `Adapter1` goes through [`adopt_adapter`] and
/// [`release_adapter`]. This task owns mutation so a write command's lookup sees an up-to-date
/// registry.
///
/// Takes already-subscribed streams, not the bare proxy: subscription must finish before
/// `GetManagedObjects()` hydration, or a change in that window is permanently missed.
pub(super) fn spawn_object_manager_forwarder<A, R>(
    connection: zbus::Connection,
    mut added: A,
    mut removed: R,
    devices: DeviceRegistry,
    adapter: AdapterSlot,
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
                    let interfaces = args.interfaces_and_properties();
                    let has = |name: &str| interfaces.keys().any(|k| k.as_str() == name);
                    let path = args.object_path().to_owned().into();
                    if track_interfaces(&connection, &devices, &adapter, path, has, &events).await
                        && events.send(BluetoothSignal::DeviceRegistryChanged).is_err()
                    {
                        break;
                    }
                }
                Some(signal) = removed.next() => {
                    let Ok(args) = signal.args() else { continue; };
                    if args.interfaces().iter().any(|i| i.as_str() == "org.bluez.Adapter1") {
                        let path: OwnedObjectPath = args.object_path().to_owned().into();
                        release_adapter(&adapter, &path, &events);
                    }
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

/// Forwards adapter `Powered`/`Discovering`/`Discoverable` changes as
/// [`BluetoothSignal::AdapterChanged`], so state follows BlueZ's own writes as well as this
/// controller's.
fn spawn_adapter_signal_forwarder(
    adapter: Adapter1Proxy<'static>,
    events: UnboundedSender<BluetoothSignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut changes = adapter
            .receive_powered_changed()
            .await
            .map(drop)
            .merge(adapter.receive_discovering_changed().await.map(drop))
            .merge(adapter.receive_discoverable_changed().await.map(drop));
        while changes.next().await.is_some() && events.send(BluetoothSignal::AdapterChanged).is_ok() {}
    })
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    fn path(leaf: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!("/org/bluez/{leaf}")).expect("valid object path")
    }

    #[tokio::test]
    async fn the_first_adapter_is_kept_until_bluez_removes_that_one() {
        let (connection, _peer) = p2p_pair().await;
        let (events, mut signals) = unbounded_channel();
        let slot = AdapterSlot::default();
        let in_use = |slot: &AdapterSlot| slot.lock().unwrap().as_ref().map(|bound| bound.path.clone());

        adopt_adapter(&connection, &slot, path("hci0"), &events).await;
        assert_eq!(in_use(&slot), Some(path("hci0")));
        assert_eq!(signals.try_recv(), Ok(BluetoothSignal::AdapterChanged));

        adopt_adapter(&connection, &slot, path("hci1"), &events).await;
        assert_eq!(in_use(&slot), Some(path("hci0")), "a second adapter does not replace the one in use");
        assert!(signals.try_recv().is_err());

        release_adapter(&slot, &path("hci1"), &events);
        assert_eq!(in_use(&slot), Some(path("hci0")), "removing another adapter changes nothing");
        assert!(signals.try_recv().is_err());

        release_adapter(&slot, &path("hci0"), &events);
        assert_eq!(in_use(&slot), None);
        assert_eq!(signals.try_recv(), Ok(BluetoothSignal::AdapterChanged));
    }
}
