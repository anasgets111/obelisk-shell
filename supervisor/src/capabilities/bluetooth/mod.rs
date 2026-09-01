//! BlueZ Bluetooth D-Bus controller (`oblisk.bluetooth`, build-steps.md; docs/oblisk-supervisor-
//! services-dbus.md §5; docs/oblisk-idl-api-specs.md §2.6; ADR-0030).
//!
//! Every proxy here is hand-written against BlueZ's own D-Bus API docs (ADR-0030's "no
//! maintained zbus proxy crate" decision) -- `org.freedesktop.DBus.ObjectManager` is the one
//! exception, reusing `zbus::fdo::ObjectManagerProxy` rather than hand-rolling a duplicate.
//!
//! ponytail: [`BluetoothController::new`] never fails outright, unlike `NetworkController::new`
//! (`zbus::Result<Self>`, propagated with `?` at the call site). NetworkManager is assumed
//! present on every target system (this codebase already depends on it for `oblisk.network`);
//! BlueZ is not -- a machine with no Bluetooth hardware, or one where `bluetoothd` simply isn't
//! running, is a real, unremarkable case that must not take the whole Supervisor down. Every
//! step that can fail (binding the `ObjectManager`, finding an adapter, registering the pairing
//! agent) is logged and degrades to an inert controller instead: `enabled`/`discovering` read as
//! `false`, the device lists stay empty, and every write action logs-and-no-ops. This extends the
//! same "graceful hardware absence" precedent `NetworkController::has_wifi_device` already
//! established for a single missing device, one level further, since here the whole service can
//! be absent.
//!
//! ponytail: `discovered_devices` is clarified once and pushed to a fresh Candidate the same way
//! every other capability's `last_snapshots` entry is -- but between an explicit `start_discovery`
//! clear and the next real event, [`BluetoothSignal::DeviceRegistryChanged`] fully re-derives both
//! device lists from the *entire* tracked registry (mirrors `NetworkController::
//! build_available_networks`'s own "no debounce, no incremental patching" discipline), not just
//! devices newly seen in this discovery session. In practice this means a device this Supervisor
//! already knew about from a previous session can reappear in `discovered_devices` after the
//! list was just cleared, as soon as *any* other device's registry entry changes -- BlueZ's own
//! `Device1` objects persist across discovery sessions and this controller doesn't track `RSSI`
//! to notice "back in range" on its own. Accepted as the simplest faithful reading of ADR-0030's
//! own deferral of these exact mechanics ("an implementation-pass detail, not designed here",
//! mirroring `NetworkState`'s own doc comment) -- a stricter session-scoped list is the upgrade
//! path if this proves visibly wrong on real hardware.

use serde::Serialize;

/// Object path this Supervisor's `org.bluez.Agent1` is exported at on its own unique
/// connection name.
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
    /// The canonical MAC address, e.g. `"00:1A:7D:DA:71:11"`. What every `bluetooth:` command takes
    /// to name a device.
    pub mac: String,
    /// The device's advertised name.
    pub name: String,
    /// `-1` if unsupported/unknown (no `Battery1` interface on this device, or its `Percentage`
    /// property failed to read) -- per the IDL comment, not a sentinel invented here.
    pub battery: i32,
    /// Always `None` this round -- codec query/control is deferred (ADR-0030): it needs a live
    /// PipeWire `Device` proxy, an `audio`-capability concern, not `bluetooth`'s.
    pub codec: Option<String>,
    /// A drawing hint from the device's class of device: `"keyboard"`, `"mouse"`, `"headphones"`,
    /// `"headset"`, `"phone"`, `"computer"`, or `"generic"` for anything the class bits do not
    /// place. Pick an icon from it; do not treat it as a capability.
    pub category: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct DiscoveredDevice {
    /// The canonical MAC address. What `bluetooth:pair(mac)` takes.
    pub mac: String,
    /// The advertised name. Often empty for a device that broadcasts only an address.
    pub name: String,
    /// Always `false` -- per the IDL comment, every entry in this pool is by definition
    /// unpaired.
    pub paired: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct BluetoothState {
    /// The adapter is powered. `false` also when there is no adapter at all, so this is not proof
    /// the machine has Bluetooth hardware.
    pub enabled: bool,
    /// A discovery scan is running, which is what fills
    /// [`BluetoothState::discovered_devices`].
    pub discovering: bool,
    /// Paired devices currently connected. In BlueZ's own object order, which is not sorted.
    pub connected_devices: Vec<ConnectedDevice>,
    /// Unpaired devices seen by the running scan. Empties when discovery stops.
    pub discovered_devices: Vec<DiscoveredDevice>,
}

/// What the signal-forwarder tasks (see [`BluetoothController::new`]) report back to
/// `main.rs`'s own top-level `select!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothSignal {
    /// The adapter's own `Powered` or `Discovering` property changed.
    AdapterChanged,
    /// A device was added to or removed from the registry, or one of a tracked device's own
    /// `Connected`/`Paired`/`Name`/`Battery1.Percentage` properties changed.
    DeviceRegistryChanged,
    /// Sent by [`BluetoothController::clear_discovered`], not a forwarder: a
    /// `bluetooth:start_discovery()` was just dispatched and `discovered_devices` must clear
    /// immediately, before `StartDiscovery`'s D-Bus round trip completes (ADR-0030). A
    /// distinct variant, not [`DeviceRegistryChanged`](Self::DeviceRegistryChanged): a full
    /// registry re-derivation here would instantly undo the clear it exists to perform.
    DiscoveryCleared,
}

/// Failure modes a `bluetooth:*` write action can hit before ever reaching BlueZ itself.
/// Logged via `Display` at the call site, not user-facing.
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

/// Bits 8-12 (Major Device Class) and 2-7 (Minor Device Class) of a BlueZ `Class` property
/// (ADR-0030's exact bit layout). Parses `Class` ourselves rather than trusting BlueZ's own
/// `Icon` property, which comes back empty whenever `Class == 0` (common for BLE peripherals
/// before GAP data is read).
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

/// `bluetooth:set_enabled(en)`'s `arguments: [en]`. Defined once in `dbus` (shared with
/// `network::parse_bool_arg`) and re-exported here so `bluetooth::parse_bool_arg` keeps working
/// unchanged at every call site.
pub use crate::capabilities::parse_bool_arg;

/// `bluetooth:pair(mac)` / `connect(mac)` / `disconnect(mac)` / `forget(mac)`'s `arguments: [mac]`.
pub fn parse_mac_arg(arguments: &[serde_json::Value]) -> Option<String> {
    Some(arguments.first()?.as_str()?.to_string())
}

/// Every action `oblisk.bluetooth:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
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

/// `oblisk.bluetooth`'s action dispatch (ADR-0037): owns the action match, argument parse, and
/// write-action spawn for every `bluetooth` `CommandEnvelope`. Write actions are
/// `tokio::spawn`ed rather than awaited inline (ADR-0030). `stop_discovery` mutates no local
/// state on purpose: the last `discovered_devices` snapshot stays visible.
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
        // Major 0x05, minor top-2-bits `00` (uncategorized device per the Bluetooth spec).
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
        // Major 0x03 is "LAN/Network Access Point" -- not one of the categories this Supervisor
        // renders an icon for.
        assert_eq!(class_to_category(0x03 << 8), "generic");
    }

    #[test]
    fn class_to_category_ignores_service_class_bits() {
        // A real Bluetooth headset's Class value carries Service Class bits above bit 12 too
        // (e.g. "Audio" = bit 21) -- these must not perturb the Major/Minor extraction.
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
