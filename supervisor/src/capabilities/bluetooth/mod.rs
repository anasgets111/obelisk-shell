//! BlueZ Bluetooth D-Bus controller (`oblisk.bluetooth`; docs/oblisk-supervisor-
//! services-dbus.md §5; docs/oblisk-idl-api-specs.md §2.6; ADR-0030).
//!
//! Proxies follow BlueZ's D-Bus API docs; `org.freedesktop.DBus.ObjectManager` reuses
//! `zbus::fdo::ObjectManagerProxy` (ADR-0030: no maintained BlueZ proxy crate).
//!
//! ponytail: [`BluetoothController::new`] degrades instead of failing like `NetworkController::new`
//! (`zbus::Result<Self>`). NetworkManager is assumed present for `oblisk.network`; BlueZ may be
//! absent with no hardware or no `bluetoothd`, so binding, adapter lookup, and agent registration
//! log and produce an inert controller: `enabled`/`discovering` are `false`, lists are empty, and
//! writes log and no-op. This extends `NetworkController::has_wifi_device` to a missing service.
//!
//! ponytail: After `start_discovery` clears the list and pushes a fresh Candidate, matching the
//! `last_snapshots` bookkeeping used by every capability, any
//! `DeviceRegistryChanged` rebuilds both lists from the entire registry, not only devices newly
//! seen this session, matching `NetworkController::build_available_networks`'s no-debounce
//! discipline. A prior-session device can reappear after any registry change because BlueZ
//! `Device1` objects persist and this controller does not track `RSSI`. This follows ADR-0030's
//! deferred mechanics; a strict session-scoped list is the upgrade if hardware shows it wrong.

use serde::Serialize;

/// Object path where this Supervisor exports `org.bluez.Agent1` on its unique connection name.
pub const AGENT_OBJECT_PATH: &str = "/org/oblisk/Bluez/Agent1";

pub mod agent;
pub mod controller;
pub mod proxies;
pub mod registry;

pub use controller::BluetoothController;

// State shape pushed as `oblisk.bluetooth`'s StateSnapshot (docs/oblisk-idl-api-specs.md §2.6).
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ConnectedDevice {
    /// Canonical MAC address, e.g. `"00:1A:7D:DA:71:11"`; every `bluetooth:` command uses it.
    pub mac: String,
    /// The device's advertised name.
    pub name: String,
    /// Battery percentage, or `-1` if unsupported/unknown (no `Battery1`, or `Percentage` failed),
    /// per the IDL.
    pub battery: i32,
    /// Always `None` for now; codec query/control is deferred (ADR-0030) to an `audio` capability
    /// with a live PipeWire `Device` proxy.
    pub codec: Option<String>,
    /// Drawing hint from the class of device: `"keyboard"`, `"mouse"`, `"headphones"`,
    /// `"headset"`, `"phone"`, `"computer"`, or `"generic"`. Choose an icon; it is not a
    /// capability.
    pub category: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct DiscoveredDevice {
    /// Canonical MAC address accepted by `bluetooth:pair(mac)`.
    pub mac: String,
    /// Advertised name, often empty when the device broadcasts only an address.
    pub name: String,
    /// Always `false`; every entry in this pool is unpaired (IDL contract).
    pub paired: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct BluetoothState {
    /// Whether the adapter is powered. `false` also means no adapter, so it does not prove
    /// Bluetooth hardware exists.
    pub enabled: bool,
    /// Whether discovery is running, which fills [`BluetoothState::discovered_devices`].
    pub discovering: bool,
    /// Paired, connected devices in BlueZ object order, which is not sorted.
    pub connected_devices: Vec<ConnectedDevice>,
    /// Unpaired devices seen by the running scan; empties when discovery stops.
    pub discovered_devices: Vec<DiscoveredDevice>,
}

/// What signal forwarders report to `main.rs`'s top-level `select!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothSignal {
    /// The adapter's own `Powered` or `Discovering` property changed.
    AdapterChanged,
    /// A device was added/removed, or its `Connected`/`Paired`/`Name`/`Battery1.Percentage`
    /// changed.
    DeviceRegistryChanged,
    /// Sent by [`BluetoothController::clear_discovered`], not a forwarder, when
    /// `bluetooth:start_discovery()` begins. It clears `discovered_devices` before
    /// `StartDiscovery` returns (ADR-0030); a distinct variant prevents registry re-derivation
    /// from immediately undoing the clear.
    DiscoveryCleared,
}

/// `bluetooth:*` write failures before reaching BlueZ; displayed only in call-site logs.
#[derive(Debug)]
enum BluetoothActionError {
    NoAdapter,
    UnknownDevice,
}

impl std::fmt::Display for BluetoothActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter => write!(f, "no Bluetooth adapter is present"),
            Self::UnknownDevice => write!(f, "no device with that MAC address has been observed via ObjectManager"),
        }
    }
}

impl std::error::Error for BluetoothActionError {}

/// Maps BlueZ `Class` bits 8-12 (Major) and 2-7 (Minor) (ADR-0030). Parses them instead of
/// trusting `Icon`, which is empty when `Class == 0`, common for BLE peripherals before GAP data.
fn class_to_category(class: u32) -> &'static str {
    let major = (class >> 8) & 0x1F;
    let minor = (class >> 2) & 0x3F;
    match major {
        0x01 => "computer",
        0x02 => "phone",
        0x04 => match minor {
            0x01 | 0x02 => "headset",
            0x06 => "headphones",
            _ => "generic",
        },
        0x05 => match (minor >> 4) & 0x3 {
            0b01 => "keyboard",
            0b10 => "mouse",
            0b11 => "keyboard",
            _ => "generic",
        },
        _ => "generic",
    }
}

