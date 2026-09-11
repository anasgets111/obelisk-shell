//! NetworkManager D-Bus controller (`oblisk.network`; docs/services.md §4;
//! ADR-0029). It holds `rusty_network_manager` proxies (ADR-0013) and merges their signal streams
//! into `main.rs`'s top-level `tokio::select!`, like `dbus::polkit`, rather than using a dedicated
//! thread like `audio::mixer`.
//!
//! Four forwarder tasks feed one channel: wireless APs/association, each device's state, and the
//! manager's radio switches/default route. ADR-0082: scan-only watching left connected machines
//! reading offline for minutes.
//!
//! ponytail: Wi-Fi/Ethernet devices resolve once at [`NetworkController::new`]; a USB dongle added
//! later needs a restart. Upgrade path: watch `device_added`/`device_removed` and rescan.
//!
//! ponytail: only the first Wi-Fi device from `GetAllDevices` is tracked. Multiple adapters need a
//! device selector in `available_networks`/`scan`/`connect`; `docs/lua-api.md §2.5`
//! has none.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rusty_network_manager::dbus_interface_types::{NMActiveConnectionState, NMDeviceState, NMDeviceType};
use rusty_network_manager::{
    AccessPointProxy, DeviceProxy, NetworkManagerProxy, SettingsConnectionProxy, SettingsProxy, WirelessProxy,
};
use serde::Serialize;
use shared::Zeroize;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

pub mod connection;

use connection::ConnectError;
use connection::{
    ConnectionIntent, access_point_is_secure, build_connection_dict, connect_error_text, connection_intent,
    connection_wants_autoconnect, dedup_and_top20, merge_psk, resolve_band, resolve_ssid, settings_match_ssid,
};
pub use connection::{parse_bool_arg, parse_connect_args, parse_ssid_arg};

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
}

/// `oblisk.network`'s live §2.5 state, not only §4.2's scan results. Every field is re-derived from
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
    /// Whether NetworkManager manages networking, from `NetworkingEnabled`. `false` means the
    /// other fields describe a switched-off stack.
    pub networking_enabled: bool,
    /// A wired device is activated. This is the setter's read-back; carrier stays up when a cable
    /// is seated, so it would not reflect `network:set_ethernet_enabled(false)`.
    pub ethernet_enabled: bool,
    /// SSID that `network:connect` is joining, or `nil`. Names the row whose spinner runs and
    /// clears when the attempt reaches either verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connecting_ssid: Option<String>,
    /// Display text for the last failed `network:connect`, or `nil` after success or before any
    /// attempt. `AddAndActivateConnection2` returns before the radio tries; this is filled later
    /// from the activation's `StateChanged(state, reason)`, where a wrong password is knowable.
    ///
    /// Sticky until the next attempt, like `UpdatesState::check_error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
    /// SSID whose `network:connect` waits for a password, or `nil`. Set by
    /// [`resolve_connect_intent`](NetworkController::resolve_connect_intent) only when needed;
    /// cleared by the consuming attempt or `network:cancel_connect`.
    ///
    /// Kept here because "no profile for this SSID" lives in NetworkManager, not config (ADR-0037).
    /// The shell binds `keyboard_interactivity` to it, so focus lasts exactly while it names a
    /// network.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_ssid: Option<String>,
    /// Last completed scan: SSID-deduplicated, connected first, then strongest, capped at 20. Kept
    /// while [`NetworkState::scanning`] is true so the panel does not blank; payload order is ready
    /// to draw.
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
    /// `LastScan` changed, so a Supervisor-triggered scan finished.
    ScanCompleted,
    /// Sent by [`NetworkController::mark_scanning`] before `RequestScan` completes (§4.2), through
    /// the same channel for FIFO ordering.
    ScanStarted,
}

/// [`NetworkController::watch_activation`]'s backstop timeout. NetworkManager normally gives up
/// well inside 45s and reports `StateChanged`; this covers an activation object that stops
/// answering without pre-empting NM or leaving a spinner stuck.
const ACTIVATION_CEILING: std::time::Duration = std::time::Duration::from_secs(45);

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
}

