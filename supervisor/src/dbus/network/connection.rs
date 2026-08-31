//! Pure connection-intent helpers for `oblisk.network`: band/security classification,
//! AP dedup, NetworkManager connection-dict construction, and write-command arg parsers.
//! Split from `dbus::network` -- see `dbus/network/mod.rs` for the module-level doc.

use std::collections::HashMap;

use rusty_network_manager::NM80211ApFlags;
use shared::Zeroize;
use zbus::zvariant::{OwnedValue, Value};

use super::AccessPointInfo;

/// How many deduplicated access points [`dedup_and_top20`] keeps (docs/oblisk-supervisor-
/// services-dbus.md §4.2: "serializes the top 20 access points").
const MAX_AVAILABLE_NETWORKS: usize = 20;

/// Failure modes [`NetworkController::connect`] can hit before ever reaching NetworkManager
/// itself. Logged via `Display` at the call site, not user-facing.
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

/// `[2400, 2500]` -> `"2.4 GHz"`, `[4900, 5900]` -> `"5 GHz"`, `[5925, 7125]` -> `"6 GHz"` (§4.2).
/// `None` outside all three ranges -- real Wi-Fi hardware always falls inside one, so this is an
/// honest "no band" rather than a guessed default.
pub(super) fn resolve_band(freq_mhz: u32) -> Option<&'static str> {
    match freq_mhz {
        2400..=2500 => Some("2.4 GHz"),
        4900..=5900 => Some("5 GHz"),
        5925..=7125 => Some("6 GHz"),
        _ => None,
    }
}

/// Whether an access point requires a security key: it advertises `PRIVACY` (WEP, the only case
/// that flag alone signals) or either RSN (WPA2/3) or WPA1 key-management flags are non-empty.
pub(super) fn access_point_is_secure(flags: u32, wpa_flags: u32, rsn_flags: u32) -> bool {
    let flags = NM80211ApFlags::from_bits_truncate(flags);
    flags.contains(NM80211ApFlags::PRIVACY) || wpa_flags != 0 || rsn_flags != 0
}

/// Merges duplicate SSIDs keeping the highest signal strength, then serializes the top 20 (§4.2).
/// Ties within the same SSID keep whichever entry was seen first -- a tie only happens between
/// two distinct BSSIDs broadcasting the same SSID, and picking either is equally correct.
///
/// `active` is merged rather than carried by the winner, and that distinction is the whole of a bug
/// this had: NetworkManager exposed two AP objects for one SSID at *the same BSSID*, strengths 62
/// and 58, with `ActiveAccessPoint` naming the weaker one. Keeping the stronger entry wholesale
/// dropped the flag with the object it came on, and a connected machine's bar read "offline".
///
/// Strength is a property of an AP object; `active` is a property of the SSID the radio is
/// associated with. So the strongest sighting wins the numbers and any sighting wins the flag.
pub(super) fn dedup_and_top20(aps: Vec<AccessPointInfo>) -> Vec<AccessPointInfo> {
    let mut best: HashMap<String, AccessPointInfo> = HashMap::new();
    for ap in aps {
        best.entry(ap.ssid.clone())
            .and_modify(|existing| {
                let active = existing.active || ap.active;
                if ap.strength > existing.strength {
                    *existing = ap.clone();
                }
                existing.active = active;
            })
            .or_insert(ap);
    }
    let mut deduped: Vec<AccessPointInfo> = best.into_values().collect();
    deduped.sort_by_key(|ap| std::cmp::Reverse(ap.strength));
    deduped.truncate(MAX_AVAILABLE_NETWORKS);
    deduped
}

/// Whether a `SettingsConnectionProxy::get_settings()` result's `connection.autoconnect` allows
/// autoconnect -- absent means NetworkManager's own default of `true` (ADR-0029: only an
/// explicit `false` disqualifies a profile).
pub(super) fn connection_wants_autoconnect(settings: &HashMap<String, HashMap<String, OwnedValue>>) -> bool {
    settings
        .get("connection")
        .and_then(|section| section.get("autoconnect"))
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(true)
}

