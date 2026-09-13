//! Pure connection-intent helpers for `obelisk.network`: join verdicts, NetworkManager dict
//! construction, and write-command arg parsers; see `network/mod.rs` for the module-level contract.

use std::collections::HashMap;

use rusty_network_manager::dbus_interface_types::NMDeviceStateReason;
use shared::Zeroize;
use zbus::zvariant::Value;

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
}
