//! Pure connection-intent helpers for `oblisk.network`: band/security classification, AP dedup,
//! link-name resolution, NetworkManager connection-dict construction, and write-command arg
//! parsers. See `network/mod.rs` for the module-level doc.

use std::collections::HashMap;

use rusty_network_manager::NM80211ApFlags;
use rusty_network_manager::dbus_interface_types::NMActiveConnectionStateReason;
use shared::Zeroize;
use zbus::zvariant::{OwnedValue, Value};

use super::AccessPointInfo;

/// How many deduplicated access points [`dedup_and_top20`] keeps (docs/oblisk-supervisor-
/// services-dbus.md §4.2: "serializes the top 20 access points").
const MAX_AVAILABLE_NETWORKS: usize = 20;

/// Failure modes [`NetworkController::connect`] can hit before ever reaching NetworkManager
/// itself. `Display` is both the log line and `NetworkState::connect_error`'s text: these are all
/// "the attempt never started", which is worth saying in the panel as plainly as in the log.
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

/// Why an activation attempt ended, in words fit to draw -- `LockState::error`'s convention, and
/// the D-Bus half of what `NetworkService.qml` spells as `_connectErrorText`. The reason comes off
/// `Connection.Active`'s `StateChanged(state, reason)`, the only place NetworkManager says *why*
/// a connection went down; `State` alone just says that it did.
///
/// Everything outside the five named reasons collapses to one line on purpose: the rest are VPN
/// service failures, dependency failures and realize failures, none of which a Wi-Fi row can act
/// on and all of which read worse than "connection failed".
pub(super) fn connect_error_text(reason: u32) -> &'static str {
    match NMActiveConnectionStateReason::try_from(reason) {
        Ok(NMActiveConnectionStateReason::NO_SECRETS) => "wrong password",
        Ok(NMActiveConnectionStateReason::LOGIN_FAILED) => "authentication failed",
        Ok(NMActiveConnectionStateReason::CONNECT_TIMEOUT) => "connection timed out",
        Ok(NMActiveConnectionStateReason::USER_DISCONNECTED) => "disconnected",
        Ok(NMActiveConnectionStateReason::DEVICE_DISCONNECTED) => "device disconnected",
        _ => "connection failed",
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

/// Merges duplicate SSIDs keeping the highest signal strength, then serializes the connected one
/// plus the strongest 19 (§4.2). Ties within the same SSID keep whichever entry was seen first --
/// a tie only happens between two distinct BSSIDs broadcasting the same SSID, and picking either
/// is equally correct.
///
/// Sorting `active` ahead of strength is what keeps the connected network inside the cut, and that
/// is load-bearing rather than cosmetic: `build_state` reads `ssid` and `strength` off this list,
/// so an association weaker than 20 neighbours would otherwise be truncated away and reported as
/// no association at all, on a machine that is plainly online. Dense apartment RF reaches 20 SSIDs
/// easily.
///
/// `active` is merged rather than carried by the winner, and that distinction is the whole of a bug
/// this had: NetworkManager exposed two AP objects for one SSID at *the same BSSID*, strengths 62
/// and 58, with `ActiveAccessPoint` naming the weaker one. Keeping the stronger entry wholesale
/// dropped the flag with the object it came on, and a connected machine's bar read "offline".
///
/// Strength is a property of an AP object; `active` is a property of the SSID the radio is
/// associated with. So the strongest sighting wins the numbers and any sighting wins the flag.
///
/// The SSID tiebreak last is not cosmetic. `best` is a `HashMap`, so `into_values` hands these
/// over in an order that reshuffles as access points come and go, and a stable sort preserves
/// whatever that was: two APs at one strength would swap rows between rebuilds for no reason, and
/// a tie across the 20th place would decide arbitrarily which one gets cut.
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
        right.active.cmp(&left.active).then(right.strength.cmp(&left.strength)).then_with(|| left.ssid.cmp(&right.ssid))
    });
    deduped.truncate(MAX_AVAILABLE_NETWORKS);
    deduped
}