/// `bluetooth:set_enabled(en)`'s `arguments: [en]`, shared with `network::parse_bool_arg` and
/// re-exported so existing `bluetooth::parse_bool_arg` callers stay unchanged.
pub use crate::capabilities::parse_bool_arg;

/// `bluetooth:pair`/`connect`/`disconnect`/`forget`'s `arguments: [mac]`.
pub fn parse_mac_arg(arguments: &[serde_json::Value]) -> Option<String> {
    Some(arguments.first()?.as_str()?.to_string())
}

/// Actions accepted by `oblisk.bluetooth:invoke(...)`; exhaustive dispatch keeps variants and arms
/// in sync.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BluetoothAction {
    SetEnabled,
    StartDiscovery,
    StopDiscovery,
    Pair,
    Connect,
    Disconnect,
    Forget,
}

/// `oblisk.bluetooth` action dispatch (ADR-0037): matches, parses, and `tokio::spawn`s each write
/// action rather than awaiting inline (ADR-0030). `stop_discovery` leaves the last
/// `discovered_devices` snapshot.
pub fn dispatch(controller: &BluetoothController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<BluetoothAction>(params) else { return };
    match action {
        BluetoothAction::SetEnabled => match parse_bool_arg(&params.arguments) {
            Some(enabled) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_enabled(enabled).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        BluetoothAction::StartDiscovery => {
            controller.clear_discovered();
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.start_discovery().await;
            });
        }
        BluetoothAction::StopDiscovery => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.stop_discovery().await;
            });
        }
        BluetoothAction::Pair => match parse_mac_arg(&params.arguments) {
            Some(mac) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.pair(&mac).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        BluetoothAction::Connect => match parse_mac_arg(&params.arguments) {
            Some(mac) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.connect(&mac).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        BluetoothAction::Disconnect => match parse_mac_arg(&params.arguments) {
            Some(mac) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.disconnect(&mac).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        BluetoothAction::Forget => match parse_mac_arg(&params.arguments) {
            Some(mac) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.forget(&mac).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- class_to_category ----

    #[test]
    fn class_to_category_maps_a_real_keyboards_class() {
        assert_eq!(class_to_category(0x002540), "keyboard");
    }

    #[test]
    fn class_to_category_maps_a_real_mouses_class() {
        assert_eq!(class_to_category(0x002580), "mouse");
    }

    #[test]
    fn class_to_category_maps_a_combo_peripheral_to_keyboard() {
        // Major 0x05 (Peripheral), minor top-2-bits `11` (combo keyboard/pointing device).
        let class = (0x05 << 8) | (0b11_0000 << 2);
        assert_eq!(class_to_category(class), "keyboard");
    }

    #[test]
    fn class_to_category_maps_an_unclassified_peripheral_to_generic() {
        // Major 0x05, minor top-2-bits `00` (uncategorized per the Bluetooth spec).
        assert_eq!(class_to_category(0x05 << 8), "generic");
    }

    #[test]
    fn class_to_category_maps_major_0x01_to_computer() {
        assert_eq!(class_to_category(0x01 << 8), "computer");
    }

    #[test]
    fn class_to_category_maps_major_0x02_to_phone() {
        assert_eq!(class_to_category(0x02 << 8), "phone");
    }

    #[test]
    fn class_to_category_maps_audio_video_minor_0x01_and_0x02_to_headset() {
        assert_eq!(class_to_category((0x04 << 8) | (0x01 << 2)), "headset");
        assert_eq!(class_to_category((0x04 << 8) | (0x02 << 2)), "headset");
    }

    #[test]
    fn class_to_category_maps_audio_video_minor_0x06_to_headphones() {
        assert_eq!(class_to_category((0x04 << 8) | (0x06 << 2)), "headphones");
    }

    #[test]
    fn class_to_category_maps_other_audio_video_minors_to_generic() {
        assert_eq!(class_to_category((0x04 << 8) | (0x03 << 2)), "generic");
    }

    #[test]
    fn class_to_category_maps_an_unmapped_major_to_generic() {
        // Major 0x03 is "LAN/Network Access Point", not a rendered category.
        assert_eq!(class_to_category(0x03 << 8), "generic");
    }

    #[test]
    fn class_to_category_ignores_service_class_bits() {
        // Real headsets carry Service Class bits above bit 12 (e.g. "Audio" = bit 21); ignore
        // them when extracting Major/Minor.
        let class = 0x24_0404_u32 & 0x00FF_FFFF;
        assert_eq!(class_to_category(class), "headset");
    }

    // ---- arg parsers ----

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
    fn parse_mac_arg_reads_the_first_argument() {
        assert_eq!(parse_mac_arg(&[serde_json::json!("00:1A:7D:DA:71:11")]), Some("00:1A:7D:DA:71:11".to_string()));
        assert_eq!(parse_mac_arg(&[]), None);
        assert_eq!(parse_mac_arg(&[serde_json::json!(42)]), None);
    }
}
