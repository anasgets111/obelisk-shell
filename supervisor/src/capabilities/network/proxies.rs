//! Hand-written NetworkManager proxies, declaring only the members `obelisk.network` calls
//! (ADR-0212).

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

// Enum values from `nm-dbus-interface.h`.
pub(super) const DEVICE_TYPE_ETHERNET: u32 = 1;
pub(super) const DEVICE_TYPE_WIFI: u32 = 2;
pub(super) const DEVICE_STATE_ACTIVATED: u32 = 100;
pub(super) const REASON_NO_SECRETS: u32 = 7;
pub(super) const REASON_SUPPLICANT_TIMEOUT: u32 = 11;
pub(super) const REASON_USER_REQUESTED: u32 = 39;
pub(super) const REASON_SSID_NOT_FOUND: u32 = 53;
pub(super) const ACTIVE_STATE_ACTIVATED: u32 = 2;
pub(super) const ACTIVE_STATE_DEACTIVATED: u32 = 4;
pub(super) const AP_FLAGS_PRIVACY: u32 = 0x1;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
pub(super) trait NetworkManager {
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn add_and_activate_connection2(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath, HashMap<String, OwnedValue>)>;
    fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;
    fn enable(&self, enable: bool) -> zbus::Result<()>;
    fn get_all_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(signal)]
    fn device_added(&self, device_path: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn device_removed(&self, device_path: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(property)]
    fn networking_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn primary_connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn primary_connection_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn wireless_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_wireless_enabled(&self, value: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
pub(super) trait Settings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(signal)]
    fn new_connection(&self, connection: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn connection_removed(&self, connection: ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(super) trait SettingsConnection {
    fn delete(&self) -> zbus::Result<()>;
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
    fn save(&self) -> zbus::Result<()>;
    fn update_unsaved(&self, properties: HashMap<&str, HashMap<&str, Value<'_>>>) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Device", default_service = "org.freedesktop.NetworkManager")]
pub(super) trait Device {
    fn disconnect(&self) -> zbus::Result<()>;
    /// Renamed so it does not collide with the `state` property's `receive_state_changed`.
    #[zbus(signal, name = "StateChanged")]
    fn device_state_changed(&self, new_state: u32, old_state: u32, reason: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn available_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(super) trait Wireless {
    fn get_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn request_scan(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn access_point_added(&self, access_point: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn access_point_removed(&self, access_point: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn last_scan(&self) -> zbus::Result<i64>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wired",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(super) trait Wired {
    #[zbus(property)]
    fn speed(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(super) trait AccessPoint {
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(super) trait IP4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}
