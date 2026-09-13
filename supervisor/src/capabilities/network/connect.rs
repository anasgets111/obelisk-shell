//! `network:connect` for `obelisk.network`: the pending intent and password prompt, the join NM
//! accepts and its verdict, and the pure helpers that shape an intent into NetworkManager's dict.
use std::collections::HashMap;

use rusty_network_manager::SettingsConnectionProxy;
use rusty_network_manager::dbus_interface_types::NMActiveConnectionState;
use rusty_network_manager::dbus_interface_types::NMDeviceStateReason;
use shared::Zeroize;
use tokio_stream::StreamExt;
use zbus::zvariant::{OwnedObjectPath, Value};

use super::devices::WifiDevice;
use super::profiles::merge_psk;
use super::{JoinError, NetworkController, NetworkSignal, PendingNetworkConnect, root_object_path};
use crate::capabilities::bind;

/// Failures before [`NetworkController::connect`] reaches NetworkManager. `Display` supplies both
/// the log line and `NetworkState::connect_error`, so each says the attempt never started.
#[derive(Debug)]
pub(super) enum ConnectError {
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

/// Maps the Wi-Fi device's `StateChanged` reason for a failed join to display text, following
/// `NetworkService.qml`'s `_connectErrorText`. The active connection's reason is no use here: it is
/// `DEVICE_DISCONNECTED` for every device failure.
///
/// Other reasons are wired, modem, or dependency failures. A Wi-Fi row cannot act on them, so they
/// collapse to "connection failed".
fn connect_error_text(reason: u32) -> &'static str {
    match NMDeviceStateReason::try_from(reason) {
        Ok(NMDeviceStateReason::NO_SECRETS) => "wrong password",
        Ok(NMDeviceStateReason::SUPPLICANT_TIMEOUT) => "connection timed out",
        Ok(NMDeviceStateReason::SSID_NOT_FOUND) => "network not found",
        Ok(NMDeviceStateReason::USER_REQUESTED) => "disconnected",
        _ => "connection failed",
    }
}

/// An activation's verdict: the error text, and whether NM rejected the key. `outcome` is `None`
/// when the Supervisor's own ceiling ran out, else `activation_outcome`'s result.
pub(super) fn activation_verdict(outcome: Option<Result<(), Option<u32>>>) -> (Option<String>, bool) {
    match outcome {
        None => (Some("connection timed out".to_string()), false),
        Some(Ok(())) => (None, false),
        Some(Err(reason)) => (
            Some(reason.map_or("connection failed", connect_error_text).to_string()),
            reason == Some(NMDeviceStateReason::NO_SECRETS as u32),
        ),
    }
}

/// The `network:connect(ssid, hidden)` intent plus `secure_submit` secret before zbus `Value`
/// wrapping, keeping this unit-testable without D-Bus. Empty means open (ADR-0029); non-empty
/// bytes must be UTF-8 for NM's string-valued `802-11-wireless-security.psk`, or fail as
/// [`ConnectError::InvalidSecret`] instead of being mangled.
#[derive(Clone, PartialEq)]
pub(super) struct ConnectionIntent {
    pub(super) ssid: String,
    pub(super) hidden: bool,
    /// Scrubbed when the intent drops, on every path including an error return (ADR-0005), matching
    /// what [`connection_intent`]'s invalid-UTF-8 arm does for the bytes it rejects. This is the
    /// Supervisor, which never restarts, on a heap measured never to return its pages.
    ///
    /// It does not reach the copy `zbus` makes inside the `Value` [`build_connection_dict`] builds;
    /// that allocation is the D-Bus layer's and not ours to scrub, and is shorter-lived than this.
    pub(super) psk: Option<shared::Zeroizing<String>>,
}

/// Hand-written so a password cannot reach a log line through `{:?}`, following
/// `shared::SecureSubmit`'s `Debug`. The derive would have printed it in full.
impl std::fmt::Debug for ConnectionIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionIntent")
            .field("ssid", &self.ssid)
            .field("hidden", &self.hidden)
            .field("psk", &format_args!("{}", if self.psk.is_some() { "<redacted>" } else { "none" }))
            .finish()
    }
}

