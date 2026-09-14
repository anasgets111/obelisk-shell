//! Saved NetworkManager profiles for `obelisk.network`: reading them, the saved-SSID cache,
//! `forget`, wired autoconnect, and shaping a typed key into a saved profile.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::proxies::{DeviceProxy, SettingsConnectionProxy};
use super::{NetworkController, root_object_path};
use crate::capabilities::bind;

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

/// Shapes a saved profile for `SettingsConnection.UpdateUnsaved`, changing only its PSK.
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

/// One saved profile matched by SSID: the path for `ActivateConnection`, its proxy, and the
/// settings already read by both callers.
pub(super) struct SavedProfile {
    pub(super) path: OwnedObjectPath,
    pub(super) connection: SettingsConnectionProxy<'static>,
    pub(super) settings: HashMap<String, HashMap<String, OwnedValue>>,
}

impl NetworkController {
    /// Every saved Wi-Fi profile for `ssid`, paired with the settings dict that matched it.
    /// `context` identifies the caller in logs. Plural because `forget` deletes all while
    /// `connect` takes the first; one `ListConnections` walk serves both.
    pub(super) async fn saved_profiles_for_ssid(&self, ssid: &str, context: &str) -> Vec<SavedProfile> {
        let mut profiles = self.wifi_profiles(&format!("{context}({ssid:?})")).await;
        profiles.retain(|profile| profile_ssid(&profile.settings).is_some_and(|bytes| bytes == ssid.as_bytes()));
        profiles
    }

    /// Every saved Wi-Fi profile. Unreadable profiles are logged under `context` and skipped.
    async fn wifi_profiles(&self, context: &str) -> Vec<SavedProfile> {
        let paths = match self.settings.list_connections().await {
            Ok(paths) => paths,
            Err(err) => {
                eprintln!("network: {context} failed to list connections: {err}");
                return Vec::new();
            }
        };
        let mut profiles = self.read_profiles(paths, context).await;
        profiles.retain(|profile| profile_ssid(&profile.settings).is_some());
        profiles
    }

    /// Binds and reads each profile at `paths`. Unreadable profiles are logged under `context` and
    /// skipped.
    async fn read_profiles(&self, paths: Vec<OwnedObjectPath>, context: &str) -> Vec<SavedProfile> {
        let mut profiles = Vec::new();
        for path in paths {
            let connection = match bind::<SettingsConnectionProxy>(&self.connection, path.clone()).await {
                Ok(connection) => connection,
                Err(err) => {
                    eprintln!("network: {context} failed to bind connection {path}: {err}");
                    continue;
                }
            };
            match connection.get_settings().await {
                Ok(settings) => profiles.push(SavedProfile { path, connection, settings }),
                Err(err) => eprintln!("network: {context} failed to read settings for {path}: {err}"),
            }
        }
        profiles
    }

    /// Replaces the saved-SSID cache with one fresh `ListConnections` walk.
    pub(super) async fn refresh_saved_ssids(&self) {
        let ssids = self
            .wifi_profiles("saved networks")
            .await
            .iter()
            .filter_map(|profile| profile_ssid(&profile.settings))
            .collect();
        *self.saved_ssids.lock().unwrap() = ssids;
    }

    /// Deletes every connection profile matching `ssid`.
    pub async fn forget(&self, ssid: &str) {
        for profile in self.saved_profiles_for_ssid(ssid, "forget").await {
            if let Err(err) = profile.connection.delete().await {
                eprintln!("network: forget({ssid:?}) failed to delete profile {}: {err}", profile.path);
            }
        }
    }

    pub(super) async fn activate_autoconnect_profile(&self, device: &DeviceProxy<'_>, device_path: &OwnedObjectPath) {
        let profile = match self.find_autoconnect_profile(device).await {
            Ok(profile) => profile,
            Err(err) => {
                eprintln!("network: failed to inspect connections for ethernet device {device_path}: {err}");
                return;
            }
        };
        let Some(conn_path) = profile else {
            // No profile exists; D-Bus cannot create one here (ADR-0029).
            return;
        };
        if let Err(err) = self.nm.activate_connection(&conn_path, device_path, &root_object_path()).await {
            eprintln!("network: failed to activate ethernet profile {conn_path} on {device_path}: {err}");
        }
    }

    /// ponytail: reads every profile the device lists before picking the first autoconnect one; a
    /// wired port lists one or two.
    async fn find_autoconnect_profile(&self, device: &DeviceProxy<'_>) -> zbus::Result<Option<OwnedObjectPath>> {
        let profiles = self.read_profiles(device.available_connections().await?, "autoconnect").await;
        Ok(profiles
            .into_iter()
            .find(|profile| connection_wants_autoconnect(&profile.settings))
            .map(|profile| profile.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn profile_ssid_reads_the_wireless_sections_ssid_bytes() {
        let settings =
            settings_with("802-11-wireless", "ssid", OwnedValue::try_from(Value::from(b"HomeWifi".to_vec())).unwrap());
        assert_eq!(profile_ssid(&settings).as_deref(), Some(&b"HomeWifi"[..]));
        assert_eq!(profile_ssid(&HashMap::new()), None, "a profile without a wireless section has no SSID");
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
}
