//! Pure connection-intent helpers for `obelisk.network`: band/security classification, AP dedup,
//! link-name resolution, NetworkManager dict construction, and write-command arg parsers; see
//! `network/mod.rs` for the module-level contract.

use std::collections::HashMap;

use rusty_network_manager::NM80211ApFlags;
use rusty_network_manager::dbus_interface_types::NMDeviceStateReason;
use shared::Zeroize;
use zbus::zvariant::{OwnedValue, Value};

use super::AccessPointInfo;

/// How many deduplicated APs [`dedup_and_top20`] keeps (docs/services.md
/// §4.2: "serializes the top 20 access points").
const MAX_AVAILABLE_NETWORKS: usize = 20;

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

/// `[2400, 2500]` -> `"2.4 GHz"`, `[4900, 5900]` -> `"5 GHz"`, `[5925, 7125]` -> `"6 GHz"`
/// (§4.2). Real Wi-Fi hardware falls inside one range, so `None` is honest "no band", not a
/// guessed default.
pub(super) fn resolve_band(freq_mhz: u32) -> Option<&'static str> {
    match freq_mhz {
        2400..=2500 => Some("2.4 GHz"),
        4900..=5900 => Some("5 GHz"),
        5925..=7125 => Some("6 GHz"),
        _ => None,
    }
}

/// Whether an AP requires a key: `PRIVACY` alone signals WEP; non-empty RSN (WPA2/3) or WPA1
/// key-management flags signal the other secured cases.
pub(super) fn access_point_is_secure(flags: u32, wpa_flags: u32, rsn_flags: u32) -> bool {
    let flags = NM80211ApFlags::from_bits_truncate(flags);
    flags.contains(NM80211ApFlags::PRIVACY) || wpa_flags != 0 || rsn_flags != 0
}

/// Merges duplicate SSIDs by highest strength, then serializes the connected one plus the strongest
/// 19 (§4.2). Equal-strength duplicates keep the first sighting; a tie is only between distinct
/// BSSIDs broadcasting the same SSID, so either choice is equally correct.
///
/// `active` sorts ahead of strength because `build_state` reads `ssid` and `strength` here. Without
/// it, an association weaker than 20 neighbours is truncated and an online machine reports no
/// association. Dense apartment RF reaches 20 SSIDs easily.
///
/// `saved` sorts next, so a saved network weaker than 20 neighbours still reaches the panel's saved
/// section. `NetworkPanel.qml` has no cap. ponytail: more than 20 saved networks in range still
/// truncate by strength. Upgrade path: exempt saved rows from the cap.
///
/// Merge `active` rather than carrying the winner's flag. NetworkManager once exposed two AP
/// objects for one SSID at the same BSSID, strengths 62 and 58, with `ActiveAccessPoint` naming the
/// 58. Keeping the stronger object dropped the flag and showed a connected machine as "offline".
///
/// Strength belongs to an AP object; `active` belongs to the associated SSID. The strongest
/// sighting supplies the numbers, and any sighting supplies the flag.
///
/// SSID is the last tiebreak because `HashMap::into_values` reshuffles as APs come and go. Stable
/// sorting then prevents equal-strength rows from swapping, including at the 20th-place cutoff.
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
    deduped.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then(right.saved.cmp(&left.saved))
            .then(right.strength.cmp(&left.strength))
            .then_with(|| left.ssid.cmp(&right.ssid))
    });
    deduped.truncate(MAX_AVAILABLE_NETWORKS);
    deduped
}

/// § 2.5's `ssid`: `"Ethernet"` for a wired default route, the associated AP's name otherwise,
/// and `None` when neither holds, which Lua reads as `nil` for offline.
///
/// Wired wins because `ssid` names what `NetworkState::connected` describes. A docked laptop may
/// stay joined to Wi-Fi, but the association is not carrying the default route.
pub(super) fn resolve_ssid(wired: bool, associated: Option<&AccessPointInfo>) -> Option<String> {
    match (wired, associated) {
        (true, _) => Some("Ethernet".to_string()),
        (false, Some(ap)) => Some(ap.ssid.clone()),
        (false, None) => None,
    }
}

/// Whether `get_settings()` permits autoconnect. Missing means NetworkManager's default `true`
/// (ADR-0029: only explicit `false` disqualifies a profile).
pub(super) fn connection_wants_autoconnect(settings: &HashMap<String, HashMap<String, OwnedValue>>) -> bool {
    settings
        .get("connection")
        .and_then(|section| section.get("autoconnect"))
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(true)
}

