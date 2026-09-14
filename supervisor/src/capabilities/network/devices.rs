//! NetworkManager devices for `obelisk.network`: resolving the Wi-Fi and wired devices, and the
//! forwarder tasks that turn their signals, and the manager's, into [`NetworkSignal`]s.

use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::{Stream, StreamExt};
use zbus::zvariant::OwnedObjectPath;

use super::proxies::{
    AccessPointProxy, DEVICE_TYPE_ETHERNET, DEVICE_TYPE_WIFI, DeviceProxy, NetworkManagerProxy, WiredProxy,
    WirelessProxy,
};
use super::{NetworkController, NetworkSignal};
use crate::capabilities::bind;

/// One resolved Wi-Fi device: its path for `AddAndActivateConnection2`, plus the `Device` and
/// `Device.Wireless` proxies. Association state and APs use those two interfaces on one object.
#[derive(Clone)]
pub(super) struct WifiDevice {
    pub(super) device_path: OwnedObjectPath,
    pub(super) device: DeviceProxy<'static>,
    pub(super) wireless: WirelessProxy<'static>,
}

/// One resolved Ethernet device. Its path feeds `ActivateConnection`, its `Device` proxy feeds
/// `ethernet_enabled` and the address, and its `Device.Wired` proxy feeds the link speed.
#[derive(Clone)]
pub(super) struct EthernetDevice {
    pub(super) path: OwnedObjectPath,
    pub(super) device: DeviceProxy<'static>,
    pub(super) wired: WiredProxy<'static>,
}

/// The Wi-Fi and wired devices NetworkManager has now, with the tasks watching them. Replaced whole
/// on [`NetworkSignal::DevicesChanged`]; callers clone devices out, so no await holds the lock.
#[derive(Default)]
pub(super) struct Devices {
    pub(super) wifi: Option<WifiDevice>,
    pub(super) ethernet: Vec<EthernetDevice>,
    /// Aborted when the set is replaced, so a removed device's watchers stop with it.
    pub(super) watchers: Vec<tokio::task::AbortHandle>,
}

/// Binds every Wi-Fi and wired device from `GetAllDevices`. Unreadable devices are logged and
/// skipped.
pub(super) async fn resolve_devices(
    connection: &zbus::Connection,
    nm: &NetworkManagerProxy<'static>,
) -> zbus::Result<(Option<WifiDevice>, Vec<EthernetDevice>)> {
    let mut wifi = None;
    let mut ethernet = Vec::new();
    for path in nm.get_all_devices().await? {
        let device = match bind::<DeviceProxy>(connection, path.clone()).await {
            Ok(device) => device,
            Err(err) => {
                eprintln!("network: failed to bind device {path}: {err}");
                continue;
            }
        };
        let device_type = match device.device_type().await {
            Ok(device_type) => device_type,
            Err(err) => {
                eprintln!("network: failed to read device_type for {path}: {err}");
                continue;
            }
        };
        match device_type {
            DEVICE_TYPE_ETHERNET => match bind::<WiredProxy>(connection, path.clone()).await {
                Ok(wired) => ethernet.push(EthernetDevice { path, device, wired }),
                Err(err) => eprintln!("network: failed to bind wired device {path}: {err}"),
            },
            DEVICE_TYPE_WIFI if wifi.is_none() => match bind::<WirelessProxy>(connection, path.clone()).await {
                Ok(wireless) => wifi = Some(WifiDevice { device_path: path, device, wireless }),
                Err(err) => eprintln!("network: failed to bind wireless device {path}: {err}"),
            },
            _ => {}
        }
    }
    Ok((wifi, ethernet))
}

/// Starts the per-device watchers for one device set and returns their abort handles.
pub(super) fn watch_devices(
    connection: &zbus::Connection,
    wifi: Option<&WifiDevice>,
    ethernet: &[EthernetDevice],
    events: &UnboundedSender<NetworkSignal>,
) -> Vec<tokio::task::AbortHandle> {
    let mut watchers = Vec::new();
    match wifi {
        Some(wifi) => {
            watchers
                .push(spawn_wifi_forwarder(connection.clone(), wifi.wireless.clone(), events.clone()).abort_handle());
            watchers.push(spawn_device_state_forwarder(wifi.device.clone(), events.clone()).abort_handle());
        }
        None => eprintln!("network: no Wi-Fi device found; scan and access-point events wait for one to appear"),
    }
    for device in ethernet {
        watchers.push(spawn_device_state_forwarder(device.device.clone(), events.clone()).abort_handle());
    }
    watchers
}

/// Forwards `org.freedesktop.NetworkManager.Device.Wireless` events to `events` until the
/// connection drops. Spawned once by [`NetworkController::new`] so borrow-heavy streams stay out
/// of `main.rs`'s top-level `select!`; a dropped receiver ends it on the next send.
///
/// `ActiveAccessPoint` is the only watched property that moves when the radio joins or leaves.
/// Watching only the AP set and `LastScan` left `connected` frozen, so an association after bar
/// startup read offline until the next scan, minutes later.
///
/// It owns the associated AP's strength watch, retargeted and aborted with each association.
/// Otherwise an orphan would rebuild for an AP nothing is connected to (ADR-0082).
fn spawn_wifi_forwarder(
    connection: zbus::Connection,
    wireless: WirelessProxy<'static>,
    events: UnboundedSender<NetworkSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ap_set = tokio::try_join!(wireless.receive_access_point_added(), wireless.receive_access_point_removed());
        let mut ap_set_changed = match ap_set {
            Ok((added, removed)) => added.map(drop).merge(removed.map(drop)),
            Err(err) => {
                eprintln!("network: failed to subscribe to AccessPointAdded/AccessPointRemoved: {err}");
                return;
            }
        };
        let mut last_scan_changed = wireless.receive_last_scan_changed().await;
        let mut active_ap_changed = wireless.receive_active_access_point_changed().await;
        // The first `active_ap_changed` emission fills the property cache, so an existing
        // association is watched without a startup read. A `JoinSet` aborts its task when dropped,
        // so aborting this watcher on a device rescan stops the strength watch too.
        let mut strength = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                Some(()) = ap_set_changed.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(change) = active_ap_changed.next() => {
                    strength.shutdown().await;
                    if let Ok(path) = change.get().await {
                        strength.extend(strength_forwarder(&connection, path, events.clone()));
                    }
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(_) = last_scan_changed.next() => {
                    if events.send(NetworkSignal::ScanCompleted).is_err() { break; }
                }
                else => break,
            }
        }
    })
}

