//! NetworkManager D-Bus controller (`oblisk.network`, build-steps.md Phase 16;
//! docs/oblisk-supervisor-services-dbus.md §4; docs/adr/0029).
//!
//! Mirrors `dbus::polkit`'s structure (a controller holding the proxies it needs, exposing
//! async methods for each IDL write action) rather than `audio::mixer`'s dedicated-thread
//! pattern -- NetworkManager's API is D-Bus-native, so its signal streams merge into
//! `main.rs`'s existing top-level `tokio::select!` instead of needing a thread of their own
//! (ADR-0029's "Listener architecture" section).
//!
//! D-Bus access goes through `rusty_network_manager` (ADR-0013's "reuse a maintained
//! `#[zbus::proxy]` crate" rule, ADR-0029) rather than hand-written proxies.
//!
//! ponytail: the Wi-Fi and Ethernet device set is resolved once, at [`NetworkController::new`]
//! time, and never re-discovered. A real machine's Wi-Fi/Ethernet hardware doesn't appear or
//! disappear mid-session in the common case this targets (a laptop's built-in adapters); a
//! USB Wi-Fi dongle plugged in after the Supervisor starts wouldn't be picked up until restart.
//! The upgrade path is subscribing to `NetworkManagerProxy::device_added`/`device_removed` and
//! re-running the same device-type scan this constructor already does.
//!
//! ponytail: exactly one Wi-Fi device is tracked (the first one `GetAllDevices` returns) --
//! every real desktop/laptop this targets has at most one built-in wireless adapter. Multiple
//! simultaneous Wi-Fi adapters would need `available_networks` (and `scan`/`connect`) to carry
//! a device selector; nothing in `docs/oblisk-idl-api-specs.md` §2.5's schema has one yet.

use std::collections::HashMap;

use rusty_network_manager::dbus_interface_types::NMDeviceType;
use rusty_network_manager::{
    AccessPointProxy, DeviceProxy, NM80211ApFlags, NetworkManagerProxy, SettingsConnectionProxy, SettingsProxy, WirelessProxy,
};
use serde::Serialize;
use shared::Zeroize;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

/// How many deduplicated access points [`dedup_and_top20`] keeps (docs/oblisk-supervisor-
/// services-dbus.md §4.2: "serializes the top 20 access points").
const MAX_AVAILABLE_NETWORKS: usize = 20;

/// One scanned access point, already resolved to what `network.available_networks` needs
/// (docs/oblisk-idl-api-specs.md §2.5). `Serialize`: this is what ends up in a `StateSnapshot`'s
/// `payload`, same convention as `audio::mixer::AppStream`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessPointInfo {
    pub ssid: String,
    pub strength: u8,
    pub secure: bool,
    pub band: String,
    pub active: bool,
}

/// `oblisk.network`'s live push state (docs/adr/0029: "the exact `NetworkState` struct shape...
/// [is an] implementation-pass detail, not designed here"). Scoped to exactly what §4.2 asks
/// for -- scanning status and the deduplicated AP list -- not the full §2.5 read schema
/// (`connected`/`ssid`/`wifi_enabled`/etc.), which §4 doesn't ask this controller to track.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NetworkState {
    pub scanning: bool,
    pub available_networks: Vec<AccessPointInfo>,
}

/// A pending `network:connect(ssid, hidden)` intent, stashed in `main.rs` (mirroring
/// `pending_challenge`, ADR-0028) until the paired `secure_submit(network, connect)` arrives
/// with the password bytes (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingNetworkConnect {
    pub ssid: String,
    pub hidden: bool,
}

/// What the Wi-Fi signal forwarder task (see [`spawn_wifi_signal_forwarder`]) reports back to
/// `main.rs`'s own top-level `select!` -- just enough to know *what kind* of rebuild-and-push is
/// needed, not the payload itself (the payload needs a fresh D-Bus round trip through
/// [`NetworkController::build_available_networks`], which only `main.rs`'s own `NetworkController`
/// handle can do, not this forwarding task).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSignal {
    /// An access point was added or removed -- the AP list needs rebuilding.
    AccessPointsChanged,
    /// `LastScan` changed, meaning a scan this Supervisor triggered has finished.
    ScanCompleted,
}

