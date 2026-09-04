//! [`BatteryController`]: the `oblisk.battery` state owner. Read-only telemetry (§ 2.2) --
//! no write actions. Split from `battery` -- see `battery/mod.rs` for the module-level doc.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

/// § 2.2's `battery.state`, one of UPower's seven `Device.State` values.
///
/// A boolean cannot carry this, and that is why it is not one. The four states a laptop with a
/// charge threshold moves between are `Charging`, `PendingCharge` (the limit is reached and the
/// mains adapter is holding the battery there), `PendingDischarge` (the battery is above the
/// limit and draining down to it, still on mains) and `Discharging` (on battery). Under the
/// `charging: bool` this replaced, the middle two both read `false`, so a config could not tell
/// "the limit is reached" from "you are on battery" -- which on this dev machine, whose
/// `charge_control_end_threshold` is 70, is most of every day.
///
/// Serialized by name, so Lua compares `b.state == "PendingCharge"`. The same shape
/// `mpris`'s `play_state` already uses at this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
pub enum BatteryStatus {
    /// UPower has no answer, which includes every host where the display device is not a battery.
    #[default]
    Unknown,
    /// Taking current from the mains adapter.
    Charging,
    /// Running off the battery, with no mains adapter supplying it.
    Discharging,
    /// Flat, which UPower reports in place of `Discharging` only at the very end.
    Empty,
    /// At the top of the battery, on mains, holding. A charge limit gives `PendingCharge` instead.
    FullyCharged,
    /// On mains, at the charge limit, not taking current. "Charge limit reached".
    PendingCharge,
    /// On mains, above the charge limit, draining down to it. The cable is in and the level falls.
    PendingDischarge,
}

impl BatteryStatus {
    /// UPower's own numbering (`org.freedesktop.UPower.Device.State`). An unknown number is
    /// [`BatteryStatus::Unknown`] rather than an error: a future UPower adding an eighth state
    /// must not fail this capability.
    fn from_upower(state: u32) -> Self {
        match state {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }
}

/// `oblisk.battery`'s full payload (§ 2.2). Field names are the `StateSnapshot` JSON keys
/// verbatim -- may not be renamed. `Default` is itself the correct "no battery hardware" answer
/// for a desktop, not a placeholder needing a sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
pub struct BatteryState {
    /// UPower's display device is a battery and reports itself present. `false` on a desktop, which
    /// is an answer rather than a missing one: check it before drawing anything else here.
    pub present: bool,
    /// Charge, `0` to `100`, rounded. Against the battery's own full capacity, not against a charge
    /// limit, so a machine capped at 70 reads `70` and stays there rather than reading `100`.
    pub percent: u8,
    /// What the battery is doing, by UPower's own name. The field that separates holding a charge
    /// limit on mains (`PendingCharge`) from actually running down (`Discharging`), which a
    /// boolean could not.
    pub state: BatteryStatus,
    /// Seconds until flat, or `nil`. UPower reports `0` both while charging and while it has not
    /// yet estimated, and neither is a duration, so both are the absent case here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_empty: Option<u32>,
    /// Seconds until full, or `nil`, on the same terms as `time_to_empty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_full: Option<u32>,
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatterySignal {
    Changed,
}

/// UPower's `DisplayDevice`, the composite it sums every battery on the machine into. The same
/// object `power::controller` reads `EnergyRate` off, and the one Quickshell's
/// `services/upower/core.cpp` binds through `GetDisplayDevice()`.
///
/// The path is well-known and fixed, so this proxies it directly rather than calling
/// `GetDisplayDevice()` for an address that is documented to be exactly this one.
#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait DisplayDevice {
    /// `2` is Battery. On a desktop the display device exists but is not one.
    #[zbus(property, name = "Type")]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;
    /// `[0, 100]`, not a fraction.
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn time_to_empty(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn time_to_full(&self) -> zbus::Result<i64>;
}

/// UPower's `Type` value for a battery.
const UPOWER_TYPE_BATTERY: u32 = 2;

/// Not `Clone`: § 2.2 has no write action, so nothing needs a second handle.
pub struct BatteryController {
    state: Arc<Mutex<BatteryState>>,
}

impl BatteryController {
    /// Returns immediately; [`run_battery_task`] does the reading in a spawned task.
    pub fn new(system_bus: zbus::Connection, events: UnboundedSender<BatterySignal>) -> Self {
        let state = Arc::new(Mutex::new(BatteryState::default()));
        tokio::spawn(run_battery_task(system_bus, Arc::clone(&state), events));
        Self { state }
    }

