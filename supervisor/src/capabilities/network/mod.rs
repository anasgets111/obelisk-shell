//! NetworkManager D-Bus controller (`obelisk.network`; docs/services.md §4;
//! ADR-0029). It holds `rusty_network_manager` proxies (ADR-0013) and merges their signal streams
//! into `main.rs`'s top-level `tokio::select!`, like `dbus::polkit`, rather than using a dedicated
//! thread like `audio::mixer`.
//!
//! Forwarder tasks feed one channel: wireless APs/association, each device's state, the manager's
//! radio switches/default route, its device list, and saved-profile changes. ADR-0082: scan-only
//! watching left connected machines reading offline for minutes.
//!
//! A device added or removed after startup, such as a USB adapter, rescans the device set and
//! restarts its watchers ([`NetworkSignal::DevicesChanged`]).
//!
//! ponytail: only the first Wi-Fi device from `GetAllDevices` is tracked. Multiple adapters need a
//! device selector in `available_networks`/`scan`/`connect`; `docs/lua-api.md §2.5`
//! has none.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rusty_network_manager::dbus_interface_types::NMDeviceState;
use rusty_network_manager::{AccessPointProxy, DeviceProxy, IP4ConfigProxy, NetworkManagerProxy, SettingsProxy};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

mod connect;
mod devices;
mod profiles;
mod scan;

use connect::Attempt;
use devices::{Devices, EthernetDevice, WifiDevice, forward, resolve_devices, spawn_manager_forwarder, watch_devices};
use scan::resolve_ssid;

/// One scanned AP, resolved to `network.available_networks` (docs/lua-api.md §2.5)
/// and serialized in a `StateSnapshot` payload, same convention as `audio::mixer::AppStream`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AccessPointInfo {
    /// Network name. Entries dedupe on it, keeping the stronger sighting.
    pub ssid: String,
    /// Signal strength, `0` to `100`.
    pub strength: u8,
    /// A key is required: WEP privacy or non-empty WPA1/RSN key management.
    pub secure: bool,
    /// `"2.4 GHz"`, `"5 GHz"` or `"6 GHz"`, from the AP's frequency.
    pub band: String,
    /// This is the AP currently associated.
    pub active: bool,
    /// A saved NetworkManager profile names this SSID, so joining it asks for no password.
    pub saved: bool,
}

/// A failed join, as `network.connect_error`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct JoinError {
    /// The network the join was for.
    pub ssid: String,
    /// Display text, such as `"wrong password"` or `"network not found"`.
    pub message: String,
}

/// `obelisk.network`'s live §2.5 state, not only §4.2's scan results. Every field is re-derived from
/// NetworkManager on each [`NetworkSignal`] (ADR-0029: no debounce or incremental state).
///
/// The AP list cannot answer "am I online": it has no wired link and cannot distinguish a powered
/// down radio from a powered radio with no association.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetworkState {
    /// A scan is in flight. Set when `network:scan()` is accepted, before NetworkManager confirms,
    /// so the spinner starts on the click.
    pub scanning: bool,
    /// A connection carries the default route, from `PrimaryConnection` (§2.5). `/` means none,
    /// hence offline.
    pub connected: bool,
    /// Wi-Fi SSID, `"Ethernet"` for a wired default route, or `nil` with no association. Wired wins
    /// when both are up. An association negotiating DHCP has an `ssid` but `connected == false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// Associated AP strength, `0` to `100`, or `0` without Wi-Fi association. Read from the merged
    /// entry the panel draws, so the bar and list agree.
    pub strength: u8,
    /// Wi-Fi radio power, from `WirelessEnabled`; distinguishes radio-off from radio-on with no
    /// association.
    pub wifi_enabled: bool,
    /// A Wi-Fi device exists; NetworkManager reports `wifi_enabled` even with no hardware behind it.
    pub wifi_present: bool,
    /// At least one wired device exists, cable or not.
    pub ethernet_present: bool,
    /// Whether NetworkManager manages networking, from `NetworkingEnabled`. `false` means the
    /// other fields describe a switched-off stack.
    pub networking_enabled: bool,
    /// A wired device is activated. This is the setter's read-back; carrier stays up when a cable
    /// is seated, so it would not reflect `network:set_ethernet_enabled(false)`.
    pub ethernet_enabled: bool,
    /// The Wi-Fi device's IPv4 address without its prefix, or `nil` while it holds none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_ip: Option<String>,
    /// The first activated wired device's IPv4 address without its prefix, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethernet_ip: Option<String>,
    /// Link speed in Mb/s of the wired device `ethernet_ip` describes, or `0` when unknown.
    pub ethernet_speed: u32,
    /// SSID that `network:connect` is joining, or `nil`. Names the row whose spinner runs, and
    /// clears when the attempt reaches a verdict or `network:abort_connect` stops it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connecting_ssid: Option<String>,
    /// The last failed `network:connect`, or `nil` after success or before any attempt.
    /// `AddAndActivateConnection2` returns before the radio tries; this is filled later from the
    /// Wi-Fi device's `StateChanged` reason, where a wrong password is knowable.
    ///
    /// Sticky until the next attempt, like `UpdatesState::check_error`. It names its network, so a
    /// sheet opened for another one does not read a leftover failure as its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<JoinError>,
    /// SSID whose `network:connect` waits for a password, or `nil`. Set by
    /// [`resolve_connect_intent`](NetworkController::resolve_connect_intent) when no saved profile
    /// or open AP answers, and after NetworkManager rejects a key; cleared by the consuming attempt
    /// or `network:cancel_connect`.
    ///
    /// Kept here because "no profile for this SSID" lives in NetworkManager, not config (ADR-0037).
    /// The shell binds `keyboard_interactivity` to it, so focus lasts exactly while it names a
    /// network.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_ssid: Option<String>,
    /// Last completed scan: SSID-deduplicated, connected, then saved, then strongest, capped at 20.
    /// Kept while [`NetworkState::scanning`] is true so the panel does not blank; payload order is
    /// ready to draw.
    pub available_networks: Vec<AccessPointInfo>,
}