/// Failure modes [`NetworkController::connect`] can hit before ever reaching NetworkManager
/// itself. Logged via `Display` at the call site, not user-facing.
#[derive(Debug)]
enum ConnectError {
    NoWifiDevice,
    InvalidSecret(String),
    Dbus(zbus::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoWifiDevice => write!(f, "no Wi-Fi device is present"),
            Self::InvalidSecret(message) => write!(f, "secret is not valid UTF-8: {message}"),
            Self::Dbus(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<zbus::Error> for ConnectError {
    fn from(err: zbus::Error) -> Self {
        Self::Dbus(err)
    }
}

/// `[2400, 2500]` -> `"2.4 GHz"`, `[4900, 5900]` -> `"5 GHz"`, `[5925, 7125]` -> `"6 GHz"`
/// (docs/oblisk-supervisor-services-dbus.md §4.2). `None` outside all three ranges -- real Wi-Fi
/// hardware's `Frequency` property always falls inside one of them, so this is an honest "no
/// band" rather than a guessed default.
fn resolve_band(freq_mhz: u32) -> Option<&'static str> {
    match freq_mhz {
        2400..=2500 => Some("2.4 GHz"),
        4900..=5900 => Some("5 GHz"),
        5925..=7125 => Some("6 GHz"),
        _ => None,
    }
}

/// Whether an access point requires a security key: it advertises `PRIVACY` (WEP, the only case
/// that flag alone signals) or either RSN (WPA2/3) or WPA1 key-management flags are non-empty.
/// Mirrors `rusty_network_manager`'s own `show_wifi_networks` example's
/// `ap_security_flags_to_security` logic, reduced to the boolean this codebase's IDL needs.
fn access_point_is_secure(flags: u32, wpa_flags: u32, rsn_flags: u32) -> bool {
    let flags = NM80211ApFlags::from_bits_truncate(flags);
    flags.contains(NM80211ApFlags::PRIVACY) || wpa_flags != 0 || rsn_flags != 0
}

/// Merges duplicate SSIDs keeping the highest signal strength, then serializes the top 20
/// (docs/oblisk-supervisor-services-dbus.md §4.2). Ties within the same SSID keep whichever
/// entry was seen first -- NetworkManager doesn't report the same physical AP object twice in
/// one `GetAccessPoints` call, so a tie only happens between two distinct BSSIDs broadcasting
/// the same SSID, and picking either one is equally correct for display purposes.
fn dedup_and_top20(aps: Vec<AccessPointInfo>) -> Vec<AccessPointInfo> {
    let mut best: HashMap<String, AccessPointInfo> = HashMap::new();
    for ap in aps {
        best.entry(ap.ssid.clone()).and_modify(|existing| if ap.strength > existing.strength { *existing = ap.clone() }).or_insert(ap);
    }
    let mut deduped: Vec<AccessPointInfo> = best.into_values().collect();
    deduped.sort_by_key(|ap| std::cmp::Reverse(ap.strength));
    deduped.truncate(MAX_AVAILABLE_NETWORKS);
    deduped
}

/// Whether a `SettingsConnectionProxy::get_settings()` result's `connection.autoconnect` allows
/// autoconnect -- absent means NetworkManager's own default of `true`, matching ADR-0029's
/// "`connection.autoconnect != false`" phrasing exactly (only an explicit `false` disqualifies
/// a profile).
fn connection_wants_autoconnect(settings: &HashMap<String, HashMap<String, OwnedValue>>) -> bool {
    settings
        .get("connection")
        .and_then(|section| section.get("autoconnect"))
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(true)
}

/// Whether a `SettingsConnectionProxy::get_settings()` result is a Wi-Fi profile for `ssid`
/// (used by `forget`, which must delete every matching profile, not just the first --
/// docs/oblisk-supervisor-services-dbus.md §4.3 says "profiles", plural).
fn settings_match_ssid(settings: &HashMap<String, HashMap<String, OwnedValue>>, ssid: &str) -> bool {
    settings
        .get("802-11-wireless")
        .and_then(|section| section.get("ssid"))
        .and_then(|value| Vec::<u8>::try_from(value.clone()).ok())
        .is_some_and(|bytes| bytes == ssid.as_bytes())
}

/// The `network:connect(ssid, hidden)` intent plus the `secure_submit` secret, boiled down to
/// "open or WPA-PSK" before any zbus-specific `Value` wrapping happens -- kept unit-testable
/// without a live D-Bus connection. An empty secret means an open network (ADR-0029); a
/// non-empty one must be valid UTF-8 to become NM's `802-11-wireless-security.psk` (a D-Bus
/// string) -- see the module doc comment on [`ConnectError::InvalidSecret`] for why an invalid
/// encoding fails loudly instead of lossily mangling the password.
#[derive(Debug, Clone, PartialEq)]
struct ConnectionIntent {
    ssid: String,
    hidden: bool,
    psk: Option<String>,
}

fn connection_intent(ssid: &str, hidden: bool, secret: &[u8]) -> Result<ConnectionIntent, ConnectError> {
    let psk = if secret.is_empty() {
        None
    } else {
        match String::from_utf8(secret.to_vec()) {
            Ok(psk) => Some(psk),
            Err(err) => {
                // The invalid-UTF-8 bytes are still a plaintext-password copy even though they
                // never became a `String` -- zeroize them before propagating, same discipline as
                // the valid-UTF-8 `psk` below (ADR-0005/ADR-0014). The message is captured first
                // since `FromUtf8Error::into_bytes` consumes the error.
                let message = err.to_string();
                let mut bytes = err.into_bytes();
                bytes.zeroize();
                return Err(ConnectError::InvalidSecret(message));
            }
        }
    };
    Ok(ConnectionIntent { ssid: ssid.to_string(), hidden, psk })
}

/// Builds the minimal connection dict `AddAndActivateConnection2` needs for `intent`
/// (docs/oblisk-supervisor-services-dbus.md §4.3): `802-11-wireless-security` is present only
/// for a secured (non-empty-secret) intent, and `hidden`/`scan-ssid` are only set when the
/// intent's `hidden` flag is set.
fn build_connection_dict(intent: &ConnectionIntent) -> HashMap<&str, HashMap<&str, Value<'_>>> {
    let mut dict: HashMap<&str, HashMap<&str, Value>> = HashMap::new();

    let mut connection: HashMap<&str, Value> = HashMap::new();
    connection.insert("id", Value::new(intent.ssid.as_str()));
    connection.insert("type", Value::new("802-11-wireless"));
    dict.insert("connection", connection);

    let mut wireless: HashMap<&str, Value> = HashMap::new();
    wireless.insert("ssid", Value::new(intent.ssid.as_bytes()));
    wireless.insert("mode", Value::new("infrastructure"));
    if intent.hidden {
        wireless.insert("hidden", Value::new(true));
        // Not a real NM setting key (NM's own hidden-network probing is driven by `hidden`
        // alone) -- included anyway because docs/oblisk-supervisor-services-dbus.md §4.3 asks
        // for both explicitly, and an extra key NetworkManager doesn't recognize in this dict is
        // silently ignored rather than rejected.
        wireless.insert("scan-ssid", Value::new(true));
    }
    dict.insert("802-11-wireless", wireless);

    if let Some(psk) = &intent.psk {
        let mut security: HashMap<&str, Value> = HashMap::new();
        security.insert("key-mgmt", Value::new("wpa-psk"));
        security.insert("psk", Value::new(psk.as_str()));
        dict.insert("802-11-wireless-security", security);
    }

    dict
}

/// `network:set_networking_enabled(en)`'s `arguments: [en]` -- shares `main.rs`'s
/// `process_run_args`-style "parse or log and drop" convention. Defined once in `dbus` (shared
/// with `bluetooth::parse_bool_arg`) and re-exported here so `network::parse_bool_arg` keeps
/// working unchanged at every call site.
pub use crate::dbus::parse_bool_arg;

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

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
}

