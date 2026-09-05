//! NetworkManager D-Bus controller (`oblisk.network`; docs/oblisk-supervisor-services-dbus.md §4;
//! ADR-0029). Mirrors `dbus::polkit`'s controller-holding-proxies structure rather than
//! `audio::mixer`'s dedicated-thread pattern: NetworkManager's API is D-Bus-native, so its signal
//! streams merge into `main.rs`'s top-level `tokio::select!` instead of needing a thread of their
//! own (ADR-0029). D-Bus access goes through `rusty_network_manager` (ADR-0013, ADR-0029) rather
//! than hand-written proxies.
//!
//! Four forwarder tasks feed one channel, one per interface that owns part of §2.5: the wireless
//! interface's AP set and association, each device's state, and the manager's radio switches and
//! default route. ADR-0082 says why the association half is not optional -- watching only the scan
//! left a connected machine reading offline for minutes at a time.
//!
//! ponytail: the Wi-Fi/Ethernet device set is resolved once, at [`NetworkController::new`] time,
//! never re-discovered, so a USB dongle plugged in after start needs a restart to be picked up.
//! Upgrade path: subscribe to `NetworkManagerProxy::device_added`/`device_removed` and rescan.
//!
//! ponytail: exactly one Wi-Fi device is tracked (the first `GetAllDevices` returns); multiple
//! adapters would need `available_networks`/`scan`/`connect` to carry a device selector, which
//! docs/oblisk-idl-api-specs.md §2.5's schema doesn't have yet.

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

/// One scanned access point, already resolved to what `network.available_networks` needs
/// (docs/oblisk-idl-api-specs.md §2.5). `Serialize`: this is what ends up in a `StateSnapshot`'s
/// `payload`, same convention as `audio::mixer::AppStream`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AccessPointInfo {
    /// The network name; entries dedupe on this, keeping only the stronger of two radios.
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

/// `oblisk.network`'s live push state: the whole §2.5 read schema, not just §4.2's scan results.
/// Every field is re-derived from NetworkManager on each [`NetworkSignal`] (ADR-0029: no debounce,
/// no incremental state).
///
/// The link fields exist because the AP list cannot answer "am I online". It says nothing about a
/// wired link, and it cannot tell a powered-down radio from a powered one with nothing joined --
/// both are simply an absence of [`AccessPointInfo::active`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetworkState {
    /// A scan is in flight. Flipped to `true` the moment `network:scan()` is accepted rather
    /// than when NetworkManager confirms, so a spinner starts on the click, not a round trip
    /// later.
    pub scanning: bool,
    /// Something is carrying the default route, from NetworkManager's `PrimaryConnection`. That
    /// property names the active connection the default route belongs to, which is §2.5's "default
    /// gateway interface is active" exactly; `/` means none, and means offline.
    pub connected: bool,
    /// The Wi-Fi SSID in use, or `"Ethernet"` when the default route is wired, or `nil` when
    /// nothing is joined. Wired wins when both are up, matching which one `connected` is about.
    /// It names an association, not a working route: a network still negotiating DHCP has an
    /// `ssid` and a `connected` of `false`, which is what makes those two fields worth having
    /// separately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// The associated AP's signal strength, `0` to `100`, or `0` with no Wi-Fi association. Read
    /// off the same merged entry the panel draws, so the bar and the list never disagree by a
    /// point.
    pub strength: u8,
    /// The Wi-Fi radio is powered, from `WirelessEnabled`. What separates "radio off" from
    /// "radio on, joined to nothing", which the AP list alone cannot.
    pub wifi_enabled: bool,
    /// NetworkManager is managing networking at all, from `NetworkingEnabled`. `false` means
    /// every other field here is a report about a stack that has been switched off.
    pub networking_enabled: bool,
    /// A wired device is activated. §2.5 words this as the link carrier, but the carrier is up
    /// whenever a cable is seated, which would leave `network:set_ethernet_enabled(false)` looking
    /// like it did nothing; this is the read-back that the setter's own toggle needs.
    pub ethernet_enabled: bool,
    /// The SSID a `network:connect` is currently trying to join, or `nil` when none is in flight.
    /// What a spinner on one row reads, the same job `LockState::authenticating` does for the lock
    /// -- and it names the row rather than being a bare flag, because a list needs to know which
    /// one. Cleared when the attempt reaches a verdict, either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connecting_ssid: Option<String>,
    /// Why the last `network:connect` failed, in words fit to draw, or `nil` when the last one
    /// worked or none has been tried. `AddAndActivateConnection2` returns before the radio has
    /// tried anything, so this is filled in later, from the activation's own
    /// `StateChanged(state, reason)`: a wrong password is only knowable there.
    ///
    /// Sticky until the next attempt, like `UpdatesState::check_error`: an error that cleared
    /// itself on the next scan would be gone before it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
    /// The SSID whose `network:connect` is waiting on a password, or `nil` when nothing is. Set by
    /// [`resolve_connect_intent`](NetworkController::resolve_connect_intent) for the one case that
    /// cannot proceed without one, and cleared by the attempt that consumes it or by
    /// `network:cancel_connect`.
    ///
    /// Here rather than derived in the config, because the fact it reports -- this machine has no
    /// profile for that SSID -- lives in NetworkManager's settings, and a config could only guess
    /// at it (ADR-0037). It is also what the shell binds `keyboard_interactivity` to: a bar that
    /// takes the keyboard whenever it feels like it is a bar that steals it, so the surface claims
    /// focus exactly while this names a network and gives it back the moment it stops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_ssid: Option<String>,
    /// Access points from the last completed scan: deduplicated by SSID, the connected one first
    /// and the rest strongest first, cut to 20, and kept as-is while [`NetworkState::scanning`] is
    /// true so a panel doesn't blank. The connected network leads by construction, so a list can
    /// be drawn in payload order without sorting it again.
    pub available_networks: Vec<AccessPointInfo>,
}

