//! [`NetworkController`]: `obelisk.network`'s proxies and push state, rebuilt from NetworkManager on
//! every signal, and the networking, radio and wired switches.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;

use super::connect::Attempt;
use super::devices::{
    Devices, EthernetDevice, WifiDevice, forward, resolve_devices, spawn_manager_forwarder, watch_devices,
};
use super::proxies::{
    AccessPointProxy, DEVICE_STATE_ACTIVATED, DeviceProxy, IP4ConfigProxy, NetworkManagerProxy, SettingsProxy,
};
use super::scan::resolve_ssid;
use super::{NetworkSignal, NetworkState, PendingNetworkConnect};

/// Proxies needed by `obelisk.network`, resolved at construction. `Clone` is cheap for zbus handles,
/// so writes can move a clone into `tokio::spawn` (ADR-0029).
#[derive(Clone)]
pub struct NetworkController {
    pub(super) connection: zbus::Connection,
    pub(super) nm: NetworkManagerProxy<'static>,
    pub(super) settings: SettingsProxy<'static>,
    /// The devices NetworkManager has now; see [`Devices`].
    pub(super) devices: Arc<Mutex<Devices>>,
    /// AP proxies kept between rebuilds, keyed by object path. They retain zbus property caches fed
    /// by `PropertiesChanged`, avoiding a match rule, `GetAll`, and unsubscribe per AP per pass.
    /// At 10 APs, rebuild time fell from 11.25ms to 0.84ms (ADR-0082).
    ///
    /// Pruned against the live path list on each rebuild; `AccessPointRemoved` already requests it.
    pub(super) access_points: Arc<Mutex<HashMap<OwnedObjectPath, AccessPointProxy<'static>>>>,
    /// Saved Wi-Fi SSIDs for [`AccessPointInfo::saved`](super::AccessPointInfo::saved), refreshed on
    /// [`NetworkSignal::SavedChanged`]. ponytail: an edited profile's SSID stays stale until the next
    /// add or remove. Upgrade path: watch each profile's `Updated`.
    pub(super) saved_ssids: Arc<Mutex<HashSet<Vec<u8>>>>,
    /// `obelisk.network` push state (ADR-0037), mutated only by
    /// [`handle_signal`](Self::handle_signal).
    /// The cloned controller shares it; the mutex is never held across an await.
    pub(super) state: Arc<Mutex<NetworkState>>,
    /// The single pending `network:connect` intent slot, see [`PendingNetworkConnect`].
    pub(super) pending_connect: Arc<Mutex<Option<PendingNetworkConnect>>>,
    /// The attempt `connecting_ssid` names; see [`Attempt`]. Locked after `state` when both are held.
    pub(super) attempt: Arc<Mutex<Attempt>>,
    /// Signal sender for [`mark_scanning`](Self::mark_scanning)'s FIFO event and for keeping the
    /// channel open when no Wi-Fi device exists.
    pub(super) events: UnboundedSender<NetworkSignal>,
}

/// Carries what `build_state` cannot read from NetworkManager across a rebuild: the scan flag and
/// the join fields, taken from `previous` because the caller overwrites it. A scan ends on
/// `ScanCompleted`, or with the Wi-Fi device, whose `LastScan` will never move again.
fn carry_across_rebuild(next: &mut NetworkState, previous: &mut NetworkState, signal: NetworkSignal) {
    next.scanning = signal != NetworkSignal::ScanCompleted && previous.scanning && next.wifi_present;
    next.connecting_ssid = previous.connecting_ssid.take();
    next.connect_error = previous.connect_error.take();
    next.password_ssid = previous.password_ssid.take();
}

/// `device`'s first IPv4 address without its prefix, read uncached because `Ip4Config`'s path
/// changes per activation. ponytail: a DHCP renewal without a state change shows the old address
/// until the next rebuild. Upgrade path: watch `AddressData`.
async fn read_ipv4(connection: &zbus::Connection, device: Option<&DeviceProxy<'static>>) -> Option<String> {
    let path = device?.ip4_config().await.ok().filter(|path| path.as_str() != "/")?;
    let config = IP4ConfigProxy::builder(connection)
        .path(path)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let addresses = config.address_data().await.ok()?;
    String::try_from(addresses.first()?.get("address")?.clone()).ok()
}