/// A pending `network:connect(ssid, hidden)` intent, stashed in the controller (ADR-0037) with
/// the same single-slot semantics the PAM one-shot protocol uses (ADR-0028), until paired
/// `secure_submit(network, connect)` supplies password bytes (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingNetworkConnect {
    pub ssid: String,
    pub hidden: bool,
}

/// What forwarders report to `main.rs`'s top-level `select!`; `build_state` makes the payload with
/// a fresh D-Bus round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSignal {
    /// Any non-`scanning` field change: AP set, association, device state, or radio. All trigger
    /// the same full re-derive (ADR-0029), so one variant is enough.
    Changed,
    /// `LastScan` changed, or NetworkManager refused `RequestScan`. Either way no scan is in flight.
    ScanCompleted,
    /// Sent by [`NetworkController::mark_scanning`] before `RequestScan` completes (§4.2), through
    /// the same channel for FIFO ordering.
    ScanStarted,
    /// A saved profile was added or removed, so the saved-SSID cache is stale.
    SavedChanged,
    /// NetworkManager added or removed a device, so the device set is stale.
    DevicesChanged,
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

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
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

/// Proxies needed by `obelisk.network`, resolved at construction. `Clone` is cheap for zbus handles,
/// so writes can move a clone into `tokio::spawn` (ADR-0029).
#[derive(Clone)]
pub struct NetworkController {
    connection: zbus::Connection,
    nm: NetworkManagerProxy<'static>,
    settings: SettingsProxy<'static>,
    /// The devices NetworkManager has now; see [`Devices`].
    devices: Arc<Mutex<Devices>>,
    /// AP proxies kept between rebuilds, keyed by object path. They retain zbus property caches fed
    /// by `PropertiesChanged`, avoiding a match rule, `GetAll`, and unsubscribe per AP per pass.
    /// At 10 APs, rebuild time fell from 11.25ms to 0.84ms (ADR-0082).
    ///
    /// Pruned against the live path list on each rebuild; `AccessPointRemoved` already requests it.
    access_points: Arc<Mutex<HashMap<OwnedObjectPath, AccessPointProxy<'static>>>>,
    /// Saved Wi-Fi SSIDs for [`AccessPointInfo::saved`], refreshed on
    /// [`NetworkSignal::SavedChanged`]. ponytail: an edited profile's SSID stays stale until the next
    /// add or remove. Upgrade path: watch each profile's `Updated`.
    saved_ssids: Arc<Mutex<HashSet<Vec<u8>>>>,
    /// `obelisk.network` push state (ADR-0037), mutated only by
    /// [`handle_signal`](Self::handle_signal).
    /// The cloned controller shares it; the mutex is never held across an await.
    state: Arc<Mutex<NetworkState>>,
    /// The single pending `network:connect` intent slot, see [`PendingNetworkConnect`].
    pending_connect: Arc<Mutex<Option<PendingNetworkConnect>>>,
    /// The attempt `connecting_ssid` names; see [`Attempt`]. Locked after `state` when both are held.
    attempt: Arc<Mutex<Attempt>>,
    /// Signal sender for [`mark_scanning`](Self::mark_scanning)'s FIFO event and for keeping the
    /// channel open when no Wi-Fi device exists.
    events: UnboundedSender<NetworkSignal>,
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