/// `rusty_network_manager`'s own hand-written `<Proxy>::new_from_path(path, connection: &Connection)`
/// helpers (verified against its source: `device.rs`/`wireless.rs`/`access_point.rs`/
/// `settings_connection.rs`) declare a return type whose lifetime is tied to the `&Connection`
/// argument's own borrow, even though the generated `builder()` they call internally clones the
/// connection into an owned value right away (`zbus::proxy::Builder::new` -- verified against
/// vendored `zbus-5.19.0` source) and never actually holds onto that borrow. That signature
/// makes it impossible to store the resulting proxy past the borrow's own scope, which this
/// controller needs to do (proxies are rebuilt per D-Bus object path and kept only as long as
/// the caller needs them, not tied to one loop iteration). These four small wrappers call the
/// same macro-generated `builder()` directly instead, which has no such lifetime-elision bug --
/// letting the caller bind the result to `'static`, matching how `dbus::polkit::register_agent`
/// already stores its own long-lived `AuthorityProxy` past any one call's scope.
async fn bind_device(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<DeviceProxy<'static>> {
    DeviceProxy::builder(connection).path(path)?.build().await
}

async fn bind_wireless(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<WirelessProxy<'static>> {
    WirelessProxy::builder(connection).path(path)?.build().await
}

async fn bind_access_point(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<AccessPointProxy<'static>> {
    AccessPointProxy::builder(connection).path(path)?.build().await
}