/// A pending `network:connect(ssid, hidden)` intent, stashed in the controller (ADR-0037) with
/// single-slot semantics like `pending_challenge` (ADR-0028), until the paired
/// `secure_submit(network, connect)` arrives with the password bytes (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingNetworkConnect {
    pub ssid: String,
    pub hidden: bool,
}

/// What the forwarder tasks report back to `main.rs`'s top-level `select!`: just enough to know
/// what kind of rebuild-and-push is needed, not the payload itself (that needs a fresh D-Bus round
/// trip through [`NetworkController::build_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSignal {
    /// Anything that can move a [`NetworkState`] field other than `scanning`: an access point
    /// appearing or going, the association changing, a device changing state, a radio toggling.
    /// One variant rather than one per source because every one of them ends in the same full
    /// re-derive (ADR-0029), so telling them apart would buy nothing.
    Changed,
    /// `LastScan` changed, meaning a scan this Supervisor triggered has finished.
    ScanCompleted,
    /// Sent by [`NetworkController::mark_scanning`], not a forwarder, so `scanning` flips `true`
    /// before `RequestScan`'s round trip completes (§4.2). Routed through the same channel as the
    /// real signals for FIFO ordering.
    ScanStarted,
}

/// How long [`NetworkController::watch_activation`] waits for a verdict before calling it a
/// timeout. A backstop, not the mechanism: NetworkManager gives up on a Wi-Fi association well
/// inside this and says so on `StateChanged`, so this only fires when the activation object stops
/// answering altogether -- long enough not to pre-empt NM, short enough that a stuck spinner
/// clears itself rather than waiting for the next attempt.
const ACTIVATION_CEILING: std::time::Duration = std::time::Duration::from_secs(45);

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
}

/// `rusty_network_manager`'s hand-written `<Proxy>::new_from_path` helpers tie their return type's
/// lifetime to the `&Connection` borrow, though the generated `builder()` they call clones the
/// connection immediately and never holds it, making it impossible to store the proxy past the
/// borrow's scope, which this controller needs. These five wrappers call `builder()` directly,
/// binding the result to `'static`.
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