    pub fn snapshot(&self) -> BatteryState {
        *self.state.lock().expect("battery state mutex poisoned")
    }
}

/// A duration UPower has actually estimated, or `None`. `0` is its "no answer" value on both
/// properties, and a negative one is not a duration at all.
fn seconds(reported: i64) -> Option<u32> {
    u32::try_from(reported).ok().filter(|seconds| *seconds > 0)
}

/// Reads the whole payload off one device. A property read that fails leaves its field at the
/// default rather than keeping the last value, the same rule `power::controller::read_state`
/// follows and for the same reason: a stale number that looks live is worse than an honest zero.
///
/// `present` needs both halves of the answer. `IsPresent` alone is true on hardware that is not a
/// battery at all, so the type is checked with it -- the same pair Quickshell's `isLaptopBattery`
/// tests before it believes a percentage.
async fn read_state(device: &DisplayDeviceProxy<'static>) -> BatteryState {
    let is_battery = device.device_type().await.is_ok_and(|kind| kind == UPOWER_TYPE_BATTERY);
    let present = is_battery && device.is_present().await.unwrap_or(false);
    if !present {
        return BatteryState::default();
    }

    BatteryState {
        present: true,
        // `round`, not a cast: a cast truncates, so 69.8% would show as 69 for the whole minute
        // before it reached 70.
        percent: device.percentage().await.unwrap_or(0.0).clamp(0.0, 100.0).round() as u8,
        state: device.state().await.map(BatteryStatus::from_upower).unwrap_or_default(),
        time_to_empty: device.time_to_empty().await.ok().and_then(seconds),
        time_to_full: device.time_to_full().await.ok().and_then(seconds),
    }
}

/// Reads once, pushes, then follows the device's `PropertiesChanged`. Every wake re-reads the
/// whole payload rather than patching the one property that fired, which is what
/// `power::controller` already does and what keeps the five fields consistent with each other.
///
/// One subscription for the whole object, not one per property: `org.freedesktop.DBus.Properties`
/// batches a device's changes into a single signal, and a percentage that moves while the state
/// flips arrives as one message. This is Quickshell's `DBusPropertyGroup` shape.
///
/// **No timer, and that is the point of ADR-0080.** The sysfs reader this replaced could not
/// see a change the kernel did not announce, and measured on this machine the kernel announced a
/// plug and nothing else: `capacity` fell 69 to 65 with zero `power_supply` uevents delivered.
/// UPower polls the hardware itself and emits on every refresh, so the polling moves to the one
/// process already doing it for every other client on the system.
/// UPower occasionally reports a spurious `Percentage` of 0 while the machine is on mains, one
/// push long, and the pill emptied for it. The reference config holds the last reading in that
/// case (`BatteryService.qml`'s `_ingestPercentage`), and so does this: a zero that arrives with the
/// battery not draining, right after a non-zero reading, keeps the previous percent. On battery a
/// genuine zero is indistinguishable from the glitch, so it passes through there, as it does there.
fn hold_through_glitch(previous: BatteryState, current: BatteryState) -> BatteryState {
    let draining = matches!(current.state, BatteryStatus::Discharging | BatteryStatus::Empty);
    if current.present && !draining && current.percent == 0 && previous.percent > 0 {
        BatteryState { percent: previous.percent, ..current }
    } else {
        current
    }
}

async fn run_battery_task(
    system_bus: zbus::Connection,
    state: Arc<Mutex<BatteryState>>,
    events: UnboundedSender<BatterySignal>,
) {
    let device = match DisplayDeviceProxy::new(&system_bus).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("battery: no UPower DisplayDevice reachable ({err}); battery will not be reported this run");
            return;
        }
    };