/// Whether a `SettingsConnectionProxy::get_settings()` result is a Wi-Fi profile for `ssid`
/// (used by `forget`, which must delete every matching profile, not just the first -- §4.3 says
/// "profiles", plural).
pub(super) fn settings_match_ssid(settings: &HashMap<String, HashMap<String, OwnedValue>>, ssid: &str) -> bool {
    settings
        .get("802-11-wireless")
        .and_then(|section| section.get("ssid"))
        .and_then(|value| Vec::<u8>::try_from(value.clone()).ok())
        .is_some_and(|bytes| bytes == ssid.as_bytes())
}

/// The `network:connect(ssid, hidden)` intent plus the `secure_submit` secret, boiled down to
/// "open or WPA-PSK" before any zbus-specific `Value` wrapping -- kept unit-testable without a
/// live D-Bus connection. An empty secret means an open network (ADR-0029); a non-empty one must
/// be valid UTF-8 to become NM's `802-11-wireless-security.psk`, a D-Bus string -- an invalid
/// encoding fails loudly ([`ConnectError::InvalidSecret`]) instead of lossily mangling it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ConnectionIntent {
    pub(super) ssid: String,
    pub(super) hidden: bool,
    pub(super) psk: Option<String>,
}

pub(super) fn connection_intent(ssid: &str, hidden: bool, secret: &[u8]) -> Result<ConnectionIntent, ConnectError> {
    let psk = if secret.is_empty() {
        None
    } else {
        match String::from_utf8(secret.to_vec()) {
            Ok(psk) => Some(psk),
            Err(err) => {
                // The invalid-UTF-8 bytes are still a plaintext-password copy -- zeroize before
                // propagating (ADR-0005/ADR-0014). Message captured first since
                // FromUtf8Error::into_bytes consumes the error.
                let message = err.to_string();
                let mut bytes = err.into_bytes();
                bytes.zeroize();
                return Err(ConnectError::InvalidSecret(message));
            }
        }
    };
    Ok(ConnectionIntent { ssid: ssid.to_string(), hidden, psk })
}

/// Builds the minimal connection dict `AddAndActivateConnection2` needs for `intent` (§4.3):
/// `802-11-wireless-security` is present only for a secured intent, and `hidden`/`scan-ssid`
/// only when the intent's `hidden` flag is set.
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
        // Not a real NM setting key (hidden-network probing is driven by hidden alone) --
        // included anyway since §4.3 asks for both explicitly, and an extra key NM doesn't
        // recognize is silently ignored rather than rejected.
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

/// `network:set_networking_enabled(en)`'s `arguments: [en]`. Defined once in `dbus` (shared with
/// `bluetooth::parse_bool_arg`) and re-exported here.
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
    fn dedup_and_top20_keeps_active_even_when_a_stronger_duplicate_is_not_the_connected_one() {
        // Measured on a real session, not imagined: NetworkManager exposed two AP objects for one
        // SSID at the same BSSID, strengths 62 and 58, and `ActiveAccessPoint` named the 58. The
        // merge kept the 62 and dropped the flag with the object, so a connected machine's bar read
        // "offline". `active` is a property of the SSID the radio is associated with, not of the AP
        // object that happens to be advertising it loudest.
        let mut connected = ap("home", 58);
        connected.active = true;
        let merged = dedup_and_top20(vec![ap("home", 62), connected]);

        assert_eq!(merged.len(), 1);
        assert!(merged[0].active, "the connected SSID must stay marked connected");
        assert_eq!(merged[0].strength, 62, "and still report the strongest signal seen for it");
    }

    #[test]
    fn dedup_and_top20_keeps_active_regardless_of_which_duplicate_arrives_first() {
        let mut connected = ap("home", 58);
        connected.active = true;
        let merged = dedup_and_top20(vec![connected, ap("home", 62)]);

        assert_eq!(merged.len(), 1);
        assert!(merged[0].active);
        assert_eq!(merged[0].strength, 62);
    }

    #[test]
    fn dedup_and_top20_leaves_an_unconnected_ssid_unconnected() {
        // The flag is merged, not invented: two sightings of an SSID nothing is associated with
        // stay inactive.
        let merged = dedup_and_top20(vec![ap("home", 62), ap("home", 58)]);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].active);
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