/// The crate's hand-written `<Proxy>::new_from_path` ties the proxy lifetime to `&Connection`, even
/// though its builder clones the connection. Call the builder directly so stored proxies are
/// `'static`.
async fn bind_device(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<DeviceProxy<'static>> {
    DeviceProxy::builder(connection).path(path)?.build().await
}

async fn bind_wireless(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<WirelessProxy<'static>> {
    WirelessProxy::builder(connection).path(path)?.build().await
}

async fn bind_access_point(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<AccessPointProxy<'static>> {
    AccessPointProxy::builder(connection).path(path)?.build().await
}

/// Hand-written `org.freedesktop.NetworkManager.Connection.Active` proxy. In crate 0.7.1,
/// `ActiveProxy` declares `state_changed`; zbus uses that explicit name verbatim, so it subscribes
/// to a member NetworkManager never emits. Twenty activations reaching `ACTIVATED` in about one
/// second produced no signal. The sibling `Device` proxy uses `StateChanged` and works, proving an
/// upstream typo rather than a convention (ADR-0013 still requires the crate where possible).
///
/// Only the needed members are declared. The `state` property also avoids a generated
/// `receive_state_changed` collision with the signal.
#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager",
    assume_defaults = false
)]
trait ActiveConnection {
    #[zbus(signal, name = "StateChanged")]
    fn active_state_changed(&self, state: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

async fn bind_active_connection(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<ActiveConnectionProxy<'static>> {
    ActiveConnectionProxy::builder(connection).path(path)?.build().await
}

async fn bind_settings_connection(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<SettingsConnectionProxy<'static>> {
    SettingsConnectionProxy::builder(connection).path(path)?.build().await
}

/// Reads one access point into the shape `network.available_networks` wants. `active` is passed
/// in rather than derived here: it is a fact about the device's association, not about the access
/// point, and only the caller holds it.
async fn read_access_point(ap: &AccessPointProxy<'static>, active: bool) -> Option<AccessPointInfo> {
    let ssid_bytes = ap.ssid().await.ok()?;
    if ssid_bytes.is_empty() {
        // ponytail: an empty hidden-AP SSID cannot be shown or deduped; including it collapses all
        // hidden APs into one `""` row. Connect still works with `hidden=true`.
        return None;
    }
    let strength = ap.strength().await.ok()?;
    let frequency = ap.frequency().await.ok()?;
    let flags = ap.flags().await.unwrap_or(0);
    let wpa_flags = ap.wpa_flags().await.unwrap_or(0);
    let rsn_flags = ap.rsn_flags().await.unwrap_or(0);
    Some(AccessPointInfo {
        ssid: String::from_utf8_lossy(&ssid_bytes).into_owned(),
        strength,
        secure: access_point_is_secure(flags, wpa_flags, rsn_flags),
        band: resolve_band(frequency).unwrap_or_default().to_string(),
        active,
    })
}

/// One resolved Wi-Fi device: its path for `AddAndActivateConnection2`, plus the `Device` and
/// `Device.Wireless` proxies. Association state and APs use those two interfaces on one object.
#[derive(Clone)]
struct WifiDevice {
    device_path: OwnedObjectPath,
    device: DeviceProxy<'static>,
    wireless: WirelessProxy<'static>,
}

/// One resolved Ethernet device. Its path feeds `ActivateConnection`; its proxy feeds state and
/// `ethernet_enabled`.
#[derive(Clone)]
struct EthernetDevice {
    path: OwnedObjectPath,
    device: DeviceProxy<'static>,
}

/// One saved profile matched by SSID: the path for `ActivateConnection`, its proxy, and the
/// settings already read by both callers.
struct SavedProfile {
    path: OwnedObjectPath,
    connection: SettingsConnectionProxy<'static>,
    settings: HashMap<String, HashMap<String, OwnedValue>>,
}

/// Proxies needed by `oblisk.network`, resolved at construction. `Clone` is cheap for zbus handles,
/// so writes can move a clone into `tokio::spawn` (ADR-0029).
#[derive(Clone)]
pub struct NetworkController {
    connection: zbus::Connection,
    nm: NetworkManagerProxy<'static>,
    settings: SettingsProxy<'static>,
    wifi: Option<WifiDevice>,
    ethernet: Vec<EthernetDevice>,
    /// AP proxies kept between rebuilds, keyed by object path. They retain zbus property caches fed
    /// by `PropertiesChanged`, avoiding a match rule, `GetAll`, and unsubscribe per AP per pass.
    /// At 10 APs, rebuild time fell from 11.25ms to 0.84ms (ADR-0082).
    ///
    /// Pruned against the live path list on each rebuild; `AccessPointRemoved` already requests it.
    access_points: Arc<Mutex<HashMap<OwnedObjectPath, AccessPointProxy<'static>>>>,
    /// `oblisk.network` push state (ADR-0037), mutated only by
    /// [`handle_signal`](Self::handle_signal).
    /// The cloned controller shares it; the mutex is never held across an await.
    state: Arc<Mutex<NetworkState>>,
    /// The single pending `network:connect` intent slot, see [`PendingNetworkConnect`].
    pending_connect: Arc<Mutex<Option<PendingNetworkConnect>>>,
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

        let mut wifi = None;
        let mut ethernet = Vec::new();
        for path in nm.get_all_devices().await? {
            let device = match bind_device(&connection, path.clone()).await {
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
            match NMDeviceType::try_from(device_type) {
                Ok(NMDeviceType::ETHERNET) => ethernet.push(EthernetDevice { path, device }),
                Ok(NMDeviceType::WIFI) if wifi.is_none() => match bind_wireless(&connection, path.clone()).await {
                    Ok(wireless) => wifi = Some(WifiDevice { device_path: path, device, wireless }),
                    Err(err) => eprintln!("network: failed to bind wireless device {path}: {err}"),
                },
                _ => {}
            }
        }

        match &wifi {
            Some(wifi) => {
                spawn_wifi_forwarder(connection.clone(), wifi.wireless.clone(), events.clone());
                spawn_device_state_forwarder(wifi.device.clone(), events.clone());
            }
            None => eprintln!("network: no Wi-Fi device found; scan/access-point events are disabled for this session"),
        }
        for device in &ethernet {
            spawn_device_state_forwarder(device.device.clone(), events.clone());
        }
        spawn_manager_forwarder(nm.clone(), events.clone());

        Ok(Self {
            connection,
            nm,
            settings,
            wifi,
            ethernet,
            access_points: Arc::new(Mutex::new(HashMap::new())),
            state: Arc::new(Mutex::new(NetworkState::default())),
            pending_connect: Arc::new(Mutex::new(None)),
            events,
        })
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
            NetworkSignal::ScanCompleted | NetworkSignal::Changed => {
                // Read D-Bus before taking the plain mutex; never hold it across an await.
                let mut next = self.build_state().await;
                let mut state = self.state.lock().unwrap();
                next.scanning = signal == NetworkSignal::Changed && state.scanning;
                // These are attempt memory, not NetworkManager readings, so carry them across the
                // re-derive like `scanning`. Take them because `state` is overwritten below.
                next.connecting_ssid = state.connecting_ssid.take();
                next.connect_error = state.connect_error.take();
                next.password_ssid = state.password_ssid.take();
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
        NetworkState {
            scanning: false,
            connected,
            ssid: resolve_ssid(wired, associated),
            strength: associated.map_or(0, |ap| ap.strength),
            wifi_enabled: self.nm.wireless_enabled().await.unwrap_or_default(),
            networking_enabled: self.nm.networking_enabled().await.unwrap_or_default(),
            ethernet_enabled: self.ethernet_is_activated().await,
            // All three are owned by the connect path and reinstated by the caller; see
            // `handle_signal`.
            connecting_ssid: None,
            connect_error: None,
            password_ssid: None,
            available_networks,
        }
    }

    /// Whether any wired device reached `ACTIVATED`. One active cable is enough for the Ethernet
    /// row, regardless of the number of ports.
    async fn ethernet_is_activated(&self) -> bool {
        for ethernet in &self.ethernet {
            match ethernet.device.state().await {
                Ok(state) => {
                    if NMDeviceState::try_from(state) == Ok(NMDeviceState::ACTIVATED) {
                        return true;
                    }
                }
                Err(err) => eprintln!("network: failed to read state for ethernet device {}: {err}", ethernet.path),
            }
        }
        false
    }

    /// Queues [`NetworkSignal::ScanStarted`] so `scanning` flips on initiation, before
    /// `RequestScan`. Only does so with Wi-Fi hardware; otherwise [`scan`](Self::scan) no-ops and
    /// `scanning` would stick at `true`.
    pub fn mark_scanning(&self) {
        if self.wifi.is_some() {
            let _ = self.events.send(NetworkSignal::ScanStarted);
        }
    }

    /// Stashes `network:connect(ssid, hidden)` until paired `secure_submit(network, connect)`.
    /// Newest intent wins.
    pub fn stash_connect_intent(&self, pending: PendingNetworkConnect) {
        *self.pending_connect.lock().unwrap() = Some(pending);
    }

    /// Takes the pending intent for the `secure_submit(network, connect)` consumer.
    pub fn take_connect_intent(&self) -> Option<PendingNetworkConnect> {
        self.pending_connect.lock().unwrap().take()
    }

    /// Decides whether a stashed `network:connect` can complete or needs a password.
    ///
    /// A saved profile or open AP connects on click with no typed secret, matching
    /// `NetworkPanel.qml`'s bare `connectToSsid(ssid, "")` for known/unsecured rows. Only a secured
    /// network without a profile sets [`NetworkState::password_ssid`] and waits for
    /// `secure_submit(network, connect)`.
    ///
    /// Without those branches, `network:connect` only stashes and every click leaves an intent
    /// nothing consumes.
    ///
    /// Hidden networks take the password branch because no in-range AP reports their security.
    /// QML's `showPasswordInput` also defaults to `true` (`?? true`); guessing wrong costs one
    /// keystroke on an open network, versus an unjoinable secured one.
    ///
    /// The SSID is looked up again in `activate_intent`, an extra `ListConnections` walk of a few
    /// profiles. Passing the match through `connect` saves about a millisecond at the cost of three
    /// signatures.
    pub async fn resolve_connect_intent(&self) {
        let Some(pending) = self.pending_connect.lock().unwrap().clone() else {
            return;
        };
        let saved = !self.saved_profiles_for_ssid(&pending.ssid, "connect").await.is_empty();
        // No in-range AP means secured, like a hidden SSID: nothing can say otherwise.
        let secure = pending.hidden
            || self
                .state
                .lock()
                .unwrap()
                .available_networks
                .iter()
                .find(|ap| ap.ssid == pending.ssid)
                .is_none_or(|ap| ap.secure);
        if !saved && secure {
            // Log the fork: saved-profile and security facts come from different sources, so a
            // missing prompt otherwise leaves three plausible causes.
            eprintln!("network: connect {:?}: saved={saved} secure={secure}, asking for a password", pending.ssid);
            self.request_password(&pending.ssid);
            return;
        }
        eprintln!("network: connect {:?}: saved={saved} secure={secure}, connecting directly", pending.ssid);
        // Re-take it because another connect may have replaced it while the lookup was on the wire.
        if let Some(pending) = self.take_connect_intent() {
            self.connect(pending, Vec::new()).await;
        }
    }

    /// Shows the password prompt and pushes it immediately. The intent stays stashed for
    /// `secure_submit(network, connect)`.
    fn request_password(&self, ssid: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.password_ssid = Some(ssid.to_string());
            state.connect_error = None;
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// `network:cancel_connect()`: drops the pending intent and password prompt.
    ///
    /// The prompt's way out. Escape in a `secure_submit` field only clears its text and stays in
    /// the field (`wayland::input`'s `SecureKeyAction::Clear`), so without this a mis-click would
    /// hold bar keyboard focus.
    ///
    /// ponytail: an activation already in flight is untouched, although `NetworkService.qml`'s
    /// `cancelConnect` disconnects when nothing else is live. Letting NM finish costs seconds;
    /// racing it can disconnect a session that just came up.
    ///
    /// No-op without a pending prompt, so `modules/shell/panel_host.lua` can call it on any panel
    /// close. Without the guard, closing another panel would clear `connect_error` and push a
    /// misleading `Changed`.
    pub fn cancel_connect(&self) {
        let pending = self.pending_connect.lock().unwrap().take();
        if pending.is_none() && self.state.lock().unwrap().password_ssid.is_none() {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            state.password_ssid = None;
            state.connect_error = None;
        }
        eprintln!("network: the pending connect was cancelled; the password prompt is down");
        let _ = self.events.send(NetworkSignal::Changed);
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
        for ethernet in &self.ethernet {
            if enabled {
                self.activate_autoconnect_profile(&ethernet.device, &ethernet.path).await;
            } else if let Err(err) = ethernet.device.disconnect().await {
                eprintln!("network: failed to disconnect ethernet device {}: {err}", ethernet.path);
            }
        }
    }

    async fn activate_autoconnect_profile(&self, device: &DeviceProxy<'_>, device_path: &OwnedObjectPath) {
        let profile = match self.find_autoconnect_profile(device).await {
            Ok(profile) => profile,
            Err(err) => {
                eprintln!("network: failed to inspect connections for ethernet device {device_path}: {err}");
                return;
            }
        };
        let Some(conn_path) = profile else {
            // No profile exists; D-Bus cannot create one here (ADR-0029).
            return;
        };
        if let Err(err) = self.nm.activate_connection(&conn_path, device_path, &root_object_path()).await {
            eprintln!("network: failed to activate ethernet profile {conn_path} on {device_path}: {err}");
        }
    }

    async fn find_autoconnect_profile(&self, device: &DeviceProxy<'_>) -> zbus::Result<Option<OwnedObjectPath>> {
        for conn_path in device.available_connections().await? {
            let conn = match bind_settings_connection(&self.connection, conn_path.clone()).await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!(
                        "network: failed to bind connection {conn_path} while searching for an autoconnect profile: {err}"
                    );
                    continue;
                }
            };
            let settings = match conn.get_settings().await {
                Ok(settings) => settings,
                Err(err) => {
                    eprintln!(
                        "network: failed to read settings for {conn_path} while searching for an autoconnect profile: {err}"
                    );
                    continue;
                }
            };
            if connection_wants_autoconnect(&settings) {
                return Ok(Some(conn_path));
            }
        }
        Ok(None)
    }

    /// § 4.2: dispatches `RequestScan({})`. Missing Wi-Fi hardware is logged, not fatal.
    pub async fn scan(&self) {
        let Some(wifi) = &self.wifi else {
            eprintln!("network: scan() requested but no Wi-Fi device is present");
            return;
        };
        if let Err(err) = wifi.wireless.request_scan(HashMap::new()).await {
            eprintln!("network: RequestScan failed: {err}");
        }
    }

    /// § 4.2: re-queries, deduplicates, and caps the current AP list at 20 by strength (ADR-0029:
    /// no debounce). Returns empty, not an error, without Wi-Fi hardware.
    pub async fn build_available_networks(&self) -> Vec<AccessPointInfo> {
        let Some(wifi) = &self.wifi else {
            return Vec::new();
        };
        let active_path = wifi.wireless.active_access_point().await.ok();
        let ap_paths = match wifi.wireless.get_access_points().await {
            Ok(paths) => paths,
            Err(err) => {
                eprintln!("network: failed to list access points: {err}");
                return Vec::new();
            }
        };

        let access_points = self.warm_access_points(&ap_paths).await;
        let mut aps = Vec::with_capacity(access_points.len());
        for (path, proxy) in &access_points {
            if let Some(ap) = read_access_point(proxy, active_path.as_ref() == Some(path)).await {
                aps.push(ap);
            }
        }
        dedup_and_top20(aps)
    }

    /// Binds missing `paths`, drops held paths no longer in range, and returns live proxies in path
    /// order. Returned clones share each held proxy's property cache.
    ///
    /// Takes the lock around, not across, binding because it is a plain mutex and binding awaits.
    async fn warm_access_points(&self, paths: &[OwnedObjectPath]) -> Vec<(OwnedObjectPath, AccessPointProxy<'static>)> {
        let missing: Vec<OwnedObjectPath> = {
            let held = self.access_points.lock().unwrap();
            paths.iter().filter(|path| !held.contains_key(*path)).cloned().collect()
        };
        let mut bound = Vec::with_capacity(missing.len());
        for path in missing {
            match bind_access_point(&self.connection, path.clone()).await {
                Ok(proxy) => bound.push((path, proxy)),
                Err(err) => eprintln!("network: failed to bind access point {path}: {err}"),
            }
        }

        let in_range: HashSet<&OwnedObjectPath> = paths.iter().collect();
        let mut held = self.access_points.lock().unwrap();
        held.extend(bound);
        held.retain(|path, _| in_range.contains(path));
        paths.iter().filter_map(|path| Some((path.clone(), held.get(path)?.clone()))).collect()
    }

    /// Supervisor services §4: turns `pending` and `secret` (empty open, non-empty WPA-PSK) into
    /// `AddAndActivateConnection2`'s dict. The caller `mem::take`s `secret` from the wire frame,
    /// making this function its owner; every outcome zeroizes it (ADR-0005/ADR-0014).
    pub async fn connect(&self, pending: PendingNetworkConnect, mut secret: Vec<u8>) {
        self.begin_connect(&pending.ssid);
        let result = self.connect_inner(&pending, &secret).await;
        secret.zeroize();
        match result {
            // NM accepted the request, not completed it; the activation reports the verdict.
            Ok(active) => self.watch_activation(active, pending.ssid),
            Err(err) => {
                eprintln!("network: connect(ssid={:?}) failed: {err}", pending.ssid);
                self.finish_connect(&pending.ssid, Some(err.to_string()));
            }
        }
    }

    /// Marks an attempt, clears its previous error, and pushes through the same FIFO as scanning so
    /// the row spins on the click.
    fn begin_connect(&self, ssid: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.connecting_ssid = Some(ssid.to_string());
            state.connect_error = None;
            // The attempt answers the prompt. Clear here, not only in `secure_submit`, so direct
            // connects also drop bar keyboard focus on Enter.
            state.password_ssid = None;
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// Records and pushes an attempt's verdict. Drops a verdict for a different in-flight SSID, so
    /// an older failure cannot land on a newer spinner; `connect` need not refuse overlap.
    fn finish_connect(&self, ssid: &str, error: Option<String>) {
        {
            let mut state = self.state.lock().unwrap();
            if state.connecting_ssid.as_deref() != Some(ssid) {
                return;
            }
            state.connecting_ssid = None;
            state.connect_error = error;
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// Watches one activation in the background.
    fn watch_activation(&self, active: OwnedObjectPath, ssid: String) {
        let controller = self.clone();
        tokio::spawn(async move {
            let error = match tokio::time::timeout(ACTIVATION_CEILING, controller.activation_outcome(&active)).await {
                Ok(error) => error,
                Err(_) => Some("connection timed out".to_string()),
            };
            controller.finish_connect(&ssid, error);
        });
    }

    /// `None` on `ACTIVATED`, reason text on deactivation.
    async fn activation_outcome(&self, active: &OwnedObjectPath) -> Option<String> {
        let generic = || Some("connection failed".to_string());
        let proxy = match bind_active_connection(&self.connection, active.clone()).await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("network: failed to bind the active connection {active}: {err}");
                return generic();
            }
        };
        // Use the signal, not `receive_state_changed()`: the property stream gives no reason.
        let mut changes = match proxy.receive_active_state_changed().await {
            Ok(changes) => changes,
            Err(err) => {
                eprintln!("network: failed to subscribe to StateChanged on {active}: {err}");
                return generic();
            }
        };

        // Subscription follows activation, so a verdict can land in the gap. Read the property
        // once.
        //
        // ponytail: a failure in that gap loses its reason and reports the generic line; only the
        // signal carries it. Success does not, and is the likelier race.
        match proxy.state().await.map(NMActiveConnectionState::try_from) {
            Ok(Ok(NMActiveConnectionState::ACTIVATED)) => return None,
            Ok(Ok(NMActiveConnectionState::DEACTIVATED)) => return generic(),
            _ => {}
        }

        while let Some(change) = changes.next().await {
            let Ok(args) = change.args() else { continue };
            match NMActiveConnectionState::try_from(args.state) {
                Ok(NMActiveConnectionState::ACTIVATED) => return None,
                Ok(NMActiveConnectionState::DEACTIVATED) => return Some(connect_error_text(args.reason).to_string()),
                _ => {}
            }
        }
        // The object disappeared without a terminal state.
        generic()
    }

    async fn connect_inner(
        &self,
        pending: &PendingNetworkConnect,
        secret: &[u8],
    ) -> Result<OwnedObjectPath, ConnectError> {
        let wifi = self.wifi.as_ref().ok_or(ConnectError::NoWifiDevice)?;
        let mut intent = connection_intent(&pending.ssid, pending.hidden, secret)?;
        let result = self.activate_intent(&intent, wifi).await;
        // Dicts borrow this plaintext PSK and are consumed now, so zeroize it explicitly
        // (ADR-0005/ADR-0014) rather than relying on Drop.
        if let Some(psk) = intent.psk.as_mut() {
            psk.zeroize();
        }
        result
    }

    /// Joins `intent`'s network, reusing a saved profile when present. NM does not deduplicate:
    /// `AddAndActivateConnection2` accepts another profile with the same id and SSID, so creating
    /// unconditionally left stale duplicates that autoconnect could choose. Returns the activation
    /// path where its outcome is reported.
    async fn activate_intent(
        &self,
        intent: &ConnectionIntent,
        wifi: &WifiDevice,
    ) -> Result<OwnedObjectPath, ConnectError> {
        let Some(saved) = self.saved_profiles_for_ssid(&intent.ssid, "connect").await.into_iter().next() else {
            let dict = build_connection_dict(intent);
            let (_, active, _) = self
                .nm
                .add_and_activate_connection2(dict, &wifi.device_path, &root_object_path(), HashMap::new())
                .await?;
            return Ok(active);
        };

        // A typed password corrects the saved key; otherwise a bad profile could only be forgotten
        // and re-added.
        //
        // ponytail: skip enterprise profiles. `GetSettings` omits secrets, so rebuilding would drop
        // the 802.1X password; use NM's saved copy until a secret agent exists (ADR-0029).
        if let Some(psk) = &intent.psk
            && !saved.settings.contains_key("802-1x")
        {
            saved.connection.update(merge_psk(&saved.settings, psk)).await?;
        }
        Ok(self.nm.activate_connection(&saved.path, &wifi.device_path, &root_object_path()).await?)
    }

    /// Every saved Wi-Fi profile for `ssid`, paired with the settings dict that matched it.
    /// `context` identifies the caller in logs. Plural because §4.3 `forget` deletes all while
    /// `connect` takes the first; one `ListConnections` walk serves both.
    async fn saved_profiles_for_ssid(&self, ssid: &str, context: &str) -> Vec<SavedProfile> {
        let paths = match self.settings.list_connections().await {
            Ok(paths) => paths,
            Err(err) => {
                eprintln!("network: {context}({ssid:?}) failed to list connections: {err}");
                return Vec::new();
            }
        };

        let mut matches = Vec::new();
        for path in paths {
            let connection = match bind_settings_connection(&self.connection, path.clone()).await {
                Ok(connection) => connection,
                Err(err) => {
                    eprintln!("network: {context}({ssid:?}) failed to bind connection {path}: {err}");
                    continue;
                }
            };
            let settings = match connection.get_settings().await {
                Ok(settings) => settings,
                Err(err) => {
                    eprintln!("network: {context}({ssid:?}) failed to read settings for {path}: {err}");
                    continue;
                }
            };
            if settings_match_ssid(&settings, ssid) {
                matches.push(SavedProfile { path, connection, settings });
            }
        }
        matches
    }

    /// Supervisor services §4: deletes every connection profile matching `ssid`.
    pub async fn forget(&self, ssid: &str) {
        for profile in self.saved_profiles_for_ssid(ssid, "forget").await {
            if let Err(err) = profile.connection.delete().await {
                eprintln!("network: forget({ssid:?}) failed to delete profile {}: {err}", profile.path);
            }
        }
    }
}

/// Actions accepted by `oblisk.network:invoke(...)`; matching variants in `dispatch` keeps the
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
    Forget,
}

/// `oblisk.network` dispatch (ADR-0037). Writes spawn rather than await inline (ADR-0029);
/// `connect`
/// stashes its intent until paired `secure_submit(network, connect)`.
pub fn dispatch(controller: &NetworkController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<NetworkAction>(params) else { return };
    match action {
        NetworkAction::SetNetworkingEnabled => match parse_bool_arg(&params.arguments) {
            Some(enabled) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_networking_enabled(enabled).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        NetworkAction::SetWifiEnabled => match parse_bool_arg(&params.arguments) {
            Some(enabled) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_wifi_enabled(enabled).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        NetworkAction::SetEthernetEnabled => match parse_bool_arg(&params.arguments) {
            Some(enabled) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_ethernet_enabled(enabled).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
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
        NetworkAction::Forget => match parse_ssid_arg(&params.arguments) {
            Some(ssid) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.forget(&ssid).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
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
) {
    tokio::spawn(async move {
        let mut ap_added = match wireless.receive_access_point_added().await {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("network: failed to subscribe to AccessPointAdded: {err}");
                return;
            }
        };
        let mut ap_removed = match wireless.receive_access_point_removed().await {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("network: failed to subscribe to AccessPointRemoved: {err}");
                return;
            }
        };
        let mut last_scan_changed = wireless.receive_last_scan_changed().await;
        let mut active_ap_changed = wireless.receive_active_access_point_changed().await;
        // The first `active_ap_changed` emission fills the property cache, so an existing
        // association is watched without a startup read.
        let mut strength: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            tokio::select! {
                Some(_) = ap_added.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(_) = ap_removed.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(change) = active_ap_changed.next() => {
                    if let Some(handle) = strength.take() { handle.abort(); }
                    if let Ok(path) = change.get().await {
                        strength = spawn_strength_forwarder(&connection, path, events.clone());
                    }
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(_) = last_scan_changed.next() => {
                    if events.send(NetworkSignal::ScanCompleted).is_err() { break; }
                }
                else => break,
            }
        }
    });
}

/// Forwards the associated AP's `Strength` as [`NetworkSignal::Changed`], keeping bars current
/// between scans. `None` for NetworkManager's `/` path means no association.
///
/// Watch only the associated AP. On real hardware over 180s it emitted 26 times, a quiet 6-second
/// poll, versus 76 events across 17 APs, one every 2.4s indefinitely. Rebuilds reread every AP,
/// so the full list stayed as fresh at one third the traffic; ADR-0029 item 6 required this
/// measured choice before adding debounce.
fn spawn_strength_forwarder(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
    events: UnboundedSender<NetworkSignal>,
) -> Option<tokio::task::JoinHandle<()>> {
    if path.as_str() == "/" {
        return None;
    }
    let connection = connection.clone();
    Some(tokio::spawn(async move {
        let access_point = match bind_access_point(&connection, path.clone()).await {
            Ok(access_point) => access_point,
            Err(err) => {
                eprintln!("network: failed to bind the associated access point {path}: {err}");
                return;
            }
        };
        let mut strength_changed = access_point.receive_strength_changed().await;
        while strength_changed.next().await.is_some() {
            if events.send(NetworkSignal::Changed).is_err() {
                break;
            }
        }
    }))
}

/// Forwards each device's `State` as [`NetworkSignal::Changed`]. Per-device tasks cover
/// `ethernet_enabled` and announce Wi-Fi disconnects before `ActiveAccessPoint` catches up.
/// zbus emits the cached current value once, priming the first snapshot without a startup read.
fn spawn_device_state_forwarder(device: DeviceProxy<'static>, events: UnboundedSender<NetworkSignal>) {
    tokio::spawn(async move {
        let mut state_changed = device.receive_state_changed().await;
        while state_changed.next().await.is_some() {
            if events.send(NetworkSignal::Changed).is_err() {
                break;
            }
        }
    });
}

/// Forwards manager-wide properties used by `NetworkState`: radio switches and the default route.
/// Device tasks cannot cover them; networking can switch off while devices stay put, and the route
/// can move between two activated devices.
fn spawn_manager_forwarder(nm: NetworkManagerProxy<'static>, events: UnboundedSender<NetworkSignal>) {
    tokio::spawn(async move {
        let mut wireless_enabled = nm.receive_wireless_enabled_changed().await;
        let mut networking_enabled = nm.receive_networking_enabled_changed().await;
        let mut primary_connection = nm.receive_primary_connection_changed().await;

        loop {
            tokio::select! {
                Some(_) = wireless_enabled.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(_) = networking_enabled.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(_) = primary_connection.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                else => break,
            }
        }
    });
}