/// Forwards the associated AP's `Strength` as [`NetworkSignal::Changed`], keeping bars current
/// between scans. `None` for NetworkManager's `/` path means no association.
///
/// Watch only the associated AP. On real hardware over 180s it emitted 26 times, a quiet 6-second
/// poll, versus 76 events across 17 APs, one every 2.4s indefinitely. Rebuilds reread every AP,
/// so the full list stayed as fresh at one third the traffic; ADR-0029 item 6 required this
/// measured choice before adding debounce.
fn strength_forwarder(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
    events: UnboundedSender<NetworkSignal>,
) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
    if path.as_str() == "/" {
        return None;
    }
    let connection = connection.clone();
    Some(async move {
        let access_point = match bind::<AccessPointProxy>(&connection, path.clone()).await {
            Ok(access_point) => access_point,
            Err(err) => {
                eprintln!("network: failed to bind the associated access point {path}: {err}");
                return;
            }
        };
        let mut strength_changed = access_point.receive_strength_changed().await;
        while strength_changed.next().await.is_some() && events.send(NetworkSignal::Changed).is_ok() {}
    })
}

/// Forwards each device's `State` as [`NetworkSignal::Changed`]. Per-device tasks cover
/// `ethernet_enabled` and announce Wi-Fi disconnects before `ActiveAccessPoint` catches up.
/// zbus emits the cached current value once, priming the first snapshot without a startup read.
fn spawn_device_state_forwarder(
    device: DeviceProxy<'static>,
    events: UnboundedSender<NetworkSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state_changed = device.receive_state_changed().await;
        while state_changed.next().await.is_some() && events.send(NetworkSignal::Changed).is_ok() {}
    })
}

/// Forwards manager-wide properties used by `NetworkState`: radio switches and the default route.
/// Device tasks cannot cover them; networking can switch off while devices stay put, and the route
/// can move between two activated devices.
pub(super) fn spawn_manager_forwarder(nm: NetworkManagerProxy<'static>, events: UnboundedSender<NetworkSignal>) {
    tokio::spawn(async move {
        let mut changes = nm
            .receive_wireless_enabled_changed()
            .await
            .map(drop)
            .merge(nm.receive_networking_enabled_changed().await.map(drop))
            .merge(nm.receive_primary_connection_changed().await.map(drop));
        while changes.next().await.is_some() && events.send(NetworkSignal::Changed).is_ok() {}
    });
}

/// Sends `signal` for each item of an added/removed subscription pair, such as `DeviceAdded` and
/// `DeviceRemoved`, until both end or the receiver does. A failed subscription is logged: changes
/// it would have reported need a restart.
pub(super) fn forward<A, B>(
    subscribed: zbus::Result<(A, B)>,
    signal: NetworkSignal,
    events: UnboundedSender<NetworkSignal>,
) where
    A: Stream + Unpin + Send + 'static,
    B: Stream + Unpin + Send + 'static,
{
    let (added, removed) = match subscribed {
        Ok(streams) => streams,
        Err(err) => {
            return eprintln!("network: failed to subscribe for {signal:?}; those changes need a restart: {err}");
        }
    };
    let mut changes = added.map(drop).merge(removed.map(drop));
    tokio::spawn(async move { while changes.next().await.is_some() && events.send(signal).is_ok() {} });
}

impl NetworkController {
    /// Rescans devices and restarts their watchers, new before old so no change falls in a gap.
    /// Skips the restart when the Wi-Fi and wired paths are unchanged, as for most veth, bridge and
    /// VPN additions.
    pub(super) async fn refresh_devices(&self) {
        let (wifi, ethernet) = match resolve_devices(&self.connection, &self.nm).await {
            Ok(found) => found,
            Err(err) => {
                eprintln!("network: failed to rescan devices: {err}");
                return;
            }
        };
        let unchanged = {
            let current = self.devices.lock().unwrap();
            current.wifi.as_ref().map(|wifi| &wifi.device_path) == wifi.as_ref().map(|wifi| &wifi.device_path)
                && current.ethernet.iter().map(|device| &device.path).eq(ethernet.iter().map(|device| &device.path))
        };
        if unchanged {
            return;
        }
        eprintln!("network: device set changed: wifi={} ethernet={}", wifi.is_some(), ethernet.len());
        let watchers = watch_devices(&self.connection, wifi.as_ref(), &ethernet, &self.events);
        let previous = std::mem::replace(&mut *self.devices.lock().unwrap(), Devices { wifi, ethernet, watchers });
        for watcher in previous.watchers {
            watcher.abort();
        }
    }
}
