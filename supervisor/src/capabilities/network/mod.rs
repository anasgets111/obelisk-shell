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

use rusty_network_manager::dbus_interface_types::{NMActiveConnectionState, NMDeviceState};
use rusty_network_manager::{
    AccessPointProxy, DeviceProxy, IP4ConfigProxy, NetworkManagerProxy, SettingsConnectionProxy, SettingsProxy,
};
use serde::Serialize;
use shared::Zeroize;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::capabilities::bind;

mod connect;
mod devices;
mod profiles;
mod scan;

use connect::ConnectError;
use connect::{ConnectionIntent, activation_verdict, build_connection_dict, connection_intent};
pub use connect::{parse_connect_args, parse_ssid_arg};
use devices::{Devices, EthernetDevice, WifiDevice, forward, resolve_devices, spawn_manager_forwarder, watch_devices};
use profiles::merge_psk;
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

/// [`NetworkController::watch_activation`]'s backstop timeout. NetworkManager normally gives up
/// well inside 45s and reports `StateChanged`; this covers an activation object that stops
/// answering without pre-empting NM or leaving a spinner stuck.
const ACTIVATION_CEILING: std::time::Duration = std::time::Duration::from_secs(45);

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

/// A join NM has accepted: the activation that reports its verdict, and the profile it created.
#[derive(Debug, Clone, PartialEq)]
struct InFlight {
    active: OwnedObjectPath,
    /// Set when `AddAndActivateConnection2` saved a new profile for this join, which an abort
    /// deletes rather than leaving a half-typed key saved.
    created: Option<OwnedObjectPath>,
    /// Set when a typed key updated a saved profile in memory only, which
    /// [`save_typed_key`](NetworkController::save_typed_key) writes to disk once NM accepts the join.
    unsaved: Option<OwnedObjectPath>,
}

/// The latest `network:connect` attempt. `id` advances on every begin and abort, so a join NM
/// accepts, or a verdict that arrives, after its attempt was aborted or replaced matches nothing.
/// The SSID cannot tell an abort and re-click of the same network apart.
#[derive(Debug, Default)]
struct Attempt {
    id: u64,
    /// The join NM accepted for `id`, which [`abort_connect`](NetworkController::abort_connect) stops.
    joined: Option<InFlight>,
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

    /// Stashes `network:connect(ssid, hidden)` until paired `secure_submit(network, connect)`.
    /// Newest intent wins.
    pub fn stash_connect_intent(&self, pending: PendingNetworkConnect) {
        *self.pending_connect.lock().unwrap() = Some(pending);
    }

