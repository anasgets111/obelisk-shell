//! NetworkManager D-Bus controller (`oblisk.network`, build-steps.md Phase 16;
//! docs/oblisk-supervisor-services-dbus.md §4; docs/adr/0029).
//!
//! Mirrors `dbus::polkit`'s structure (a controller holding proxies, exposing async methods per
//! IDL write action) rather than `audio::mixer`'s dedicated-thread pattern: NetworkManager's API
//! is D-Bus-native, so its signal streams merge into `main.rs`'s top-level `tokio::select!`
//! instead of needing a thread of their own (ADR-0029).
//!
//! D-Bus access goes through `rusty_network_manager` (ADR-0013, ADR-0029) rather than
//! hand-written proxies.
//!
//! ponytail: the Wi-Fi and Ethernet device set is resolved once, at [`NetworkController::new`]
//! time, never re-discovered -- a USB Wi-Fi dongle plugged in after the Supervisor starts
//! wouldn't be picked up until restart. Upgrade path: subscribe to
//! `NetworkManagerProxy::device_added`/`device_removed` and re-run the same scan.
//!
//! ponytail: exactly one Wi-Fi device is tracked (the first `GetAllDevices` returns). Multiple
//! simultaneous adapters would need `available_networks`/`scan`/`connect` to carry a device
//! selector; nothing in docs/oblisk-idl-api-specs.md §2.5's schema has one yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusty_network_manager::dbus_interface_types::NMDeviceType;
use rusty_network_manager::{
    AccessPointProxy, DeviceProxy, NetworkManagerProxy, SettingsConnectionProxy, SettingsProxy, WirelessProxy,
};
use serde::Serialize;
use shared::Zeroize;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

pub mod connection;

use connection::ConnectError;
use connection::{
    access_point_is_secure, build_connection_dict, connection_intent, connection_wants_autoconnect, dedup_and_top20,
    resolve_band, settings_match_ssid,
};
pub use connection::{parse_bool_arg, parse_connect_args, parse_ssid_arg};

/// One scanned access point, already resolved to what `network.available_networks` needs
/// (docs/oblisk-idl-api-specs.md §2.5). `Serialize`: this is what ends up in a `StateSnapshot`'s
/// `payload`, same convention as `audio::mixer::AppStream`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AccessPointInfo {
    /// The network name. The key entries are deduplicated on, so two radios of one network appear
    /// once, at the stronger signal.
    pub ssid: String,
    /// Signal strength, `0` to `100`.
    pub strength: u8,
    /// A key is required: the AP advertises WEP privacy, or non-empty WPA1 or RSN key management.
    pub secure: bool,
    /// `"2.4 GHz"`, `"5 GHz"` or `"6 GHz"`, from the AP's frequency.
    pub band: String,
    /// This is the AP currently associated.
    pub active: bool,
}

/// `oblisk.network`'s live push state (docs/adr/0029). Scoped to exactly what §4.2 asks for --
/// scanning status and the deduplicated AP list -- not the full §2.5 read schema
/// (`connected`/`ssid`/`wifi_enabled`/etc.), which §4 doesn't ask this controller to track.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetworkState {
    /// A scan is in flight. Flipped to `true` the moment `network:scan()` is accepted rather than
    /// when NetworkManager confirms, so a spinner starts on the click instead of a round trip later.
    pub scanning: bool,
    /// The access points from the last completed scan: deduplicated by SSID keeping the strongest
    /// radio of each, sorted strongest first, and cut to 20. Keeps the previous list while
    /// [`NetworkState::scanning`] is true, so a panel does not blank out mid-scan.
    pub available_networks: Vec<AccessPointInfo>,
}

/// A pending `network:connect(ssid, hidden)` intent, stashed in the controller (ADR-0037) --
/// single-slot semantics like `pending_challenge` (ADR-0028) -- until the paired
/// `secure_submit(network, connect)` arrives with the password bytes (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingNetworkConnect {
    pub ssid: String,
    pub hidden: bool,
}

/// What the Wi-Fi signal forwarder task reports back to `main.rs`'s top-level `select!` -- just
/// enough to know what kind of rebuild-and-push is needed, not the payload itself (that needs a
/// fresh D-Bus round trip through [`NetworkController::build_available_networks`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSignal {
    /// An access point was added or removed -- the AP list needs rebuilding.
    AccessPointsChanged,
    /// `LastScan` changed, meaning a scan this Supervisor triggered has finished.
    ScanCompleted,
    /// Sent by [`NetworkController::mark_scanning`], not the forwarder: `scanning` must flip to
    /// `true` immediately, before `RequestScan`'s D-Bus round trip completes (§4.2). Routed
    /// through the same channel as the real signals for FIFO ordering.
    ScanStarted,
}

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
}