/// `org.freedesktop.NetworkManager.Connection.Active`, hand-written against ADR-0013's "go through
/// `rusty_network_manager`" rule because that crate's binding for this one interface cannot work.
/// 0.7.1 declares the signal `#[zbus(signal, name = "state_changed")]`, and zbus takes an explicit
/// `name` verbatim rather than PascalCasing it, so `ActiveProxy::receive_active_state_changed`
/// subscribes to a member NetworkManager never emits and the stream stays silent forever. Measured,
/// not guessed: an activation that reached `ACTIVATED` in about a second produced no signal in 20.
/// The sibling `Device` proxy spells the same attribute `name = "StateChanged"` and works, which is
/// what makes this a typo upstream rather than a convention to follow.
///
/// Only the two members this controller needs, and `state` earns its place beyond convenience: a
/// property named `state` alongside a signal named `state_changed` would collide on the generated
/// `receive_state_changed`, which is how the upstream typo is an easy one to make.
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
        // ponytail: a hidden AP reports an empty SSID, no name to show or dedupe by, so including
        // it would collapse every hidden AP into one bogus "" entry. Connecting still works via
        // network:connect(ssid, hidden=true).
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

/// One resolved Wi-Fi device: its own object path (needed as `AddAndActivateConnection2`'s
/// `device` argument) alongside both proxies bound to it. `device` is separate from `wireless`
/// because association state lives on `org.freedesktop.NetworkManager.Device` and the AP list on
/// `...Device.Wireless`, two interfaces on the one object.
#[derive(Clone)]
struct WifiDevice {
    device_path: OwnedObjectPath,
    device: DeviceProxy<'static>,
    wireless: WirelessProxy<'static>,
}

/// One resolved Ethernet device. The path is what `ActivateConnection` wants; the proxy answers
/// `ethernet_enabled` and feeds a state forwarder.
#[derive(Clone)]
struct EthernetDevice {
    path: OwnedObjectPath,
    device: DeviceProxy<'static>,
}

/// One saved NetworkManager profile matched by SSID: its object path (what `ActivateConnection`
/// takes), the proxy to act on it, and the settings dict it was matched on, kept rather than
/// re-read because both callers already have a use for it.
struct SavedProfile {
    path: OwnedObjectPath,
    connection: SettingsConnectionProxy<'static>,
    settings: HashMap<String, HashMap<String, OwnedValue>>,
}

/// Holds every proxy `oblisk.network`'s write actions and AP survey need, resolved once at
/// construction. `Clone` since every field is a cheap `zbus` handle, letting a clone move into a
/// `tokio::spawn`ed write action without the caller losing its own (ADR-0029: writes spawn rather
/// than await inline).
#[derive(Clone)]
pub struct NetworkController {
    connection: zbus::Connection,
    nm: NetworkManagerProxy<'static>,
    settings: SettingsProxy<'static>,
    wifi: Option<WifiDevice>,
    ethernet: Vec<EthernetDevice>,
    /// Access-point proxies kept alive between rebuilds, keyed by object path. Not a cache of
    /// values -- a cache of *proxies*, so zbus fills each one's property cache once and keeps it
    /// filled from `PropertiesChanged` instead of the rebuild paying for a match rule, a `GetAll`
    /// and an unsubscribe per access point per pass. Measured at 10 access points: 11.25ms a
    /// rebuild before, 0.84ms after (ADR-0082).
    ///
    /// Pruned against the live path list on every rebuild rather than by watching
    /// `AccessPointRemoved`, which is the same signal that asks for the rebuild anyway.
    access_points: Arc<Mutex<HashMap<OwnedObjectPath, AccessPointProxy<'static>>>>,
    /// `oblisk.network`'s own push state (ADR-0037), mutated only by
    /// [`handle_signal`](Self::handle_signal). `Mutex` because the controller is `Clone`; never
    /// held across an await.
    state: Arc<Mutex<NetworkState>>,
    /// The single pending `network:connect` intent slot, see [`PendingNetworkConnect`].
    pending_connect: Arc<Mutex<Option<PendingNetworkConnect>>>,
    /// Clone of the signal channel's sender: routes [`mark_scanning`](Self::mark_scanning)'s
    /// immediate flip through the same FIFO as real signals, and keeps the channel open with no
    /// Wi-Fi device.
    events: UnboundedSender<NetworkSignal>,
}