pub(super) fn connection_intent(ssid: &str, hidden: bool, secret: &[u8]) -> Result<ConnectionIntent, ConnectError> {
    let psk = if secret.is_empty() {
        None
    } else {
        match String::from_utf8(secret.to_vec()) {
            Ok(psk) => Some(shared::Zeroizing::new(psk)),
            Err(err) => {
                // Zeroize the invalid UTF-8 password copy before propagating (ADR-0005/ADR-0014).
                // Capture the message first because `into_bytes` consumes the error.
                let message = err.to_string();
                let mut bytes = err.into_bytes();
                bytes.zeroize();
                return Err(ConnectError::InvalidSecret(message));
            }
        }
    };
    Ok(ConnectionIntent { ssid: ssid.to_string(), hidden, psk })
}

/// Builds the minimal `AddAndActivateConnection2` dict (§4.3): security only for a secured intent,
/// and `hidden`/`scan-ssid` only when `intent.hidden` is set.
pub(super) fn build_connection_dict(intent: &ConnectionIntent) -> HashMap<&str, HashMap<&str, Value<'_>>> {
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
        // NM probes hidden networks from `hidden` alone, but §4.3 requires both. NM silently
        // ignores the extra `scan-ssid` key.
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

/// [`NetworkController::watch_activation`]'s backstop timeout. NetworkManager normally gives up
/// well inside 45s and reports `StateChanged`; this covers an activation object that stops
/// answering without pre-empting NM or leaving a spinner stuck.
const ACTIVATION_CEILING: std::time::Duration = std::time::Duration::from_secs(45);

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
pub(super) struct Attempt {
    id: u64,
    /// The join NM accepted for `id`, which [`abort_connect`](NetworkController::abort_connect) stops.
    joined: Option<InFlight>,
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

impl NetworkController {
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
    /// network without a profile sets [`NetworkState::password_ssid`](super::NetworkState::password_ssid) and waits for
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
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rusty_network_manager::{DeviceProxy, NetworkManagerProxy, SettingsProxy, WirelessProxy};
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::capabilities::network::NetworkState;
    use crate::capabilities::test_support::p2p_pair_serving;

    #[test]
    fn connection_intent_treats_an_empty_secret_as_open() {
        let intent = connection_intent("HomeWifi", false, &[]).unwrap();
        assert_eq!(intent.psk, None);
    }

    #[test]
    fn connection_intent_carries_a_non_empty_secret_as_the_psk() {
        let intent = connection_intent("HomeWifi", false, b"hunter2").unwrap();
        assert_eq!(intent.psk.as_ref().map(|psk| psk.as_str()), Some("hunter2"));
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
    fn only_the_device_reason_for_a_bad_key_reopens_the_prompt() {
        // NM fails the device with `NO_SECRETS` for a bad PSK; the active connection only says
        // `DEVICE_DISCONNECTED`.
        let failed = |reason: NMDeviceStateReason| activation_verdict(Some(Err(Some(reason as u32))));
        let verdict = |error: &str, rejected_key| (Some(error.to_string()), rejected_key);
        assert_eq!(failed(NMDeviceStateReason::NO_SECRETS), verdict("wrong password", true));
        assert_eq!(failed(NMDeviceStateReason::SUPPLICANT_TIMEOUT), verdict("connection timed out", false));
        assert_eq!(failed(NMDeviceStateReason::SSID_NOT_FOUND), verdict("network not found", false));
        assert_eq!(activation_verdict(Some(Err(None))), verdict("connection failed", false));
        assert_eq!(activation_verdict(None), verdict("connection timed out", false));
        assert_eq!(activation_verdict(Some(Ok(()))), (None, false));
    }

    #[test]
    fn connect_error_text_falls_back_for_reasons_a_wifi_row_cannot_act_on() {
        // `MODEM_FAILED` is a modem reason; 255 is not a reason at all.
        assert_eq!(connect_error_text(NMDeviceStateReason::MODEM_FAILED as u32), "connection failed");
        assert_eq!(connect_error_text(255), "connection failed");
    }

    /// The derive would have printed the password in full the first time an intent reached a log
    /// line or an `unwrap` panic message.
    #[test]
    fn debug_redacts_the_password_but_still_says_whether_there_is_one() {
        let secured = connection_intent("net", false, b"hunter2").unwrap();
        let rendered = format!("{secured:?}");
        assert!(!rendered.contains("hunter2"), "the password must not survive formatting: {rendered}");
        assert!(rendered.contains("<redacted>"), "but the field is still reported: {rendered}");
        assert!(rendered.contains("net"), "and everything that is not the password still prints");

        let open = connection_intent("net", false, b"").unwrap();
        assert!(format!("{open:?}").contains("none"), "an open network is distinguishable from a secured one");
    }

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