/// `rusty_network_manager`'s own hand-written `<Proxy>::new_from_path` helpers declare a return
/// type whose lifetime is tied to the `&Connection` argument's borrow, even though the generated
/// `builder()` they call internally clones the connection into an owned value right away and
/// never holds onto that borrow. That signature makes it impossible to store the resulting proxy
/// past the borrow's scope, which this controller needs to do. These four wrappers call the
/// same macro-generated `builder()` directly, which has no such lifetime-elision bug, letting
/// the caller bind the result to `'static`.
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

async fn bind_settings_connection(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> zbus::Result<SettingsConnectionProxy<'static>> {
    SettingsConnectionProxy::builder(connection).path(path)?.build().await
}

/// One resolved Wi-Fi device: its own object path (needed as `AddAndActivateConnection2`'s
/// `device` argument) alongside the `WirelessProxy` bound to it.
#[derive(Clone)]
struct WifiDevice {
    device_path: OwnedObjectPath,
    wireless: WirelessProxy<'static>,
}

/// Holds every proxy `oblisk.network`'s write actions and AP survey need, resolved once at
/// construction. `Clone`: every field is a cheap `zbus` proxy/connection handle, so a clone can
/// be moved into a `tokio::spawn`ed task for one write action without the caller losing its own
/// handle (ADR-0029: write actions spawn rather than await inline).
#[derive(Clone)]
pub struct NetworkController {
    connection: zbus::Connection,
    nm: NetworkManagerProxy<'static>,
    settings: SettingsProxy<'static>,
    wifi: Option<WifiDevice>,
    ethernet_device_paths: Vec<OwnedObjectPath>,
    /// `oblisk.network`'s own push state (ADR-0037), mutated only by
    /// [`handle_signal`](Self::handle_signal). `Mutex` because the controller is `Clone`; never
    /// held across an await.
    state: Arc<Mutex<NetworkState>>,
    /// The single pending `network:connect` intent slot -- see [`PendingNetworkConnect`].
    pending_connect: Arc<Mutex<Option<PendingNetworkConnect>>>,
    /// Clone of the signal channel's sender: lets [`mark_scanning`](Self::mark_scanning) route
    /// its immediate flip through the same FIFO as the forwarder's real signals, and keeps the
    /// channel open even with no Wi-Fi device.
    events: UnboundedSender<NetworkSignal>,
}

impl NetworkController {
    /// Connects to NetworkManager over `connection` (the Supervisor's existing system-bus
    /// connection), resolves the Wi-Fi and Ethernet device sets, and spawns the Wi-Fi signal
    /// forwarder feeding `events` (ADR-0037). A device whose own `DeviceType` can't be read is
    /// logged and skipped, not fatal to startup.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<NetworkSignal>) -> zbus::Result<Self> {
        let nm = NetworkManagerProxy::new(&connection).await?;
        let settings = SettingsProxy::new(&connection).await?;

