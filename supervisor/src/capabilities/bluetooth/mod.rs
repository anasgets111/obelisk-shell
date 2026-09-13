//! BlueZ Bluetooth D-Bus controller (`obelisk.bluetooth`; docs/obelisk-supervisor-
//! services-dbus.md §5; docs/lua-api.md §2.6; ADR-0030).
//!
//! Proxies follow BlueZ's D-Bus API docs; `org.freedesktop.DBus.ObjectManager` reuses
//! `zbus::fdo::ObjectManagerProxy` (ADR-0030: no maintained BlueZ proxy crate).
//!
//! ponytail: [`BluetoothController::new`] degrades instead of failing like `NetworkController::new`
//! (`zbus::Result<Self>`). NetworkManager is assumed present for `obelisk.network`; BlueZ may be
//! absent with no hardware or no `bluetoothd`, so binding, adapter lookup, and agent registration
//! log and produce an inert controller: `enabled`/`discovering` are `false`, lists are empty, and
//! writes log and no-op. This extends `NetworkController::has_wifi_device` to a missing service.
//! An adapter BlueZ adds later is picked up; a `bluetoothd` that starts after the Supervisor is
//! not, because the `ObjectManager` binding and agent registration happen once. Upgrade path:
//! watch `org.bluez`'s `NameOwnerChanged` and rebuild.
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
pub const AGENT_OBJECT_PATH: &str = "/org/obelisk/Bluez/Agent1";

pub mod agent;
pub mod controller;
pub mod proxies;
pub mod registry;

pub use controller::BluetoothController;

// State shape pushed as `obelisk.bluetooth`'s StateSnapshot (docs/lua-api.md §2.6).
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
    /// Drawing hint from the class of device: `"keyboard"`, `"mouse"`, `"headphones"`,
    /// `"headset"`, `"phone"`, `"computer"`, or `"generic"`. Choose an icon; it is not a
    /// capability.
    pub category: String,
    /// Same as [`DiscoveredDevice::busy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PairedDevice {
    /// Canonical MAC address accepted by `bluetooth:connect(mac)` and `bluetooth:forget(mac)`.
    pub mac: String,
    /// The device's advertised name.
    pub name: String,
    /// Drawing hint, the same set as [`ConnectedDevice::category`].
    pub category: String,
    /// BlueZ refuses every connection to or from the device until it is unblocked.
    pub blocked: bool,
    /// Same as [`DiscoveredDevice::busy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct DiscoveredDevice {
    /// Canonical MAC address accepted by `bluetooth:pair(mac)`.
    pub mac: String,
    /// Advertised name, often empty when the device broadcasts only an address.
    pub name: String,
    /// Always `false`; every entry in this pool is unpaired (IDL contract).
    pub paired: bool,
    /// BlueZ refuses to pair with or connect to the device until it is unblocked.
    pub blocked: bool,
    /// `"pairing"`, `"connecting"` or `"disconnecting"` while this Supervisor's call for the device
    /// runs, or `nil`. A device can change lists mid-action, so any list can carry any label. BlueZ
    /// has no property for a call in flight, so a pair or connect started by another client or by
    /// the device itself never shows here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<String>,
}

/// What the pairing agent is asking the user, drawn by `modules/global/bluetooth_pairing.lua`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PairingRequest {
    /// `"confirm"`: does the device show `code`? `"authorize"`: a device asks to pair.
    /// `"service"`: a paired but untrusted device asks to connect. `"display"`: type `code` on the
    /// device, with nothing to answer.
    pub kind: String,
    /// The device's MAC address.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Six-digit passkey or legacy PIN for `"confirm"` and `"display"`, else `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct BluetoothState {
    /// An adapter is bound. `false` means no adapter or no `bluetoothd`, so every other field is
    /// inert and every write is a logged no-op.
    pub available: bool,
    /// Whether the adapter is powered, so always `false` without one.
    pub enabled: bool,
    /// Whether discovery is running, which fills [`BluetoothState::discovered_devices`].
    pub discovering: bool,
    /// Other devices can find this adapter and ask to pair; the agent asks the user before any of
    /// them does. BlueZ turns it off after `DiscoverableTimeout` (180s by default), and that change
    /// reaches this field like any other.
    pub discoverable: bool,
    /// Paired, connected devices. Unordered: the registry is a `HashMap`, so the order can change
    /// on any rebuild. Sort before drawing.
    pub connected_devices: Vec<ConnectedDevice>,
    /// Paired devices that are not connected, unordered like `connected_devices`.
    pub paired_devices: Vec<PairedDevice>,
    /// Unpaired devices seen by the running scan; empties when discovery stops.
    pub discovered_devices: Vec<DiscoveredDevice>,
    /// The pairing question on screen, or `nil`. Answer with `bluetooth:answer_pairing(accept)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_request: Option<PairingRequest>,
}

/// What signal forwarders report to `main.rs`'s top-level `select!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothSignal {
    /// The adapter's own `Powered`, `Discovering` or `Discoverable` property changed.
    AdapterChanged,
    /// A device was added/removed, or its `Connected`/`Paired`/`Name`/`Blocked`/
    /// `Battery1.Percentage` changed.
    DeviceRegistryChanged,
    /// Sent by [`BluetoothController::clear_discovered`], not a forwarder, when
    /// `bluetooth:start_discovery()` begins. It clears `discovered_devices` before
    /// `StartDiscovery` returns (ADR-0030); a distinct variant prevents registry re-derivation
    /// from immediately undoing the clear.
    DiscoveryCleared,
    /// The agent put up or took down a pairing request.
    PairingChanged,
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

/// Actions accepted by `obelisk.bluetooth:invoke(...)`; exhaustive dispatch keeps variants and arms
/// in sync.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BluetoothAction {
    SetEnabled,
    SetDiscoverable,
    StartDiscovery,
    StopDiscovery,
    Pair,
    Connect,
    Disconnect,
    Forget,
    AnswerPairing,
}

/// `obelisk.bluetooth` action dispatch (ADR-0037): matches, parses, and `tokio::spawn`s each write
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
        BluetoothAction::SetDiscoverable => match parse_bool_arg(&params.arguments) {
            Some(on) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_discoverable(on).await;
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
        // Not spawned: it only answers a waiting agent call, and a late answer could land on the
        // next prompt.
        BluetoothAction::AnswerPairing => match parse_bool_arg(&params.arguments) {
            Some(accept) => controller.answer_pairing(accept),
            None => crate::log_malformed_command(params),
        },
        BluetoothAction::StopDiscovery => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.stop_discovery().await;
            });
        }
        // The four device actions differ only in the method they call. The outer match stays
        // exhaustive over `BluetoothAction`, so a new variant is still a compile error here.
        BluetoothAction::Pair | BluetoothAction::Connect | BluetoothAction::Disconnect | BluetoothAction::Forget => {
            match parse_mac_arg(&params.arguments) {
                Some(mac) => {
                    let controller = controller.clone();
                    tokio::spawn(async move {
                        match action {
                            BluetoothAction::Pair => controller.pair(&mac).await,
                            BluetoothAction::Connect => controller.connect(&mac).await,
                            BluetoothAction::Disconnect => controller.disconnect(&mac).await,
                            BluetoothAction::Forget => controller.forget(&mac).await,
                            other => unreachable!("the arm above admits four actions, not {other:?}"),
                        }
                    });
                }
                None => crate::log_malformed_command(params),
            }
        }
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