/// The SSID bytes of a Wi-Fi profile's `get_settings()`, or `None` for any other connection type.
pub(super) fn profile_ssid(settings: &HashMap<String, HashMap<String, OwnedValue>>) -> Option<Vec<u8>> {
    settings
        .get("802-11-wireless")
        .and_then(|section| section.get("ssid"))
        .and_then(|value| Vec::<u8>::try_from(value.clone()).ok())
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

/// Shapes a saved profile for `SettingsConnection.UpdateUnsaved` (§4.3), changing only its PSK.
/// The update replaces the whole profile, so static addresses, route metrics, and autoconnect
/// priority pass through; an open profile gains a `wpa-psk` section. `None` when the profile's
/// `key-mgmt` takes no PSK: forcing `wpa-psk` downgraded SAE, and 802.1X would lose the password
/// `GetSettings` omits until a secret agent exists (ADR-0029).
pub(super) fn merge_psk<'a>(
    settings: &'a HashMap<String, HashMap<String, OwnedValue>>,
    psk: &'a str,
) -> Option<HashMap<&'a str, HashMap<&'a str, Value<'a>>>> {
    let key_mgmt = settings
        .get("802-11-wireless-security")
        .and_then(|security| security.get("key-mgmt"))
        .and_then(|value| String::try_from(value.clone()).ok());
    if !matches!(key_mgmt.as_deref(), None | Some("wpa-psk" | "sae")) {
        return None;
    }
    let mut merged: HashMap<&str, HashMap<&str, Value>> = settings
        .iter()
        .map(|(section, keys)| {
            (section.as_str(), keys.iter().map(|(key, value)| (key.as_str(), Value::from(value.clone()))).collect())
        })
        .collect();

    let security = merged.entry("802-11-wireless-security").or_default();
    security.entry("key-mgmt").or_insert_with(|| Value::new("wpa-psk"));
    security.insert("psk", Value::new(psk));
    Some(merged)
}

