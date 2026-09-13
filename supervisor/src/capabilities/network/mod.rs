//! NetworkManager D-Bus controller (`obelisk.network`; ADR-0029). It holds `rusty_network_manager`
//! proxies (ADR-0013) and merges their signal streams into `main.rs`'s top-level `tokio::select!`,
//! like `dbus::polkit`, rather than using a dedicated thread like `audio::mixer`.
//!
//! Forwarder tasks feed one channel: wireless APs/association, each device's state, the manager's
//! radio switches/default route, its device list, and saved-profile changes. ADR-0082: scan-only
//! watching left connected machines reading offline for minutes.
//!
//! A device added or removed after startup, such as a USB adapter, rescans the device set and
//! restarts its watchers ([`NetworkSignal::DevicesChanged`]).
//!
//! ponytail: only the first Wi-Fi device from `GetAllDevices` is tracked. Multiple adapters need a
//! device selector in `available_networks`/`scan`/`connect`; none exists.

use serde::Serialize;
use zbus::zvariant::ObjectPath;

mod connect;
mod controller;
mod devices;
mod profiles;
mod scan;

pub use controller::NetworkController;

/// One scanned AP, resolved to `network.available_networks` and serialized in a `StateSnapshot`
/// payload, same convention as `audio::mixer::AppStream`.
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

/// `obelisk.network`'s whole live state, not only its scan results. Every field is re-derived from
/// NetworkManager on each [`NetworkSignal`] (ADR-0029: no debounce or incremental state).
///
/// The AP list cannot answer "am I online": it has no wired link and cannot distinguish a powered
/// down radio from a powered radio with no association.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetworkState {
    /// A scan is in flight. Set when `network:scan()` is accepted, before NetworkManager confirms,
    /// so the spinner starts on the click.
    pub scanning: bool,
    /// A connection carries the default route, from `PrimaryConnection`. `/` means none,
    /// hence offline.
    pub connected: bool,
    /// Wi-Fi SSID, `"Ethernet"` for a wired default route, or `nil` with no association. Wired wins
    /// when both are up. An association negotiating DHCP has an `ssid` but `connected == false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// Associated AP strength, `0` to `100`, or `0` without Wi-Fi association. Read from the merged
    /// `available_networks` entry, so an indicator and the list agree.
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
    /// Kept while [`NetworkState::scanning`] is true so a drawn list does not blank; payload order is
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
    /// Sent by [`NetworkController::mark_scanning`] before `RequestScan` completes, through
    /// the same channel for FIFO ordering.
    ScanStarted,
    /// A saved profile was added or removed, so the saved-SSID cache is stale.
    SavedChanged,
    /// NetworkManager added or removed a device, so the device set is stale.
    DevicesChanged,
}

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
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