    /// Takes the pending intent for `secure_submit(network, connect)`, only while the prompt names
    /// it. The frame carries no SSID, so a click that replaced the intent under an open prompt
    /// would otherwise join, and save the key into, a network the key was never typed for.
    pub fn take_prompted_intent(&self) -> Option<PendingNetworkConnect> {
        let state = self.state.lock().unwrap();
        let prompted = state.password_ssid.as_deref();
        self.pending_connect.lock().unwrap().take_if(|pending| prompted == Some(pending.ssid.as_str()))
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
            self.request_password(&pending);
            return;
        }
        eprintln!("network: connect {:?}: saved={saved} secure={secure}, connecting directly", pending.ssid);
        // Only this click's intent: another connect may have replaced it while the lookup was on the
        // wire, and that one resolves itself.
        let taken = self.pending_connect.lock().unwrap().take_if(|current| *current == pending);
        if let Some(pending) = taken {
            self.connect(pending, shared::Zeroizing::new(Vec::new())).await;
        }
    }

    /// Shows the password prompt for `pending` and pushes it immediately, unless a newer click or a
    /// rejected join replaced the intent during the lookup. The intent stays stashed for
    /// `secure_submit(network, connect)`.
    fn request_password(&self, pending: &PendingNetworkConnect) {
        {
            let mut state = self.state.lock().unwrap();
            if self.pending_connect.lock().unwrap().as_ref() != Some(pending) {
                return;
            }
            state.password_ssid = Some(pending.ssid.clone());
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
    /// An activation already in flight is left alone. Every panel close calls this, and closing the
    /// panel should not undo a join the user started; [`abort_connect`](Self::abort_connect) is the
    /// sheet's explicit Cancel.
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

    /// `network:abort_connect()`: clears `connecting_ssid` and advances the [`Attempt`], so the join's
    /// late acceptance or verdict matches nothing, then [`stop`](Self::stop)s that join. No-op without
    /// an attempt. Before NM accepts, [`accept`](Self::accept) refuses the join and `connect` stops it.
    pub fn abort_connect(&self) {
        let in_flight = {
            let mut state = self.state.lock().unwrap();
            if state.connecting_ssid.take().is_none() {
                return;
            }
            state.connect_error = None;
            let mut attempt = self.attempt.lock().unwrap();
            attempt.id += 1;
            attempt.joined.take()
        };
        eprintln!("network: the join in flight was aborted");
        let _ = self.events.send(NetworkSignal::Changed);
        if let Some(in_flight) = in_flight {
            let controller = self.clone();
            tokio::spawn(async move {
                controller.stop(&in_flight).await;
            });
        }
    }

    /// Stops an aborted join. It deletes a profile the join created, which also ends the activation
    /// and leaves no half-typed key saved; a join through an existing profile is only deactivated.
    async fn stop(&self, in_flight: &InFlight) {
        let result = match &in_flight.created {
            Some(created) => match bind::<SettingsConnectionProxy>(&self.connection, created.clone()).await {
                Ok(connection) => connection.delete().await,
                Err(err) => Err(err),
            },
            None => self.nm.deactivate_connection(&in_flight.active).await,
        };
        if let Err(err) = result {
            eprintln!("network: failed to stop the aborted join {}: {err}", in_flight.active);
        }
    }

    /// Writes a typed key to disk once NM accepted the join. `UpdateUnsaved` held it in memory until
    /// then, so a rejected key never replaces a good one on disk, where `GetSettings` could not read
    /// it back. No-op for a join that typed no key.
    ///
    /// ponytail: a rejected or aborted key stays in memory, shadowing the good one, until NM
    /// restarts or a later key is accepted. `ReloadConnections` would drop it, but polkit asks
    /// `auth_admin_keep` for it, an admin password per typo. Upgrade path: `GetSecrets` before the
    /// update, restored on failure.
    async fn save_typed_key(&self, in_flight: &InFlight) {
        let Some(profile) = &in_flight.unsaved else { return };
        let result = match bind::<SettingsConnectionProxy>(&self.connection, profile.clone()).await {
            Ok(connection) => connection.save().await,
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            eprintln!("network: failed to save the accepted key for {profile}: {err}");
        }
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

    /// Supervisor services §4: turns `pending` and `secret` (empty open, non-empty WPA-PSK) into
    /// `AddAndActivateConnection2`'s dict. The caller `mem::take`s `secret` from the wire frame,
    /// making this function its owner (ADR-0005/ADR-0014).
    ///
    /// `Zeroizing`, not a bare `Vec`, for the reason `pam_worker`'s two entry points take one: this
    /// runs in a spawned task, and cancelling it mid-activation drops the future without running
    /// anything written after the `await`.
    pub async fn connect(&self, pending: PendingNetworkConnect, secret: shared::Zeroizing<Vec<u8>>) {
        let attempt = self.begin_connect(&pending.ssid);
        let result = self.connect_inner(&pending, &secret).await;
        // Straight after the read, not at end of scope: the reporting below logs and takes a lock,
        // and none of it needs the plaintext alive.
        drop(secret);
        match result {
            // NM accepted the request, not completed it; the activation reports the verdict.
            Ok(in_flight) if self.accept(attempt, &in_flight) => self.watch_activation(attempt, in_flight, pending),
            Ok(in_flight) => self.stop(&in_flight).await,
            Err(err) => {
                eprintln!("network: connect(ssid={:?}) failed: {err}", pending.ssid);
                self.finish_connect(attempt, &pending, Some(err.to_string()), false);
            }
        }
    }

    /// Marks an attempt, clears its previous error, and pushes through the same FIFO as scanning so
    /// the row spins on the click. Returns the attempt's id.
    fn begin_connect(&self, ssid: &str) -> u64 {
        let id = {
            let mut state = self.state.lock().unwrap();
            state.connecting_ssid = Some(ssid.to_string());
            state.connect_error = None;
            // The attempt answers the prompt. Clear here, not only in `secure_submit`, so direct
            // connects also drop bar keyboard focus on Enter.
            state.password_ssid = None;
            let mut attempt = self.attempt.lock().unwrap();
            attempt.id += 1;
            // The new attempt owns no join until NM accepts it, so an abort before then cannot
            // stop the previous one.
            attempt.joined = None;
            attempt.id
        };
        let _ = self.events.send(NetworkSignal::Changed);
        id
    }

    /// Records the join NM accepted for `attempt`. `false` when an abort or a newer attempt came
    /// first, and the caller stops the join instead of watching it.
    fn accept(&self, attempt: u64, in_flight: &InFlight) -> bool {
        let mut current = self.attempt.lock().unwrap();
        let live = current.id == attempt;
        if live {
            current.joined = Some(in_flight.clone());
        }
        live
    }

    /// Records and pushes `attempt`'s verdict, only while `attempt` is the latest, so an aborted or
    /// older join's verdict cannot land on a newer spinner. `ask_password` parks the intent again and
    /// raises the prompt, keeping `connect_error` as the reason, unless a click made while the join
    /// ran already holds the slot.
    fn finish_connect(&self, attempt: u64, pending: &PendingNetworkConnect, error: Option<String>, ask_password: bool) {
        {
            let mut state = self.state.lock().unwrap();
            let mut current = self.attempt.lock().unwrap();
            if current.id != attempt {
                return;
            }
            state.connecting_ssid = None;
            state.connect_error = error.map(|message| JoinError { ssid: pending.ssid.clone(), message });
            current.joined = None;
            let mut slot = self.pending_connect.lock().unwrap();
            if ask_password && slot.is_none() {
                state.password_ssid = Some(pending.ssid.clone());
                *slot = Some(pending.clone());
            }
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// Watches one activation in the background. A rejected key reopens the password prompt, or every
    /// later click would reuse the saved bad key. Not for 802.1X, whose profile never takes a typed
    /// PSK, so the prompt would loop.
    fn watch_activation(&self, attempt: u64, in_flight: InFlight, pending: PendingNetworkConnect) {
        let controller = self.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(ACTIVATION_CEILING, controller.activation_outcome(&in_flight.active)).await.ok();
            if matches!(outcome, Some(Ok(()))) {
                controller.save_typed_key(&in_flight).await;
            }
            let (error, rejected_key) = activation_verdict(outcome);
            let ask_password = rejected_key
                && !controller
                    .saved_profiles_for_ssid(&pending.ssid, "connect")
                    .await
                    .iter()
                    .any(|profile| profile.settings.contains_key("802-1x"));
            controller.finish_connect(attempt, &pending, error, ask_password);
        });
    }

    /// `Ok` on `ACTIVATED`. `Err` on deactivation, carrying the Wi-Fi device's last `StateChanged`
    /// reason. The active connection's own reason cannot say a key was rejected: NM reports every
    /// device failure to it as `DEVICE_DISCONNECTED` (`nm-act-request.c`). NM emits the device
    /// signal first (`_set_state_full`), and `biased` reads it first.
    async fn activation_outcome(&self, active: &OwnedObjectPath) -> Result<(), Option<u32>> {
        let proxy = match bind::<ActiveConnectionProxy>(&self.connection, active.clone()).await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("network: failed to bind the active connection {active}: {err}");
                return Err(None);
            }
        };
        let Some(wifi) = self.wifi() else { return Err(None) };
        // Use the signals, not `receive_state_changed()`: the property stream gives no reason.
        let (mut changes, mut device_changes) =
            match tokio::try_join!(proxy.receive_active_state_changed(), wifi.device.receive_device_state_changed()) {
                Ok(streams) => streams,
                Err(err) => {
                    eprintln!("network: failed to subscribe to StateChanged for {active}: {err}");
                    return Err(None);
                }
            };

        // Subscription follows activation, so a verdict can land in the gap. Read the property
        // once.
        //
        // ponytail: a failure in that gap loses its reason and reports the generic line; only the
        // signal carries it. Success does not, and is the likelier race.
        match proxy.state().await.map(NMActiveConnectionState::try_from) {
            Ok(Ok(NMActiveConnectionState::ACTIVATED)) => return Ok(()),
            Ok(Ok(NMActiveConnectionState::DEACTIVATED)) => return Err(None),
            _ => {}
        }

        let mut reason = None;
        loop {
            tokio::select! {
                biased;
                Some(change) = device_changes.next() => {
                    // Leaving FAILED, NM queues DISCONNECTED with reason NONE (`nm-device.c`), and
                    // `biased` can drain both before the verdict; NONE must not erase the reason.
                    if let Ok(args) = change.args()
                        && args.reason != 0
                    {
                        reason = Some(args.reason);
                    }
                }
                change = changes.next() => {
                    // `None`: the object disappeared without a terminal state.
                    let Some(change) = change else { return Err(reason) };
                    let Ok(args) = change.args() else { continue };
                    match NMActiveConnectionState::try_from(args.state) {
                        Ok(NMActiveConnectionState::ACTIVATED) => return Ok(()),
                        Ok(NMActiveConnectionState::DEACTIVATED) => return Err(reason),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn connect_inner(&self, pending: &PendingNetworkConnect, secret: &[u8]) -> Result<InFlight, ConnectError> {
        let wifi = self.wifi().ok_or(ConnectError::NoWifiDevice)?;
        let mut intent = connection_intent(&pending.ssid, pending.hidden, secret)?;
        let result = self.activate_intent(&intent, &wifi).await;
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
    /// where its outcome is reported, and the profile it created, if it created one.
    async fn activate_intent(&self, intent: &ConnectionIntent, wifi: &WifiDevice) -> Result<InFlight, ConnectError> {
        let Some(saved) = self.saved_profiles_for_ssid(&intent.ssid, "connect").await.into_iter().next() else {
            let dict = build_connection_dict(intent);
            let (created, active, _) = self
                .nm
                .add_and_activate_connection2(dict, &wifi.device_path, &root_object_path(), HashMap::new())
                .await?;
            return Ok(InFlight { active, created: Some(created), unsaved: None });
        };

        // A typed password corrects the saved key; otherwise a bad profile could only be forgotten
        // and re-added. In memory only until NM accepts it (`save_typed_key`), so a typo never
        // replaces a good key on disk.
        let unsaved = match intent.psk.as_ref().and_then(|psk| merge_psk(&saved.settings, psk)) {
            Some(merged) => {
                saved.connection.update_unsaved(merged).await?;
                Some(saved.path.clone())
            }
            None => None,
        };
        let active = self.nm.activate_connection(&saved.path, &wifi.device_path, &root_object_path()).await?;
        Ok(InFlight { active, created: None, unsaved })
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

#[cfg(test)]
mod tests {
    use rusty_network_manager::WirelessProxy;

    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A controller mid-attempt on `connecting`, bound to a peer serving what `serve` installs
    /// (`Ok` for nothing). `finish_connect` only touches the state and intent slots.
    async fn attempting<F>(
        connecting: &str,
        serve: F,
    ) -> (NetworkController, UnboundedReceiver<NetworkSignal>, zbus::Connection)
    where
        F: FnOnce(zbus::connection::Builder<'static>) -> zbus::Result<zbus::connection::Builder<'static>>,
    {
        let (connection, peer) = p2p_pair_serving(serve).await;
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let controller = NetworkController {
            nm: NetworkManagerProxy::new(&connection).await.expect("binding makes no call"),
            settings: SettingsProxy::new(&connection).await.expect("binding makes no call"),
            connection,
            devices: Arc::default(),
            access_points: Arc::default(),
            saved_ssids: Arc::default(),
            state: Arc::new(Mutex::new(NetworkState {
                connecting_ssid: Some(connecting.to_string()),
                ..NetworkState::default()
            })),
            pending_connect: Arc::default(),
            attempt: Arc::default(),
            events,
        };
        (controller, receiver, peer)
    }

    fn home() -> PendingNetworkConnect {
        PendingNetworkConnect { ssid: "home".to_string(), hidden: true }
    }

    fn joined(n: u32) -> InFlight {
        let active = OwnedObjectPath::try_from(format!("/org/freedesktop/NetworkManager/ActiveConnection/{n}"))
            .expect("valid object path");
        InFlight { active, created: None, unsaved: None }
    }

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

    #[tokio::test]
    async fn a_rejected_key_reopens_the_prompt_for_the_same_network() {
        let (controller, mut receiver, _peer) = attempting("home", Ok).await;

        controller.finish_connect(0, &home(), Some("wrong password".to_string()), true);

        let state = controller.state.lock().unwrap().clone();
        assert_eq!(state.connecting_ssid, None);
        assert_eq!(state.password_ssid.as_deref(), Some("home"));
        assert_eq!(
            state.connect_error,
            Some(JoinError { ssid: "home".to_string(), message: "wrong password".to_string() }),
            "the prompt says why it is back, and for which network"
        );
        assert_eq!(controller.take_prompted_intent(), Some(home()), "the typed key needs an intent to pair with");
        assert_eq!(receiver.try_recv(), Ok(NetworkSignal::Changed));
    }

    #[tokio::test]
    async fn a_key_typed_for_one_network_never_joins_another() {
        let (controller, _receiver, _peer) = attempting("home", Ok).await;
        let office = PendingNetworkConnect { ssid: "office".to_string(), hidden: false };

        // The sheet asks for home, then a click on office takes the slot before Enter.
        controller.stash_connect_intent(home());
        controller.request_password(&home());
        controller.stash_connect_intent(office.clone());
        assert_eq!(controller.take_prompted_intent(), None, "the key was typed for home");

        // Home's lookup finishing late raises no prompt over office's intent.
        controller.state.lock().unwrap().password_ssid = None;
        controller.request_password(&home());
        assert_eq!(controller.state.lock().unwrap().password_ssid, None);

        // Nor does a key rejected for home while office was clicked.
        controller.finish_connect(0, &home(), Some("wrong password".to_string()), true);
        assert_eq!(controller.take_prompted_intent(), None);
        assert_eq!(*controller.pending_connect.lock().unwrap(), Some(office));
    }

    #[tokio::test]
    async fn a_join_aborted_and_clicked_again_never_stands_in_for_the_new_one() {
        // Both attempts name "home"; only the attempt id tells the first join from the second.
        let (controller, mut receiver, _peer) = attempting("home", Ok).await;
        let first = controller.begin_connect("home");
        controller.abort_connect();
        assert_eq!(controller.state.lock().unwrap().connecting_ssid, None, "the spinner stops on the click");
        let second = controller.begin_connect("home");

        assert!(!controller.accept(first, &joined(1)), "the first join is stopped, not watched");
        assert!(controller.accept(second, &joined(2)));

        controller.finish_connect(first, &home(), Some("disconnected".to_string()), false);
        let state = controller.state.lock().unwrap().clone();
        assert_eq!((state.connecting_ssid.as_deref(), state.connect_error), (Some("home"), None));
        let pushes = std::iter::from_fn(|| receiver.try_recv().ok()).count();
        assert_eq!(pushes, 3, "two begins and the abort push; the aborted join's verdict does not");

        controller.finish_connect(second, &home(), None, false);
        let state = controller.state.lock().unwrap().clone();
        assert_eq!((state.connecting_ssid, state.connect_error), (None, None));
        assert_eq!(controller.attempt.lock().unwrap().joined, None);
    }

    #[tokio::test]
    async fn a_new_attempt_owns_no_join_until_nm_accepts_it() {
        let (controller, _receiver, _peer) = attempting("home", Ok).await;
        assert!(controller.accept(0, &joined(1)));

        controller.begin_connect("office");

        assert_eq!(controller.attempt.lock().unwrap().joined, None, "an abort now must not stop home's join");
    }

    /// One saved profile, counting each `Save`.
    struct FakeProfile(Arc<std::sync::atomic::AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl FakeProfile {
        async fn save(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn an_accepted_join_saves_only_the_key_it_typed() {
        const PROFILE: &str = "/org/freedesktop/NetworkManager/Settings/9";
        let saves = Arc::default();
        let profile = FakeProfile(Arc::clone(&saves));
        let (controller, _receiver, _peer) = attempting("home", |peer| peer.serve_at(PROFILE, profile)).await;
        let typed = InFlight { unsaved: Some(OwnedObjectPath::try_from(PROFILE).unwrap()), ..joined(1) };

        controller.save_typed_key(&joined(1)).await;
        controller.save_typed_key(&typed).await;

        assert_eq!(saves.load(std::sync::atomic::Ordering::SeqCst), 1, "a join that typed no key saves nothing");
    }

    const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/1";
    const DEVICE: &str = "/org/freedesktop/NetworkManager/Devices/4";

    /// An activation whose `State` read queues, ahead of its reply, what NM emits for a rejected key:
    /// device FAILED(NO_SECRETS), device DISCONNECTED(NONE), then the activation's DEACTIVATED.
    struct RejectedKey;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl RejectedKey {
        #[zbus(property)]
        async fn state(&self, #[zbus(connection)] connection: &zbus::Connection) -> u32 {
            let device = "org.freedesktop.NetworkManager.Device";
            for body in [(120u32, 50u32, 7u32), (30, 120, 0)] {
                connection.emit_signal(None::<&str>, DEVICE, device, "StateChanged", &body).await.unwrap();
            }
            let active = "org.freedesktop.NetworkManager.Connection.Active";
            connection.emit_signal(None::<&str>, ACTIVE, active, "StateChanged", &(4u32, 3u32)).await.unwrap();
            1
        }
    }

    #[tokio::test]
    async fn a_rejected_key_keeps_its_reason_past_the_disconnect_nm_queues_after_it() {
        let (controller, _receiver, _peer) = attempting("home", |peer| peer.serve_at(ACTIVE, RejectedKey)).await;
        let device_path = OwnedObjectPath::try_from(DEVICE).unwrap();
        let wifi = WifiDevice {
            device: bind::<DeviceProxy>(&controller.connection, device_path.clone()).await.unwrap(),
            wireless: bind::<WirelessProxy>(&controller.connection, device_path.clone()).await.unwrap(),
            device_path,
        };
        controller.devices.lock().unwrap().wifi = Some(wifi);

        let outcome = controller.activation_outcome(&OwnedObjectPath::try_from(ACTIVE).unwrap()).await;

        assert_eq!(outcome, Err(Some(7)), "NO_SECRETS, so the password prompt comes back");
    }
}