/// `network:set_networking_enabled(en)`'s `arguments: [en]`. Defined once in `dbus` (shared with
/// `bluetooth::parse_bool_arg`) and re-exported here.
pub use crate::capabilities::parse_bool_arg;

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
        AccessPointInfo {
            ssid: ssid.to_string(),
            strength,
            secure: false,
            band: "2.4 GHz".to_string(),
            active: false,
            saved: false,
        }
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
        // Real session: the same BSSID appeared as strengths 62 and 58, with `ActiveAccessPoint`
        // naming 58. Keeping 62 alone dropped `active` and showed "offline".
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
        // Merge the flag, but do not invent it for an unassociated SSID.
        let merged = dedup_and_top20(vec![ap("home", 62), ap("home", 58)]);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].active);
    }

    #[test]
    fn dedup_and_top20_keeps_the_connected_network_even_when_20_neighbours_are_stronger() {
        // `build_state` reads `ssid` and `strength` here; truncating the association reports an
        // online machine as joined to nothing.
        let mut aps: Vec<AccessPointInfo> = (0..25).map(|i| ap(&format!("neighbour{i}"), 50 + i as u8)).collect();
        let mut connected = ap("home", 20);
        connected.active = true;
        aps.push(connected);

        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert!(merged[0].active, "the connected network leads the list");
        assert_eq!(merged[0].ssid, "home");
    }

    #[test]
    fn dedup_and_top20_keeps_a_saved_network_even_when_20_neighbours_are_stronger() {
        let mut aps: Vec<AccessPointInfo> = (0..25).map(|i| ap(&format!("neighbour{i}"), 50 + i as u8)).collect();
        let mut office = ap("office", 20);
        office.saved = true;
        aps.push(office);

        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert_eq!(merged[0].ssid, "office", "a saved network outranks every unsaved one");
    }

    #[test]
    fn dedup_and_top20_sorts_by_strength_descending() {
        let result = dedup_and_top20(vec![ap("weak", 10), ap("strong", 90), ap("mid", 50)]);
        assert_eq!(result.iter().map(|a| a.ssid.as_str()).collect::<Vec<_>>(), vec!["strong", "mid", "weak"]);
    }

    #[test]
    fn dedup_and_top20_breaks_strength_ties_by_ssid_so_the_order_is_deterministic() {
        // `HashMap::into_values` makes the input order nondeterministic as APs change. Eight equal
        // strengths make an accidental pass a one-in-40320 shot.
        let aps: Vec<AccessPointInfo> = ["delta", "alpha", "hotel", "charlie", "golf", "bravo", "foxtrot", "echo"]
            .iter()
            .map(|ssid| ap(ssid, 55))
            .collect();

        let merged = dedup_and_top20(aps);
        let names: Vec<&str> = merged.iter().map(|a| a.ssid.as_str()).collect();
        assert_eq!(names, ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]);
    }

    #[test]
    fn dedup_and_top20_cuts_a_boundary_tie_by_ssid_rather_than_by_luck() {
        // Nineteen strong entries and two tied for slot 20 must yield the same row on every
        // rebuild; otherwise the panel flickers without a radio change.
        let mut aps: Vec<AccessPointInfo> = (0..19).map(|i| ap(&format!("strong{i}"), 90)).collect();
        aps.push(ap("zulu", 40));
        aps.push(ap("kilo", 40));

        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert_eq!(merged[19].ssid, "kilo", "the alphabetically-first of the tied pair keeps the last slot");
    }

    #[test]
    fn dedup_and_top20_truncates_to_20() {
        let aps: Vec<AccessPointInfo> = (0..30).map(|i| ap(&format!("ap{i}"), i as u8)).collect();
        assert_eq!(dedup_and_top20(aps).len(), 20);
    }

    #[test]
    fn dedup_and_top20_keeps_the_20_strongest_not_just_the_first_20() {
        let mut aps: Vec<AccessPointInfo> = (0..30).map(|i| ap(&format!("ap{i}"), i as u8)).collect();
        // Reverse the input so a naive "take the first 20" implementation fails.
        aps.reverse();
        let result = dedup_and_top20(aps);
        assert!(result.iter().all(|a| a.strength >= 10), "must keep the strongest 20, not the first 20 seen");
    }

    fn settings_with(section: &str, key: &str, value: OwnedValue) -> HashMap<String, HashMap<String, OwnedValue>> {
        HashMap::from([(section.to_string(), HashMap::from([(key.to_string(), value)]))])
    }

    #[test]
    fn resolve_ssid_names_the_associated_network_over_wi_fi() {
        assert_eq!(resolve_ssid(false, Some(&ap("home", 70))), Some("home".to_string()));
    }

    #[test]
    fn resolve_ssid_says_ethernet_even_while_wi_fi_stays_associated() {
        // Both links stay joined, but `connected` describes the cable's default route.
        assert_eq!(resolve_ssid(true, Some(&ap("home", 70))), Some("Ethernet".to_string()));
    }

    #[test]
    fn resolve_ssid_is_none_when_nothing_is_joined() {
        assert_eq!(resolve_ssid(false, None), None);
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
    fn profile_ssid_reads_the_wireless_sections_ssid_bytes() {
        let settings =
            settings_with("802-11-wireless", "ssid", OwnedValue::try_from(Value::from(b"HomeWifi".to_vec())).unwrap());
        assert_eq!(profile_ssid(&settings).as_deref(), Some(&b"HomeWifi"[..]));
        assert_eq!(profile_ssid(&HashMap::new()), None, "a profile without a wireless section has no SSID");
    }

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
    fn merge_psk_passes_every_other_section_through_untouched() {
        // `Update` replaces the whole profile; dropping any section loses static addresses,
        // metrics, or autoconnect priority.
        let settings: HashMap<String, HashMap<String, OwnedValue>> = HashMap::from([
            (
                "connection".to_string(),
                HashMap::from([("id".to_string(), OwnedValue::try_from(Value::from("HomeWifi")).unwrap())]),
            ),
            (
                "ipv4".to_string(),
                HashMap::from([("method".to_string(), OwnedValue::try_from(Value::from("manual")).unwrap())]),
            ),
        ]);

        let merged = merge_psk(&settings, "hunter2").unwrap();
        assert_eq!(String::try_from(merged["connection"]["id"].clone()).unwrap(), "HomeWifi");
        assert_eq!(String::try_from(merged["ipv4"]["method"].clone()).unwrap(), "manual");
    }

    #[test]
    fn merge_psk_adds_a_security_section_to_a_profile_saved_as_open() {
        // An open profile has no security section, so add `key-mgmt` beside `psk`.
        let settings =
            settings_with("802-11-wireless", "ssid", OwnedValue::try_from(Value::from(b"HomeWifi".to_vec())).unwrap());

        let merged = merge_psk(&settings, "hunter2").unwrap();
        let security = &merged["802-11-wireless-security"];
        assert_eq!(String::try_from(security["key-mgmt"].clone()).unwrap(), "wpa-psk");
        assert_eq!(String::try_from(security["psk"].clone()).unwrap(), "hunter2");
    }

    #[test]
    fn merge_psk_replaces_a_key_the_profile_already_carries() {
        let settings =
            settings_with("802-11-wireless-security", "psk", OwnedValue::try_from(Value::from("stale")).unwrap());

        let merged = merge_psk(&settings, "hunter2").unwrap();
        assert_eq!(String::try_from(merged["802-11-wireless-security"]["psk"].clone()).unwrap(), "hunter2");
    }

    #[test]
    fn merge_psk_keeps_sae_and_refuses_key_types_that_take_no_psk() {
        let with = |key_mgmt: &str| {
            settings_with("802-11-wireless-security", "key-mgmt", OwnedValue::try_from(Value::from(key_mgmt)).unwrap())
        };
        let sae = with("sae");
        let merged = merge_psk(&sae, "hunter2").unwrap();
        assert_eq!(String::try_from(merged["802-11-wireless-security"]["key-mgmt"].clone()).unwrap(), "sae");
        for key_mgmt in ["wpa-eap", "owe", "none"] {
            assert!(merge_psk(&with(key_mgmt), "hunter2").is_none(), "{key_mgmt} takes no PSK");
        }
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