        let mut wifi = None;
        let mut ethernet_device_paths = Vec::new();
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
                Ok(NMDeviceType::ETHERNET) => ethernet_device_paths.push(path),
                Ok(NMDeviceType::WIFI) if wifi.is_none() => match bind_wireless(&connection, path.clone()).await {
                    Ok(wireless) => wifi = Some(WifiDevice { device_path: path, wireless }),
                    Err(err) => eprintln!("network: failed to bind wireless device {path}: {err}"),
                },
                _ => {}
            }
        }

        match &wifi {
            Some(wifi) => spawn_wifi_signal_forwarder(wifi.wireless.clone(), events.clone()),
            None => eprintln!("network: no Wi-Fi device found; scan/access-point events are disabled for this session"),
        }

        Ok(Self {
            connection,
            nm,
            settings,
            wifi,
            ethernet_device_paths,
            state: Arc::new(Mutex::new(NetworkState::default())),
            pending_connect: Arc::new(Mutex::new(None)),
            events,
        })
    }

    /// Applies one [`NetworkSignal`] to the controller-owned [`NetworkState`] and returns the
    /// updated state to push (docs/adr/0029: no debounce -- every relevant event fully
    /// re-derives the AP list from scratch).
    pub async fn handle_signal(&self, signal: NetworkSignal) -> NetworkState {
        match signal {
            NetworkSignal::ScanStarted => {
                let mut state = self.state.lock().unwrap();
                state.scanning = true;
                state.clone()
            }
            NetworkSignal::ScanCompleted | NetworkSignal::AccessPointsChanged => {
                let available_networks = self.build_available_networks().await;
                let mut state = self.state.lock().unwrap();
                if signal == NetworkSignal::ScanCompleted {
                    state.scanning = false;
                }
                state.available_networks = available_networks;
                state.clone()
            }
        }
    }

    /// The immediate half of `network:scan()`: queues [`NetworkSignal::ScanStarted`] so
    /// `scanning` flips to `true` on initiation, not once `RequestScan` completes. Only when a
    /// Wi-Fi device exists: with none, [`scan`](Self::scan) silently no-ops and no
    /// `ScanCompleted` will ever arrive to flip `scanning` back, leaving it stuck `true`.
    pub fn mark_scanning(&self) {
        if self.wifi.is_some() {
            let _ = self.events.send(NetworkSignal::ScanStarted);
        }
    }

    /// Stashes a `network:connect(ssid, hidden)` intent until its paired
    /// `secure_submit(network, connect)` arrives -- newest intent wins (single slot).
    pub fn stash_connect_intent(&self, pending: PendingNetworkConnect) {
        *self.pending_connect.lock().unwrap() = Some(pending);
    }

    /// Takes the pending connect intent, if any -- the `secure_submit(network, connect)` arm's
    /// one consumer.
    pub fn take_connect_intent(&self) -> Option<PendingNetworkConnect> {
        self.pending_connect.lock().unwrap().take()
    }

    /// § 4.1: `NetworkingEnabled` is a NetworkManager read-only property -- only
    /// `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled` have setters. The real, only way to toggle
    /// it is the `Enable(bool)` method, deviating from the spec text's literal "sets the
    /// `NetworkingEnabled` property".
    pub async fn set_networking_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.enable(enabled).await {
            eprintln!("network: failed to set networking_enabled={enabled}: {err}");
        }
    }

    /// § 4.1: `WirelessEnabled` is a real read-write property.
    pub async fn set_wifi_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.set_wireless_enabled(enabled).await {
            eprintln!("network: failed to set wifi_enabled={enabled}: {err}");
        }
    }

    /// § 4.1 / ADR-0029: `false` disconnects every wired device; `true` activates each device's
    /// existing autoconnect profile, if any, and is a no-op for a device with none -- there is
    /// no NM method that fabricates a carrier connection without a profile already present.
    pub async fn set_ethernet_enabled(&self, enabled: bool) {
        for path in &self.ethernet_device_paths {
            let device = match bind_device(&self.connection, path.clone()).await {
                Ok(device) => device,
                Err(err) => {
                    eprintln!("network: failed to bind ethernet device {path}: {err}");
                    continue;
                }
            };
            if enabled {
                self.activate_autoconnect_profile(&device, path).await;
            } else if let Err(err) = device.disconnect().await {
                eprintln!("network: failed to disconnect ethernet device {path}: {err}");
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
            // No profile exists for this device -- nothing D-Bus can do about that (ADR-0029).
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

    /// § 4.2: dispatches `RequestScan({})` off the calling path. A missing Wi-Fi device is
    /// logged, not a panic -- a system with no wireless hardware still runs everything else.
    pub async fn scan(&self) {
        let Some(wifi) = &self.wifi else {
            eprintln!("network: scan() requested but no Wi-Fi device is present");
            return;
        };
        if let Err(err) = wifi.wireless.request_scan(HashMap::new()).await {
            eprintln!("network: RequestScan failed: {err}");
        }
    }

    /// § 4.2: fully re-queries the Wi-Fi device's current AP list and returns it deduplicated
    /// and capped at the top 20 by strength (docs/adr/0029: no debounce). Empty, not an error,
    /// when there's no Wi-Fi device.
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

        let mut aps = Vec::with_capacity(ap_paths.len());
        for path in ap_paths {
            if let Some(ap) = self.read_access_point(&path, active_path.as_ref()).await {
                aps.push(ap);
            }
        }
        dedup_and_top20(aps)
    }

    async fn read_access_point(
        &self,
        path: &OwnedObjectPath,
        active_path: Option<&OwnedObjectPath>,
    ) -> Option<AccessPointInfo> {
        let ap = bind_access_point(&self.connection, path.clone()).await.ok()?;
        let ssid_bytes = ap.ssid().await.ok()?;
        if ssid_bytes.is_empty() {
            // ponytail: a hidden AP reports an empty SSID -- no name to show or dedupe by, and
            // including it would collapse every hidden AP in range into one bogus "" entry.
            // Connecting still works via network:connect(ssid, hidden=true).
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
            active: active_path == Some(path),
        })
    }

    /// § 4.3: `pending`'s SSID/hidden flag plus `secret` (empty means open, non-empty means
    /// WPA-PSK) become `AddAndActivateConnection2`'s connection dict. `secret` is zeroized
    /// immediately after use regardless of outcome (ADR-0005/ADR-0014): the caller has already
    /// `mem::take`n it out of the wire `SecureSubmit` frame, so this is that plaintext's one
    /// owner.
    pub async fn connect(&self, pending: PendingNetworkConnect, mut secret: Vec<u8>) {
        let result = self.connect_inner(&pending, &secret).await;
        secret.zeroize();
        if let Err(err) = result {
            eprintln!("network: connect(ssid={:?}) failed: {err}", pending.ssid);
        }
    }

    async fn connect_inner(&self, pending: &PendingNetworkConnect, secret: &[u8]) -> Result<(), ConnectError> {
        let wifi = self.wifi.as_ref().ok_or(ConnectError::NoWifiDevice)?;
        let mut intent = connection_intent(&pending.ssid, pending.hidden, secret)?;
        let dict = build_connection_dict(&intent);
        let result =
            self.nm.add_and_activate_connection2(dict, &wifi.device_path, &root_object_path(), HashMap::new()).await;
        // intent.psk is build_connection_dict's own plaintext-password copy, zeroized explicitly
        // here on every exit path rather than left to Drop alone (ADR-0005/ADR-0014). dict
        // borrows from intent and is fully consumed by the call above, so this is the first
        // point it's safe to mutate.
        if let Some(psk) = intent.psk.as_mut() {
            psk.zeroize();
        }
        result?;
        Ok(())
    }

    /// § 4.3: deletes every connection profile matching `ssid` (plural, per spec -- not just
    /// the first match).
    pub async fn forget(&self, ssid: &str) {
        let paths = match self.settings.list_connections().await {
            Ok(paths) => paths,
            Err(err) => {
                eprintln!("network: forget({ssid:?}) failed to list connections: {err}");
                return;
            }
        };
        for path in paths {
            let conn = match bind_settings_connection(&self.connection, path.clone()).await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("network: forget({ssid:?}) failed to bind connection {path}: {err}");
                    continue;
                }
            };
            let settings = match conn.get_settings().await {
                Ok(settings) => settings,
                Err(err) => {
                    eprintln!("network: forget({ssid:?}) failed to read settings for {path}: {err}");
                    continue;
                }
            };
            if settings_match_ssid(&settings, ssid)
                && let Err(err) = conn.delete().await
            {
                eprintln!("network: forget({ssid:?}) failed to delete profile {path}: {err}");
            }
        }
    }
}