    /// Freshly reads every §2.5 field except `scanning`. Each property falls back to `Default` on
    /// error, so one unreadable field does not abort the snapshot.
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
                Ok(state) => {
                    if NMDeviceState::try_from(state) == Ok(NMDeviceState::ACTIVATED) {
                        return Some(ethernet);
                    }
                }
                Err(err) => eprintln!("network: failed to read state for ethernet device {}: {err}", ethernet.path),
            }
        }
        None
    }

    /// The Wi-Fi device now, cloned out so no await holds the device lock.
    fn wifi(&self) -> Option<WifiDevice> {
        self.devices.lock().unwrap().wifi.clone()
    }

    /// The wired devices now, cloned out like [`wifi`](Self::wifi).
    fn ethernet(&self) -> Vec<EthernetDevice> {
        self.devices.lock().unwrap().ethernet.clone()
    }

    /// § 4.1: `NetworkingEnabled` is read-only; only `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled`
    /// have setters. Toggle it with `Enable(bool)`, not the spec's literal property write.
    pub async fn set_networking_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.enable(enabled).await {
            eprintln!("network: failed to set networking_enabled={enabled}: {err}");
        }
    }

    /// § 4.1: `WirelessEnabled` is read-write.
    pub async fn set_wifi_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.set_wireless_enabled(enabled).await {
            eprintln!("network: failed to set wifi_enabled={enabled}: {err}");
        }
    }

    /// § 4.1 / ADR-0029: `false` disconnects every wired device; `true` activates each existing
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

/// Actions accepted by `obelisk.network:invoke(...)`; matching variants in `dispatch` keeps the
/// action table compiler-checked.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAction {
    SetNetworkingEnabled,
    SetWifiEnabled,
    SetEthernetEnabled,
    Scan,
    Connect,
    CancelConnect,
    AbortConnect,
    Forget,
    DisconnectWifi,
}

/// `obelisk.network` dispatch (ADR-0037). Writes spawn rather than await inline (ADR-0029);
/// `connect`
/// stashes its intent until paired `secure_submit(network, connect)`.
pub fn dispatch(controller: &NetworkController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<NetworkAction>(params) else { return };
    match action {
        NetworkAction::SetNetworkingEnabled | NetworkAction::SetWifiEnabled | NetworkAction::SetEthernetEnabled => {
            match crate::capabilities::parse_bool_arg(&params.arguments) {
                Some(enabled) => {
                    let controller = controller.clone();
                    tokio::spawn(async move {
                        match action {
                            NetworkAction::SetNetworkingEnabled => controller.set_networking_enabled(enabled).await,
                            NetworkAction::SetWifiEnabled => controller.set_wifi_enabled(enabled).await,
                            _ => controller.set_ethernet_enabled(enabled).await,
                        }
                    });
                }
                None => crate::log_malformed_command(params),
            }
        }
        NetworkAction::Scan => {
            controller.mark_scanning();
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.scan().await;
            });
        }
        NetworkAction::Connect => match parse_connect_args(&params.arguments) {
            Some((ssid, hidden)) => {
                controller.stash_connect_intent(PendingNetworkConnect { ssid, hidden });
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.resolve_connect_intent().await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        // Not spawned: it touches no D-Bus, and a late cancel would resurrect the prompt.
        NetworkAction::CancelConnect => controller.cancel_connect(),
        NetworkAction::AbortConnect => controller.abort_connect(),
        NetworkAction::Forget => match parse_ssid_arg(&params.arguments) {
            Some(ssid) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.forget(&ssid).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        NetworkAction::DisconnectWifi => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.disconnect_wifi().await;
            });
        }
    }
}

/// `network:connect(ssid, hidden)`'s `arguments: [ssid, hidden]`.
pub fn parse_connect_args(arguments: &[serde_json::Value]) -> Option<(String, bool)> {
    let ssid = arguments.first()?.as_str()?.to_string();
    let hidden = arguments.get(1)?.as_bool()?;
    Some((ssid, hidden))
}

/// `network:forget(ssid)`'s `arguments: [ssid]`.
pub fn parse_ssid_arg(arguments: &[serde_json::Value]) -> Option<String> {
    Some(arguments.first()?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn parse_connect_args_parses_ssid_and_hidden() {
        assert_eq!(
            parse_connect_args(&[serde_json::json!("HomeWifi"), serde_json::json!(true)]),
            Some(("HomeWifi".to_string(), true))
        );
    }

    #[test]
    fn parse_connect_args_rejects_a_malformed_shape() {
        assert_eq!(parse_connect_args(&[]), None, "missing both elements");
        assert_eq!(parse_connect_args(&[serde_json::json!(1), serde_json::json!(true)]), None, "ssid is not a string");
        assert_eq!(
            parse_connect_args(&[serde_json::json!("HomeWifi"), serde_json::json!("nope")]),
            None,
            "hidden is not a boolean"
        );
    }

    #[test]
    fn parse_ssid_arg_reads_the_first_argument() {
        assert_eq!(parse_ssid_arg(&[serde_json::json!("HomeWifi")]), Some("HomeWifi".to_string()));
        assert_eq!(parse_ssid_arg(&[]), None);
    }
}