    // Subscribed before the first read, for `power::controller`'s reason: each subscription is
    // its own round trip, and a cable pulled during that window would land between a read and a
    // subscription that does not exist yet.
    let properties = match zbus::fdo::PropertiesProxy::builder(&system_bus)
        .destination("org.freedesktop.UPower")
        .and_then(|builder| builder.path("/org/freedesktop/UPower/devices/DisplayDevice"))
    {
        Ok(builder) => match builder.build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("battery: cannot watch the UPower DisplayDevice for changes ({err}); giving up on it");
                return;
            }
        },
        Err(err) => {
            eprintln!("battery: cannot address the UPower DisplayDevice ({err}); giving up on it");
            return;
        }
    };
    let Ok(mut changed) = properties.receive_properties_changed().await else {
        eprintln!("battery: cannot subscribe to the UPower DisplayDevice's properties; giving up on it");
        return;
    };

    let mut previous = read_state(&device).await;
    *state.lock().expect("battery state mutex poisoned") = previous;
    if events.send(BatterySignal::Changed).is_err() {
        return;
    }

    // Ends when UPower goes away, which drops the proxies and their connection references. There
    // is nothing else to wake this task, so parking on a dead stream would leak it.
    while changed.next().await.is_some() {
        let current = hold_through_glitch(previous, read_state(&device).await);
        if current != previous {
            *state.lock().expect("battery state mutex poisoned") = current;
            previous = current;
            if events.send(BatterySignal::Changed).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- BatteryStatus::from_upower ----

    /// The whole reason § 2.2 has a `state` and not a `charging`. Each of these is a distinct
    /// thing to show a user, and the three middle rows all collapsed to `false` before.
    #[test]
    fn every_upower_state_maps_to_its_own_name() {
        for (reported, expected) in [
            (0, BatteryStatus::Unknown),
            (1, BatteryStatus::Charging),
            (2, BatteryStatus::Discharging),
            (3, BatteryStatus::Empty),
            (4, BatteryStatus::FullyCharged),
            (5, BatteryStatus::PendingCharge),
            (6, BatteryStatus::PendingDischarge),
        ] {
            assert_eq!(BatteryStatus::from_upower(reported), expected, "State = {reported}");
        }
    }

    /// An eighth state in some future UPower must degrade, not fail: this capability reports what
    /// it understands and a config renders `"Unknown"` rather than the whole payload going away.
    #[test]
    fn a_state_number_this_build_does_not_know_reads_as_unknown() {
        assert_eq!(BatteryStatus::from_upower(7), BatteryStatus::Unknown);
        assert_eq!(BatteryStatus::from_upower(u32::MAX), BatteryStatus::Unknown);
    }

    /// The names are the wire format: a config compares against these strings, so a rename here
    /// is a breaking change to § 2.2 and has to look like one.
    #[test]
    fn the_state_serializes_under_the_name_a_config_compares_against() {
        let json = serde_json::to_string(&BatteryState {
            present: true,
            percent: 70,
            state: BatteryStatus::PendingCharge,
            time_to_empty: None,
            time_to_full: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"present":true,"percent":70,"state":"PendingCharge"}"#);
    }

    // ---- hold_through_glitch ----

    #[test]
    fn a_zero_on_mains_right_after_a_reading_keeps_the_reading() {
        let previous =
            BatteryState { present: true, percent: 70, state: BatteryStatus::PendingCharge, ..Default::default() };
        let glitch = BatteryState { percent: 0, ..previous };
        assert_eq!(hold_through_glitch(previous, glitch).percent, 70);
        let drained = BatteryState { percent: 0, state: BatteryStatus::Discharging, ..previous };
        assert_eq!(hold_through_glitch(previous, drained).percent, 0, "on battery a zero is a zero");
        assert!(
            !hold_through_glitch(previous, BatteryState::default()).present,
            "a battery going away is not a glitch"
        );
    }

    // ---- seconds ----

    /// UPower reports `0` on `TimeToEmpty` the entire time a battery is charging, and on both
    /// properties before it has enough history to estimate. Neither is a duration.
    #[test]
    fn an_unestimated_or_negative_duration_is_absent_rather_than_zero() {
        assert_eq!(seconds(0), None);
        assert_eq!(seconds(-1), None);
        assert_eq!(seconds(8040), Some(8040));
    }

    /// A desktop's display device exists and is not a battery, which is `present = false` and the
    /// default payload -- not an error and not an absent capability.
    #[test]
    fn the_default_payload_is_the_no_battery_answer() {
        assert_eq!(
            BatteryState::default(),
            BatteryState {
                present: false,
                percent: 0,
                state: BatteryStatus::Unknown,
                time_to_empty: None,
                time_to_full: None
            }
        );
    }
}