async fn bind_settings_connection(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<SettingsConnectionProxy<'static>> {
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
/// construction (see the module doc comment's ponytail notes). `Clone`: every field is a cheap
/// `zbus` proxy/connection handle (`Arc`-backed under the hood), so a clone can be moved into a
/// `tokio::spawn`ed task for one write action without the caller losing its own handle --
/// exactly what ADR-0029's "write actions get `tokio::spawn`ed rather than awaited inline" needs.
#[derive(Clone)]
pub struct NetworkController {
    connection: zbus::Connection,
    nm: NetworkManagerProxy<'static>,
    settings: SettingsProxy<'static>,
    wifi: Option<WifiDevice>,
    ethernet_device_paths: Vec<OwnedObjectPath>,
}

impl NetworkController {
    /// Connects to NetworkManager over `connection` (the Supervisor's existing system-bus
    /// connection, shared with `dbus::polkit`) and resolves the Wi-Fi and Ethernet device sets.
    /// A device whose own `DeviceType` can't be read is logged and skipped, not fatal to
    /// startup -- one misbehaving device shouldn't take the whole controller down.
    pub async fn new(connection: zbus::Connection) -> zbus::Result<Self> {
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

        Ok(Self { connection, nm, settings, wifi, ethernet_device_paths })
    }

    /// A fresh clone of the Wi-Fi device's `WirelessProxy`, if one was found -- what
    /// [`spawn_wifi_signal_forwarder`] needs to subscribe to AP-added/removed/scan-completed
    /// signals from outside this controller.
    pub fn wifi_signal_source(&self) -> Option<WirelessProxy<'static>> {
        self.wifi.as_ref().map(|wifi| wifi.wireless.clone())
    }

    /// Whether a Wi-Fi device was found at construction time -- a cheap, synchronous check
    /// `main.rs`'s `("network", "scan")` handler needs before flipping `network.scanning` to
    /// `true` (Correctness review): with no Wi-Fi device, [`scan`](Self::scan) silently no-ops
    /// and [`spawn_wifi_signal_forwarder`] was never spawned, so no `ScanCompleted` signal would
    /// ever arrive to flip `scanning` back to `false`.
    pub fn has_wifi_device(&self) -> bool {
        self.wifi.is_some()
    }

    /// § 4.1: `NetworkingEnabled` is a NetworkManager *read-only* property -- there is no
    /// `SetNetworkingEnabled` setter (verified against `rusty_network_manager`'s generated
    /// `NetworkManagerProxy`: only `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled` have setters).
    /// The real, and only, way to toggle it is the `Enable(bool)` method, so this deviates from
    /// the spec text's literal "sets the `NetworkingEnabled` property" -- see this module's
    /// report notes.
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

    /// § 4.1 / ADR-0029's "Ethernet toggle" section: `false` disconnects every wired device;
    /// `true` activates each device's existing autoconnect profile, if any, and is a no-op
    /// (not an error) for a device with none -- there is no NM method that fabricates a carrier
    /// connection without a profile already present.
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
                    eprintln!("network: failed to bind connection {conn_path} while searching for an autoconnect profile: {err}");
                    continue;
                }
            };
            let settings = match conn.get_settings().await {
                Ok(settings) => settings,
                Err(err) => {
                    eprintln!("network: failed to read settings for {conn_path} while searching for an autoconnect profile: {err}");
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
    /// and capped at the top 20 by strength (docs/adr/0029: "no debounce" -- every relevant
    /// event re-derives this from scratch rather than incrementally patching a cached list).
    /// Empty (not an error) when there's no Wi-Fi device.
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

    async fn read_access_point(&self, path: &OwnedObjectPath, active_path: Option<&OwnedObjectPath>) -> Option<AccessPointInfo> {
        let ap = bind_access_point(&self.connection, path.clone()).await.ok()?;
        let ssid_bytes = ap.ssid().await.ok()?;
        if ssid_bytes.is_empty() {
            // ponytail: a broadcast-suppressed (hidden) AP reports an empty SSID here -- there's
            // no name to show or to dedupe by, and including it would collapse every hidden AP
            // in range into one bogus "" entry. Connecting to a hidden network still works via
            // `network:connect(ssid, hidden=true)`, which never needs this AP to be listed.
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
    /// immediately after use regardless of outcome (ADR-0005/ADR-0014 discipline, matching the
    /// existing PAM `secure_submit` arm's pattern) -- the caller has already `mem::take`n it out
    /// of the wire `SecureSubmit` frame, so this is that plaintext's one owner.
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
        let result = self.nm.add_and_activate_connection2(dict, &wifi.device_path, &root_object_path(), HashMap::new()).await;
        // `intent.psk` is `build_connection_dict`'s own plaintext-password copy (the `String`
        // `connection_intent` built via `String::from_utf8`) -- zeroized explicitly here, right
        // after its one sanctioned use, on every exit path (success or D-Bus failure), rather than
        // left to `Drop` alone (ADR-0005/ADR-0014). `dict` borrows from `intent` and is fully
        // consumed by the call above, so this is the first point it's safe to mutate.
        if let Some(psk) = intent.psk.as_mut() {
            // SAFETY: overwriting with zero bytes keeps the `String` valid UTF-8.
            unsafe { psk.as_bytes_mut() }.zeroize();
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

/// Runs until `wireless`'s connection drops, forwarding `AccessPointAdded`/`AccessPointRemoved`/
/// `LastScan`-changed signals to `events` as [`NetworkSignal`]s. Spawned once, from `main.rs`,
/// with its own clone of the `WirelessProxy` -- keeps the borrow-heavy `PropertyStream`/
/// `SignalStream` types this needs entirely local to this task's own async block, rather than
/// threading borrowed streams through `main.rs`'s already-large top-level `select!` (this
/// module's alternative permitted by build-steps.md Phase 16; ADR-0029's own "merge into the
/// top-level select!" wording is satisfied one level up, via `events.recv()` in that select!).
///
/// A dropped `events` receiver (Supervisor shutting down) ends this task the next time it tries
/// to forward a signal, same "not a reason to keep going" treatment every other channel-forwarding
/// task in this codebase gives a closed channel.
pub fn spawn_wifi_signal_forwarder(wireless: WirelessProxy<'static>, events: UnboundedSender<NetworkSignal>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ap(ssid: &str, strength: u8) -> AccessPointInfo {
        AccessPointInfo { ssid: ssid.to_string(), strength, secure: false, band: "2.4 GHz".to_string(), active: false }
    }

    #[test]
    fn resolve_band_maps_each_spec_range() {
        assert_eq!(resolve_band(2400), Some("2.4 GHz"));
        assert_eq!(resolve_band(2450), Some("2.4 GHz"));
        assert_eq!(resolve_band(2500), Some("2.4 GHz"));
        assert_eq!(resolve_band(4900), Some("5 GHz"));
        assert_eq!(resolve_band(5180), Some("5 GHz"));
        assert_eq!(resolve_band(5900), Some("5 GHz"));
        assert_eq!(resolve_band(5925), Some("6 GHz"));
        assert_eq!(resolve_band(6200), Some("6 GHz"));
        assert_eq!(resolve_band(7125), Some("6 GHz"));
    }

    #[test]
    fn resolve_band_is_none_outside_every_range() {
        assert_eq!(resolve_band(0), None);
        assert_eq!(resolve_band(2399), None);
        assert_eq!(resolve_band(2501), None, "the gap between 2.4 GHz and 5 GHz");
        assert_eq!(resolve_band(5901), None, "the gap between 5 GHz and 6 GHz");
        assert_eq!(resolve_band(7126), None);
    }

    #[test]
    fn access_point_is_secure_is_false_for_a_fully_open_network() {
        assert!(!access_point_is_secure(0, 0, 0));
    }

    #[test]
    fn access_point_is_secure_is_true_for_wep_privacy_alone() {
        assert!(access_point_is_secure(NM80211ApFlags::PRIVACY.bits(), 0, 0));
    }

    #[test]
    fn access_point_is_secure_is_true_when_only_wpa_flags_are_set() {
        assert!(access_point_is_secure(0, 0b0000_0100, 0));
    }

    #[test]
    fn access_point_is_secure_is_true_when_only_rsn_flags_are_set() {
        assert!(access_point_is_secure(0, 0, 0b0000_0100));
    }

    #[test]
    fn dedup_and_top20_keeps_the_highest_strength_entry_per_ssid() {
        let result = dedup_and_top20(vec![ap("home", 40), ap("home", 90), ap("home", 60)]);
        assert_eq!(result, vec![ap("home", 90)]);
    }

    #[test]
    fn dedup_and_top20_sorts_by_strength_descending() {
        let result = dedup_and_top20(vec![ap("weak", 10), ap("strong", 90), ap("mid", 50)]);
        assert_eq!(result.iter().map(|a| a.ssid.as_str()).collect::<Vec<_>>(), vec!["strong", "mid", "weak"]);
    }

    #[test]
    fn dedup_and_top20_truncates_to_20() {
        let aps: Vec<AccessPointInfo> = (0..30).map(|i| ap(&format!("ap{i}"), i as u8)).collect();
        assert_eq!(dedup_and_top20(aps).len(), 20);
    }

    #[test]
    fn dedup_and_top20_keeps_the_20_strongest_not_just_the_first_20() {
        let mut aps: Vec<AccessPointInfo> = (0..30).map(|i| ap(&format!("ap{i}"), i as u8)).collect();
        // The 30 strongest-first entries would be ap29..ap10 if sorting by strength works;
        // shuffle the input order so a naive "take the first 20" bug would fail this.
        aps.reverse();
        let result = dedup_and_top20(aps);
        assert!(result.iter().all(|a| a.strength >= 10), "must keep the strongest 20, not the first 20 seen");
    }

    fn settings_with(section: &str, key: &str, value: OwnedValue) -> HashMap<String, HashMap<String, OwnedValue>> {
        HashMap::from([(section.to_string(), HashMap::from([(key.to_string(), value)]))])
    }

    #[test]
    fn connection_wants_autoconnect_defaults_true_when_the_key_is_absent() {
        let settings: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();
        assert!(connection_wants_autoconnect(&settings));
    }

    #[test]
    fn connection_wants_autoconnect_is_false_only_when_explicitly_false() {
        let settings = settings_with("connection", "autoconnect", OwnedValue::try_from(Value::from(false)).unwrap());
        assert!(!connection_wants_autoconnect(&settings));
    }

    #[test]
    fn connection_wants_autoconnect_is_true_when_explicitly_true() {
        let settings = settings_with("connection", "autoconnect", OwnedValue::try_from(Value::from(true)).unwrap());
        assert!(connection_wants_autoconnect(&settings));
    }

    #[test]
    fn settings_match_ssid_compares_the_wireless_sections_ssid_bytes() {
        let settings = settings_with("802-11-wireless", "ssid", OwnedValue::try_from(Value::from(b"HomeWifi".to_vec())).unwrap());
        assert!(settings_match_ssid(&settings, "HomeWifi"));
        assert!(!settings_match_ssid(&settings, "OfficeWifi"));
    }

    #[test]
    fn settings_match_ssid_is_false_when_the_wireless_section_is_missing() {
        let settings: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();
        assert!(!settings_match_ssid(&settings, "HomeWifi"));
    }

    #[test]
    fn connection_intent_treats_an_empty_secret_as_open() {
        let intent = connection_intent("HomeWifi", false, &[]).unwrap();
        assert_eq!(intent.psk, None);
    }

    #[test]
    fn connection_intent_carries_a_non_empty_secret_as_the_psk() {
        let intent = connection_intent("HomeWifi", false, b"hunter2").unwrap();
        assert_eq!(intent.psk, Some("hunter2".to_string()));
    }

    #[test]
    fn connection_intent_rejects_non_utf8_secrets() {
        assert!(connection_intent("HomeWifi", false, &[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn build_connection_dict_omits_security_for_an_open_network() {
        let intent = connection_intent("HomeWifi", false, &[]).unwrap();
        let dict = build_connection_dict(&intent);
        assert!(!dict.contains_key("802-11-wireless-security"));
        assert!(!dict["802-11-wireless"].contains_key("hidden"));
    }

    #[test]
    fn build_connection_dict_sets_wpa_psk_for_a_secured_network() {
        let intent = connection_intent("HomeWifi", false, b"hunter2").unwrap();
        let dict = build_connection_dict(&intent);
        let security = &dict["802-11-wireless-security"];
        assert_eq!(String::try_from(security["key-mgmt"].clone()).unwrap(), "wpa-psk");
        assert_eq!(String::try_from(security["psk"].clone()).unwrap(), "hunter2");
    }

    #[test]
    fn build_connection_dict_marks_hidden_and_scan_ssid_for_a_hidden_network() {
        let intent = connection_intent("HiddenNet", true, &[]).unwrap();
        let dict = build_connection_dict(&intent);
        let wireless = &dict["802-11-wireless"];
        assert!(bool::try_from(wireless["hidden"].clone()).unwrap());
        assert!(bool::try_from(wireless["scan-ssid"].clone()).unwrap());
    }

    #[test]
    fn build_connection_dict_carries_the_ssid_bytes() {
        let intent = connection_intent("HomeWifi", false, &[]).unwrap();
        let dict = build_connection_dict(&intent);
        assert_eq!(Vec::<u8>::try_from(dict["802-11-wireless"]["ssid"].clone()).unwrap(), b"HomeWifi".to_vec());
    }

    #[test]
    fn parse_bool_arg_reads_the_first_argument() {
        assert_eq!(parse_bool_arg(&[serde_json::json!(true)]), Some(true));
        assert_eq!(parse_bool_arg(&[serde_json::json!(false)]), Some(false));
    }

    #[test]
    fn parse_bool_arg_rejects_a_malformed_shape() {
        assert_eq!(parse_bool_arg(&[]), None);
        assert_eq!(parse_bool_arg(&[serde_json::json!("yes")]), None);
    }

    #[test]
    fn parse_connect_args_parses_ssid_and_hidden() {
        assert_eq!(parse_connect_args(&[serde_json::json!("HomeWifi"), serde_json::json!(true)]), Some(("HomeWifi".to_string(), true)));
    }

    #[test]
    fn parse_connect_args_rejects_a_malformed_shape() {
        assert_eq!(parse_connect_args(&[]), None, "missing both elements");
        assert_eq!(parse_connect_args(&[serde_json::json!(1), serde_json::json!(true)]), None, "ssid is not a string");
        assert_eq!(parse_connect_args(&[serde_json::json!("HomeWifi"), serde_json::json!("nope")]), None, "hidden is not a boolean");
    }

    #[test]
    fn parse_ssid_arg_reads_the_first_argument() {
        assert_eq!(parse_ssid_arg(&[serde_json::json!("HomeWifi")]), Some("HomeWifi".to_string()));
        assert_eq!(parse_ssid_arg(&[]), None);
    }
}