impl NetworkController {
    /// Connects to NetworkManager over `connection` (the Supervisor's system-bus connection),
    /// resolves the Wi-Fi and Ethernet device sets, and spawns the forwarders feeding `events`
    /// (ADR-0037). A device whose `DeviceType` can't be read is logged and skipped, not fatal to
    /// startup.
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
                // Every D-Bus read happens before the lock is taken: it is a plain `Mutex` and is
                // never held across an await.
                let mut next = self.build_state().await;
                let mut state = self.state.lock().unwrap();
                next.scanning = signal == NetworkSignal::Changed && state.scanning;
                // Neither of these is derivable from NetworkManager -- they are a memory of an
                // attempt, not a reading of the stack -- so they ride across the re-derive the
                // way `scanning` does. Taken rather than cloned: `state` is overwritten below.
                next.connecting_ssid = state.connecting_ssid.take();
                next.connect_error = state.connect_error.take();
                next.password_ssid = state.password_ssid.take();
                *state = next;
                state.clone()
            }
        }
    }

    /// Every §2.5 field except `scanning`, read fresh from NetworkManager. Each read falls back to
    /// its `Default` on error rather than aborting the rebuild: one unreadable property should cost
    /// its own field, not the whole snapshot.
    async fn build_state(&self) -> NetworkState {
        let available_networks = self.build_available_networks().await;
        let associated = available_networks.iter().find(|ap| ap.active);
        // `PrimaryConnection` is `/` when nothing holds the default route, and names the active
        // connection that does otherwise -- so its type is what says whether "connected" is wired.
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

    /// Whether any wired device reached `ACTIVATED`. Any, not all: one cable carrying traffic is
    /// what a panel's Ethernet row is asking about, however many ports the machine has.
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

    /// The immediate half of `network:scan()`: queues [`NetworkSignal::ScanStarted`] so `scanning`
    /// flips to `true` on initiation, not once `RequestScan` completes. Only when a Wi-Fi device
    /// exists, since with none [`scan`](Self::scan) no-ops and `scanning` would stay stuck `true`.
    pub fn mark_scanning(&self) {
        if self.wifi.is_some() {
            let _ = self.events.send(NetworkSignal::ScanStarted);
        }
    }

    /// Stashes a `network:connect(ssid, hidden)` intent until its paired
    /// `secure_submit(network, connect)` arrives. Newest intent wins (single slot).
    pub fn stash_connect_intent(&self, pending: PendingNetworkConnect) {
        *self.pending_connect.lock().unwrap() = Some(pending);
    }

    /// Takes the pending connect intent, if any: the `secure_submit(network, connect)` arm's
    /// one consumer.
    pub fn take_connect_intent(&self) -> Option<PendingNetworkConnect> {
        self.pending_connect.lock().unwrap().take()
    }

    /// Decides what a just-stashed `network:connect` intent still needs, and either completes it
    /// or asks the shell for a password.
    ///
    /// Three outcomes, and they are `NetworkPanel.qml`'s own three. A network this machine already
    /// has a profile for, and an open one, join on the click with nothing typed -- the QML joins a
    /// `known` or unsecured access point with a bare `connectToSsid(ssid, "")` for the same reason.
    /// Only a secured network with no profile has anything left to ask, and that one sets
    /// [`NetworkState::password_ssid`] and waits for `secure_submit(network, connect)` to release
    /// it.
    ///
    /// Without the first two branches a click on such a row did nothing at all: `network:connect`
    /// only ever stashes, so every one of them left an intent nothing would consume.
    ///
    /// A hidden network takes the password branch however its security reads, because a hidden
    /// SSID has no access point in range to read it off. `showPasswordInput` in the QML defaults
    /// the same way (`?? true`), and the cost of guessing wrong is one keystroke on an open
    /// network against an unjoinable secured one.
    ///
    /// The SSID is looked up here and again inside `activate_intent`, one extra `ListConnections`
    /// walk of a handful of profiles. Threading the match down through `connect` would save a
    /// millisecond and cost three signatures.
    pub async fn resolve_connect_intent(&self) {
        let Some(pending) = self.pending_connect.lock().unwrap().clone() else {
            return;
        };
        let saved = !self.saved_profiles_for_ssid(&pending.ssid, "connect").await.is_empty();
        // An SSID with no access point in range is treated as secured for the same reason a hidden
        // one is: nothing here can say otherwise.
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
            // Logged at the fork rather than inside the branches: this is the one decision that
            // decides whether the shell asks for a password, and the two facts behind it come from
            // different places (NetworkManager's saved profiles, the last scan's AP list), so a
            // prompt that fails to appear is otherwise three guesses about which one was wrong.
            eprintln!("network: connect {:?}: saved={saved} secure={secure}, asking for a password", pending.ssid);
            self.request_password(&pending.ssid);
            return;
        }
        eprintln!("network: connect {:?}: saved={saved} secure={secure}, connecting directly", pending.ssid);
        // Re-taken rather than assumed: a second `network:connect` may have replaced the intent
        // while the lookup above was on the wire, and the newest one wins (single slot).
        if let Some(pending) = self.take_connect_intent() {
            self.connect(pending, Vec::new()).await;
        }
    }

    /// Puts the shell into asking-for-a-password, and pushes so the prompt appears on the click.
    /// The intent stays stashed: it is what `secure_submit(network, connect)` will consume.
    fn request_password(&self, ssid: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.password_ssid = Some(ssid.to_string());
            state.connect_error = None;
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// `network:cancel_connect()`: drops the pending intent and stops asking for a password.
    ///
    /// The way out of a prompt, and the only one -- Escape inside a `secure_submit` field clears
    /// what was typed but stays in the field (`wayland::input`'s `SecureKeyAction::Clear`), so
    /// without this a prompt raised by a mis-click would hold the bar's keyboard focus until
    /// something else took it.
    ///
    /// ponytail: this does not touch an activation already in flight, though
    /// `NetworkService.qml`'s `cancelConnect` disconnects the device when nothing else is live.
    /// Once NetworkManager has the request, letting it finish and report costs a few seconds;
    /// racing it costs a `Disconnect` that can land on a session that just came up.
    ///
    /// A no-op when there is nothing to cancel, and that is what makes it callable from a panel
    /// close (`modules/shell/panel_host.lua`'s click-outside catcher) rather than only from the
    /// prompt's own close button. Without the guard, every click that shut any panel would clear
    /// `connect_error` -- wiping the one line that says why the last attempt failed, at the moment
    /// the user closed the panel to go read it somewhere else -- and push a `Changed` for it.
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

    /// § 4.1: `NetworkingEnabled` is a NetworkManager read-only property; only
    /// `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled` have setters. The only real way to toggle
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
    /// existing autoconnect profile, if any, and is a no-op for a device with none, since no NM
    /// method fabricates a carrier connection without a profile already present.
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
            // No profile exists for this device, nothing D-Bus can do about that (ADR-0029).
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
    /// logged, not a panic: a system with no wireless hardware still runs everything else.
    pub async fn scan(&self) {
        let Some(wifi) = &self.wifi else {
            eprintln!("network: scan() requested but no Wi-Fi device is present");
            return;
        };
        if let Err(err) = wifi.wireless.request_scan(HashMap::new()).await {
            eprintln!("network: RequestScan failed: {err}");
        }
    }

    /// § 4.2: fully re-queries the Wi-Fi device's current AP list, deduplicated and capped at
    /// the top 20 by strength (ADR-0029: no debounce). Empty, not an error, with no Wi-Fi device.
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

    /// Binds whichever of `paths` is not held yet, drops whatever is held and no longer in range,
    /// and hands back the live proxies in `paths` order. The returned proxies are clones, which
    /// share the held one's property cache rather than starting a cold one.
    ///
    /// The lock is taken twice around the binding rather than once across it: it is a plain
    /// `Mutex` and binding is an await.
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

    /// Supervisor services § 4: `pending`'s SSID/hidden flag plus `secret` (empty means open,
    /// non-empty means WPA-PSK) become `AddAndActivateConnection2`'s connection dict.
    /// `secret` is zeroized on every outcome (ADR-0005/ADR-0014): the caller already
    /// `mem::take`s it out of the wire `SecureSubmit` frame, making this that plaintext's owner.
    pub async fn connect(&self, pending: PendingNetworkConnect, mut secret: Vec<u8>) {
        self.begin_connect(&pending.ssid);
        let result = self.connect_inner(&pending, &secret).await;
        secret.zeroize();
        match result {
            // NetworkManager has accepted the request, not completed it: the radio has not tried
            // yet, so whether it works is only knowable from the activation that comes back.
            Ok(active) => self.watch_activation(active, pending.ssid),
            Err(err) => {
                eprintln!("network: connect(ssid={:?}) failed: {err}", pending.ssid);
                self.finish_connect(&pending.ssid, Some(err.to_string()));
            }
        }
    }

    /// Marks an attempt in flight and clears the previous one's error, then pushes so a row can
    /// start spinning on the click rather than a round trip later -- `mark_scanning`'s posture,
    /// via the same channel for the same FIFO reason.
    fn begin_connect(&self, ssid: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.connecting_ssid = Some(ssid.to_string());
            state.connect_error = None;
            // The attempt is under way, so the prompt that raised it is answered. Clearing it here
            // rather than in the `secure_submit` arm covers the branches that never prompted too,
            // and is what drops the bar's keyboard focus the instant Enter is pressed.
            state.password_ssid = None;
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// Records an attempt's verdict and pushes it.
    ///
    /// A verdict for an SSID that is no longer the one in flight is dropped. Two overlapping
    /// attempts would otherwise let the older one's failure land on top of the newer one's
    /// spinner, which is why `connect` needs no "one at a time" refusal to go with this.
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

    /// Watches one activation to its verdict in the background, off the caller's path.
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

    /// `None` once the activation reaches `ACTIVATED`, the reason text if it deactivates instead.
    async fn activation_outcome(&self, active: &OwnedObjectPath) -> Option<String> {
        let generic = || Some("connection failed".to_string());
        let proxy = match bind_active_connection(&self.connection, active.clone()).await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("network: failed to bind the active connection {active}: {err}");
                return generic();
            }
        };
        // The signal, not `receive_state_changed()` -- that is the `State` *property* stream, which
        // says a connection went down without ever saying why.
        let mut changes = match proxy.receive_active_state_changed().await {
            Ok(changes) => changes,
            Err(err) => {
                eprintln!("network: failed to subscribe to StateChanged on {active}: {err}");
                return generic();
            }
        };

        // Subscribing happens after the activation call has already returned, so a verdict can land
        // in the gap. Reading the property once closes it.
        //
        // ponytail: a failure that lands in that gap loses its reason and reports the generic line,
        // since only the signal carries one. Success does not, which is the far likelier race.
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
        // The object was removed without ever reaching a terminal state.
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
        // intent.psk is the plaintext-password copy every dict below borrows from, zeroized
        // explicitly here rather than left to Drop alone (ADR-0005/ADR-0014): each of those dicts
        // is fully consumed by now, so this is the first point it's safe to mutate.
        if let Some(psk) = intent.psk.as_mut() {
            psk.zeroize();
        }
        result
    }

    /// Joins `intent`'s network, reusing a saved profile for the SSID when this machine has one.
    ///
    /// NetworkManager does not deduplicate profiles: `AddAndActivateConnection2` stores a new one
    /// every call, and it accepts a second profile with the same id *and* the same SSID without
    /// complaint. Creating unconditionally meant every re-join from the panel left another copy
    /// behind, and a stale one that autoconnect might pick over the good one.
    ///
    /// Returns the activation's own object path, which is where its outcome is reported.
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

        // A password typed for a network that is already saved is a correction to the stored one,
        // so it is written back rather than dropped on the floor -- otherwise a profile saved with
        // the wrong key could never be fixed from the panel, only forgotten and re-added.
        //
        // ponytail: skipped for enterprise profiles. `GetSettings` omits secrets, so rebuilding
        // one from its own read-back would drop the 802.1X password along with it; NM's saved copy
        // is the better bet until there is a secret agent to answer for one (ADR-0029 leaves
        // agents out of scope).
        if let Some(psk) = &intent.psk
            && !saved.settings.contains_key("802-1x")
        {
            saved.connection.update(merge_psk(&saved.settings, psk)).await?;
        }
        Ok(self.nm.activate_connection(&saved.path, &wifi.device_path, &root_object_path()).await?)
    }

    /// Every saved Wi-Fi profile for `ssid`, each paired with the settings dict it was matched on.
    /// `context` names the caller in the log lines, since both callers reach here for different
    /// reasons and a bare "failed to read settings" would not say which.
    ///
    /// Plural because §4.3's `forget` must delete them all; `connect` takes the first. One
    /// `ListConnections` walk serves both, which is why the two do not each have their own.
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

    /// Supervisor services § 4: deletes every connection profile matching `ssid` (plural, per
    /// spec, not just the first match).
    pub async fn forget(&self, ssid: &str) {
        for profile in self.saved_profiles_for_ssid(ssid, "forget").await {
            if let Err(err) = profile.connection.delete().await {
                eprintln!("network: forget({ssid:?}) failed to delete profile {}: {err}", profile.path);
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
    CancelConnect,
    Forget,
}

/// `oblisk.network`'s action dispatch (ADR-0037). Write actions are `tokio::spawn`ed rather than
/// awaited inline (ADR-0029); `connect` only stashes its intent until the paired
/// `secure_submit(network, connect)` arrives.
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
        // Not spawned: it touches no D-Bus, and a cancel that lands a turn late is a prompt that
        // reappears after the click that dismissed it.
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

/// Runs until `wireless`'s connection drops, forwarding the `org.freedesktop.NetworkManager
/// .Device.Wireless` events to `events` as [`NetworkSignal`]s. Spawned once from
/// [`NetworkController::new`] with its own `WirelessProxy` clone, keeping the borrow-heavy stream
/// types local to this task rather than threading them through `main.rs`'s top-level `select!`. A
/// dropped `events` receiver (shutdown) ends the task on its next forward attempt, the same
/// posture every channel-forwarding task here takes.
///
/// `ActiveAccessPoint` is here for the reason the other three are not enough: it is the only thing
/// on this interface that moves when the radio joins or leaves a network. Watching only the AP set
/// and `LastScan` left `connected` frozen at whatever the last scan happened to see, and a machine
/// that associated after the bar started read offline until the next scan, minutes later.
///
/// It also owns the associated AP's strength watch, re-targeted every time the association moves
/// and aborted with the association it belonged to -- an orphaned one would keep asking for
/// rebuilds on behalf of an AP nothing is connected to (ADR-0082).
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
        // Set by the `active_ap_changed` arm below, including its first emission, which zbus sends
        // when the property cache fills -- so an association that predates this task is watched
        // without a startup read of its own.
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

/// Forwards the associated access point's `Strength` as [`NetworkSignal::Changed`], so the bars on
/// the bar drop as you walk away from the router rather than holding the last scan's number.
/// `None` for the `/` path NetworkManager reports when nothing is associated.
///
/// The associated AP only, not every AP in range, and that is a measured choice: over 180 seconds
/// on real hardware the associated AP emitted 26 times (a 6-second poll that stays quiet while the
/// number holds) against 76 across all 17 APs in range, one every 2.4 seconds indefinitely. A
/// rebuild re-reads every AP's strength anyway, so watching the association alone keeps the whole
/// list just as fresh on a third of the traffic -- which is also why this needs no debounce, the
/// case ADR-0029 item 6 said to reconsider only against real numbers.
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

/// Forwards one device's `State` as [`NetworkSignal::Changed`]. Spawned per device, Wi-Fi and
/// every Ethernet one alike, because that is what `ethernet_enabled` moves on and it is also how a
/// Wi-Fi disconnect announces itself before `ActiveAccessPoint` catches up.
///
/// zbus emits a property stream's current value once when the cache first fills, so this also
/// primes the very first snapshot without a separate startup read.
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

/// Forwards the manager-wide properties `NetworkState` reads: the two radio switches and whatever
/// holds the default route. A device forwarder cannot cover these -- switching networking off
/// leaves the devices where they are, and the default route can move between two devices that both
/// stay activated.
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