/// Every action `oblisk.network:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAction {
    SetNetworkingEnabled,
    SetWifiEnabled,
    SetEthernetEnabled,
    Scan,
    Connect,
    Forget,
}

/// `oblisk.network`'s action dispatch (ADR-0037). Write actions are `tokio::spawn`ed rather than
/// awaited inline (ADR-0029); `connect` only stashes its intent, the actual connect runs when
/// the paired `secure_submit(network, connect)` arrives.
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
            Some((ssid, hidden)) => controller.stash_connect_intent(PendingNetworkConnect { ssid, hidden }),
            None => crate::log_malformed_command(params),
        },
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

/// Runs until `wireless`'s connection drops, forwarding `AccessPointAdded`/`AccessPointRemoved`/
/// `LastScan`-changed signals to `events` as [`NetworkSignal`]s. Spawned once, from
/// [`NetworkController::new`], with its own clone of the `WirelessProxy` -- keeps the
/// borrow-heavy `PropertyStream`/`SignalStream` types local to this task's own async block,
/// rather than threading borrowed streams through `main.rs`'s top-level `select!`.
///
/// A dropped `events` receiver (Supervisor shutting down) ends this task the next time it tries
/// to forward a signal, the same posture every channel-forwarding task in this codebase takes.
fn spawn_wifi_signal_forwarder(wireless: WirelessProxy<'static>, events: UnboundedSender<NetworkSignal>) {
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

        loop {
            tokio::select! {
                Some(_) = ap_added.next() => {
                    if events.send(NetworkSignal::AccessPointsChanged).is_err() { break; }
                }
                Some(_) = ap_removed.next() => {
                    if events.send(NetworkSignal::AccessPointsChanged).is_err() { break; }
                }
                Some(_) = last_scan_changed.next() => {
                    if events.send(NetworkSignal::ScanCompleted).is_err() { break; }
                }
                else => break,
            }
        }
    });
}