/// § 2.5's `ssid`: `"Ethernet"` when the default route is wired, the associated AP's name when it
/// is not, and `None` when neither holds -- which is what a config reads as `nil` for "offline".
///
/// Wired wins over an association rather than the other way round: `ssid` names whatever
/// `NetworkState::connected` is about, and a laptop docked over Ethernet stays joined to Wi-Fi the
/// whole time, so the association is the one that is not carrying anything.
pub(super) fn resolve_ssid(wired: bool, associated: Option<&AccessPointInfo>) -> Option<String> {
    match (wired, associated) {
        (true, _) => Some("Ethernet".to_string()),
        (false, Some(ap)) => Some(ap.ssid.clone()),
        (false, None) => None,
    }
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

/// A saved profile's own `GetSettings` read-back with `802-11-wireless-security` rewritten to
/// WPA-PSK and `psk`, shaped for `SettingsConnection.Update` (§4.3).
///
/// Update replaces the whole profile, so this passes every other section straight through: a
/// static address, a route metric or an autoconnect priority the user set on that profile survives
/// a re-typed password. Only the security section is touched, and `key-mgmt` alongside `psk`
/// because a profile saved as open has no security section to put a key into.
pub(super) fn merge_psk<'a>(
    settings: &'a HashMap<String, HashMap<String, OwnedValue>>,
    psk: &'a str,
) -> HashMap<&'a str, HashMap<&'a str, Value<'a>>> {
    let mut merged: HashMap<&str, HashMap<&str, Value>> = settings
        .iter()
        .map(|(section, keys)| {
            (section.as_str(), keys.iter().map(|(key, value)| (key.as_str(), Value::from(value.clone()))).collect())
        })
        .collect();

    let security = merged.entry("802-11-wireless-security").or_default();
    security.insert("key-mgmt", Value::new("wpa-psk"));
    security.insert("psk", Value::new(psk));
    merged
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
    fn dedup_and_top20_keeps_the_connected_network_even_when_20_neighbours_are_stronger() {
        // The failure this prevents is not a cosmetic ordering one: `build_state` reads `ssid` and
        // `strength` off this list, so truncating the association away reports a plainly-online
        // machine as joined to nothing.
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
    fn dedup_and_top20_sorts_by_strength_descending() {
        let result = dedup_and_top20(vec![ap("weak", 10), ap("strong", 90), ap("mid", 50)]);
        assert_eq!(result.iter().map(|a| a.ssid.as_str()).collect::<Vec<_>>(), vec!["strong", "mid", "weak"]);
    }

    #[test]
    fn dedup_and_top20_breaks_strength_ties_by_ssid_so_the_order_is_deterministic() {
        // `best.into_values()` is a HashMap drain, so without the tiebreak this order is whatever
        // the current hash layout happens to be, and it moves as access points come and go. Eight
        // equal-strength entries make an accidental pass a one-in-40320 shot.
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
        // Nineteen strong entries and two tied for the last slot. Which of the two survives has to
        // be the same answer on every rebuild, or the panel's twentieth row flickers between them
        // while nothing about the radio has changed.
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
    fn resolve_ssid_names_the_associated_network_over_wi_fi() {
        assert_eq!(resolve_ssid(false, Some(&ap("home", 70))), Some("home".to_string()));
    }

    #[test]
    fn resolve_ssid_says_ethernet_even_while_wi_fi_stays_associated() {
        // A docked laptop is joined to both. `connected` is about the default route, which is the
        // cable, so `ssid` has to name the cable too or the two fields describe different links.
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
    fn settings_match_ssid_compares_the_wireless_sections_ssid_bytes() {
        let settings =
            settings_with("802-11-wireless", "ssid", OwnedValue::try_from(Value::from(b"HomeWifi".to_vec())).unwrap());
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
    fn merge_psk_passes_every_other_section_through_untouched() {
        // `Update` replaces the whole profile, so anything this drops is silently lost from the
        // saved network: a static address, a metric, an autoconnect priority.
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

        let merged = merge_psk(&settings, "hunter2");
        assert_eq!(String::try_from(merged["connection"]["id"].clone()).unwrap(), "HomeWifi");
        assert_eq!(String::try_from(merged["ipv4"]["method"].clone()).unwrap(), "manual");
    }

    #[test]
    fn merge_psk_adds_a_security_section_to_a_profile_saved_as_open() {
        // A network that was open and now has a key has no `802-11-wireless-security` section at
        // all, so `key-mgmt` has to be written alongside the psk rather than assumed present.
        let settings =
            settings_with("802-11-wireless", "ssid", OwnedValue::try_from(Value::from(b"HomeWifi".to_vec())).unwrap());

        let merged = merge_psk(&settings, "hunter2");
        let security = &merged["802-11-wireless-security"];
        assert_eq!(String::try_from(security["key-mgmt"].clone()).unwrap(), "wpa-psk");
        assert_eq!(String::try_from(security["psk"].clone()).unwrap(), "hunter2");
    }

    #[test]
    fn merge_psk_replaces_a_key_the_profile_already_carries() {
        let settings =
            settings_with("802-11-wireless-security", "psk", OwnedValue::try_from(Value::from("stale")).unwrap());

        let merged = merge_psk(&settings, "hunter2");
        assert_eq!(String::try_from(merged["802-11-wireless-security"]["psk"].clone()).unwrap(), "hunter2");
    }

    #[test]
    fn connect_error_text_names_the_reason_a_wrong_password_arrives_as() {
        // NO_SECRETS is the one that matters: it is what NetworkManager reports for a bad PSK, and
        // the whole point of watching the activation rather than the AddAndActivate return value.
        assert_eq!(connect_error_text(NMActiveConnectionStateReason::NO_SECRETS as u32), "wrong password");
        assert_eq!(connect_error_text(NMActiveConnectionStateReason::LOGIN_FAILED as u32), "authentication failed");
        assert_eq!(connect_error_text(NMActiveConnectionStateReason::CONNECT_TIMEOUT as u32), "connection timed out");
    }

    #[test]
    fn connect_error_text_falls_back_for_reasons_a_wifi_row_cannot_act_on() {
        // SERVICE_START_FAILED is a VPN reason, and 255 is not a reason at all.
        assert_eq!(connect_error_text(NMActiveConnectionStateReason::SERVICE_START_FAILED as u32), "connection failed");
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
}