impl NetworkController {
    /// Connects over the Supervisor's system-bus `connection`, resolves Wi-Fi/Ethernet devices,
    /// and spawns `events` forwarders (ADR-0037). Unreadable `DeviceType`s are logged and skipped.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<NetworkSignal>) -> zbus::Result<Self> {
        let nm = NetworkManagerProxy::new(&connection).await?;
        let settings = SettingsProxy::new(&connection).await?;

        // Subscribed before the first device scan, so an adapter plugged in during it is not missed.
        let device_list = tokio::try_join!(nm.receive_device_added(), nm.receive_device_removed());
        forward(device_list, NetworkSignal::DevicesChanged, events.clone());
        let (wifi, ethernet) = resolve_devices(&connection, &nm).await?;
        let watchers = watch_devices(&connection, wifi.as_ref(), &ethernet, &events);
        spawn_manager_forwarder(nm.clone(), events.clone());
        // Subscribed before the first fill below, so a profile saved in between is not missed.
        let profiles = tokio::try_join!(settings.receive_new_connection(), settings.receive_connection_removed());
        forward(profiles, NetworkSignal::SavedChanged, events.clone());

        let controller = Self {
            connection,
            nm,
            settings,
            devices: Arc::new(Mutex::new(Devices { wifi, ethernet, watchers })),
            access_points: Arc::new(Mutex::new(HashMap::new())),
            saved_ssids: Arc::default(),
            state: Arc::new(Mutex::new(NetworkState::default())),
            pending_connect: Arc::new(Mutex::new(None)),
            attempt: Arc::default(),
            events,
        };
        controller.refresh_saved_ssids().await;
        Ok(controller)
    }

    /// Applies one [`NetworkSignal`] to the controller-owned [`NetworkState`] and returns the
    /// updated state to push (ADR-0029: no debounce, every relevant event fully re-derives the
    /// state from scratch).
    pub async fn handle_signal(&self, signal: NetworkSignal) -> NetworkState {
        match signal {
            NetworkSignal::ScanStarted => {
                let mut state = self.state.lock().unwrap();
                state.scanning = true;
                state.clone()
            }
            _ => {
                match signal {
                    NetworkSignal::SavedChanged => self.refresh_saved_ssids().await,
                    NetworkSignal::DevicesChanged => self.refresh_devices().await,
                    _ => {}
                }
                // Read D-Bus before taking the plain mutex; never hold it across an await.
                let mut next = self.build_state().await;
                let mut state = self.state.lock().unwrap();
                carry_across_rebuild(&mut next, &mut state, signal);
                *state = next;
                state.clone()
            }
        }
    }

    /// Freshly reads every `NetworkState` field except `scanning`. Each property falls back to
    /// `Default` on error, so one unreadable field does not abort the snapshot.
    async fn build_state(&self) -> NetworkState {
        let available_networks = self.build_available_networks().await;
        let associated = available_networks.iter().find(|ap| ap.active);
        // `PrimaryConnection` is `/` without a default route; its type says whether the route is
        // wired.
        let connected = self.nm.primary_connection().await.is_ok_and(|path| path.as_str() != "/");
        let wired = connected && self.nm.primary_connection_type().await.is_ok_and(|kind| kind == "802-3-ethernet");
        let wifi = self.wifi();
        let ethernet = self.activated_ethernet().await;
        let wifi_ip = read_ipv4(&self.connection, wifi.as_ref().map(|wifi| &wifi.device)).await;
        let ethernet_ip = read_ipv4(&self.connection, ethernet.as_ref().map(|ethernet| &ethernet.device)).await;
        NetworkState {
            scanning: false,
            connected,
            ssid: resolve_ssid(wired, associated),
            strength: associated.map_or(0, |ap| ap.strength),
            wifi_enabled: self.nm.wireless_enabled().await.unwrap_or_default(),
            wifi_present: wifi.is_some(),
            // `ethernet()` clones out, so no guard lives across the awaits below.
            ethernet_present: !self.ethernet().is_empty(),
            networking_enabled: self.nm.networking_enabled().await.unwrap_or_default(),
            ethernet_enabled: ethernet.is_some(),
            wifi_ip,
            ethernet_ip,
            ethernet_speed: match &ethernet {
                Some(ethernet) => ethernet.wired.speed().await.unwrap_or(0),
                None => 0,
            },
            // All three are owned by the connect path; see `carry_across_rebuild`.
            connecting_ssid: None,
            connect_error: None,
            password_ssid: None,
            available_networks,
        }
    }

    /// The first wired device that reached `ACTIVATED`. One active cable is enough for the Ethernet
    /// tile, regardless of the number of ports.
    async fn activated_ethernet(&self) -> Option<EthernetDevice> {
        for ethernet in self.ethernet() {
            match ethernet.device.state().await {
                Ok(DEVICE_STATE_ACTIVATED) => return Some(ethernet),
                Ok(_) => {}
                Err(err) => eprintln!("network: failed to read state for ethernet device {}: {err}", ethernet.path),
            }
        }
        None
    }

    /// The Wi-Fi device now, cloned out so no await holds the device lock.
    pub(super) fn wifi(&self) -> Option<WifiDevice> {
        self.devices.lock().unwrap().wifi.clone()
    }

    /// The wired devices now, cloned out like [`wifi`](Self::wifi).
    pub(super) fn ethernet(&self) -> Vec<EthernetDevice> {
        self.devices.lock().unwrap().ethernet.clone()
    }

    /// `NetworkingEnabled` is read-only; only `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled`
    /// have setters. Toggle it with `Enable(bool)`, not a direct property write.
    pub async fn set_networking_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.enable(enabled).await {
            eprintln!("network: failed to set networking_enabled={enabled}: {err}");
        }
    }

    /// `WirelessEnabled` is read-write.
    pub async fn set_wifi_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.set_wireless_enabled(enabled).await {
            eprintln!("network: failed to set wifi_enabled={enabled}: {err}");
        }
    }

    /// ADR-0029: `false` disconnects every wired device; `true` activates each existing
    /// autoconnect profile. A device with none is a no-op; NM cannot fabricate a connection.
    pub async fn set_ethernet_enabled(&self, enabled: bool) {
        for ethernet in self.ethernet() {
            if enabled {
                self.activate_autoconnect_profile(&ethernet.device, &ethernet.path).await;
            } else if let Err(err) = ethernet.device.disconnect().await {
                eprintln!("network: failed to disconnect ethernet device {}: {err}", ethernet.path);
            }
        }
    }

    /// `network:disconnect_wifi()`: `Device.Disconnect` on the Wi-Fi device. NetworkManager also stops
    /// autoconnect there until the user joins again, so the radio does not rejoin behind the click.
    pub async fn disconnect_wifi(&self) {
        let Some(wifi) = self.wifi() else {
            eprintln!("network: disconnect_wifi() requested but no Wi-Fi device is present");
            return;
        };
        if let Err(err) = wifi.device.disconnect().await {
            eprintln!("network: failed to disconnect the Wi-Fi device: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::JoinError;
    use super::*;

    #[test]
    fn a_rebuild_keeps_the_join_and_ends_a_scan_with_its_device() {
        let previous = NetworkState {
            scanning: true,
            wifi_present: true,
            connecting_ssid: Some("home".to_string()),
            connect_error: Some(JoinError { ssid: "home".to_string(), message: "wrong password".to_string() }),
            password_ssid: Some("home".to_string()),
            ..NetworkState::default()
        };
        let rebuilt = |wifi_present, signal| {
            let mut next = NetworkState { wifi_present, ..NetworkState::default() };
            carry_across_rebuild(&mut next, &mut previous.clone(), signal);
            next
        };

        assert_eq!(rebuilt(true, NetworkSignal::Changed), previous, "every carried field survives");
        assert!(!rebuilt(true, NetworkSignal::ScanCompleted).scanning);
        assert!(!rebuilt(false, NetworkSignal::DevicesChanged).scanning, "an unplugged adapter's scan never completes");
    }
}
