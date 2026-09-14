//! [`BatteryController`] owns read-only `obelisk.battery` telemetry. Module-level behavior is
//! documented in `battery/mod.rs`.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

/// `battery.state`, one of UPower's seven `Device.State` values.
///
/// A boolean collapsed `PendingCharge` and `PendingDischarge` into `false`, making a battery that
/// is merely not moving indistinguishable from one that is draining. On a laptop that sets
/// `charge_control_end_threshold` -- 70 here -- the not-moving case is most of every day, which is
/// why the state is carried by name. The three seen on this hardware are `Charging`,
/// `PendingCharge` and `Discharging`.
///
/// UPower documents these seven only as names: its `Device` page lists the enum and defines no
/// value. So each doc below says what the kernel and this hardware were observed to do, and none
/// of them is a guarantee from UPower.
///
/// Serialized by name, so Lua compares `b.state == "PendingCharge"`; `mpris.play_state` uses the
/// same boundary shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub enum BatteryStatus {
    /// UPower has no answer, including hosts whose display device is not a battery.
    #[default]
    Unknown,
    /// Taking current; on this hardware that means an adapter is supplying it.
    Charging,
    /// Draining.
    Discharging,
    /// Flat; UPower reports this instead of `Discharging` only at the very end.
    Empty,
    /// Charged and holding. A battery stopped below full reports `PendingCharge` instead.
    FullyCharged,
    /// Waiting to charge: not draining, not taking current.
    ///
    /// A reached charge limit is the usual cause on a laptop that sets one, but a weak charger, a
    /// thermal pause, and the second or two after a plug while the driver still reads
    /// `Not charging` all report it too, so nothing downstream may read a limit out of it. The
    /// first line stands alone on purpose: `stubs.rs` gives a variant only that much.
    PendingCharge,
    /// Waiting to discharge, by name; UPower defines it no further.
    ///
    /// Linux battery sysfs has no status that produces it, so it is not expected on this hardware.
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

/// `obelisk.battery`'s full payload. Field names are the `StateSnapshot` JSON keys verbatim
/// and may not be renamed. `Default` is the correct desktop answer when no battery exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct BatteryState {
    /// Whether UPower's display device is a battery and present. `false` on a desktop is an answer,
    /// not missing data; check it before drawing the other fields.
    pub present: bool,
    /// Charge, `0` to `100`, rounded against the battery's full capacity, not its charge limit. A
    /// machine capped at 70 therefore reads `70`, not `100`.
    pub percent: u8,
    /// UPower's state, including `PendingCharge` on mains versus `Discharging` on battery.
    pub state: BatteryStatus,
    /// Seconds until flat, or `nil`. UPower reports `0` while charging and before it has estimated;
    /// neither is a duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_empty: Option<u32>,
    /// Seconds until full, or `nil` on the same terms as `time_to_empty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_full: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatterySignal {
    Changed,
}

/// UPower's `DisplayDevice`, the composite of every battery. `power::controller` reads
/// `EnergyRate` from it, and Quickshell's `services/upower/core.cpp` binds through
/// `GetDisplayDevice()`.
///
/// Its documented path is fixed, so this proxies it directly instead of calling
/// `GetDisplayDevice()`.
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

pub struct BatteryController {
    state: Arc<Mutex<BatteryState>>,
}

impl BatteryController {
    pub fn new(system_bus: zbus::Connection, events: UnboundedSender<BatterySignal>) -> Self {
        let state = Arc::new(Mutex::new(BatteryState::default()));
        tokio::spawn(run_battery_task(system_bus, Arc::clone(&state), events));
        Self { state }
    }

    pub fn snapshot(&self) -> BatteryState {
        *self.state.lock().expect("battery state mutex poisoned")
    }
}

/// A positive duration UPower estimated. `0` means "no answer" on both properties; negatives are
/// not durations.
fn seconds(reported: i64) -> Option<u32> {
    u32::try_from(reported).ok().filter(|seconds| *seconds > 0)
}

/// Reads the whole payload. Failed properties keep their defaults rather than stale values, the
/// same rule as `power::controller::read_state`: a live-looking stale number is worse than zero.
///
/// `IsPresent` alone is true for non-battery display devices, so `Type` and `IsPresent` are checked
/// together, as Quickshell's `isLaptopBattery` does.
async fn read_state(device: &DisplayDeviceProxy<'static>) -> BatteryState {
    let is_battery = device.device_type().await.is_ok_and(|kind| kind == UPOWER_TYPE_BATTERY);
    let present = is_battery && device.is_present().await.unwrap_or(false);
    if !present {
        return BatteryState::default();
    }

    BatteryState {
        present: true,
        // Round rather than cast: 69.8% would otherwise show 69 for the whole minute before 70.
        percent: device.percentage().await.unwrap_or(0.0).clamp(0.0, 100.0).round() as u8,
        state: device.state().await.map(BatteryStatus::from_upower).unwrap_or_default(),
        time_to_empty: device.time_to_empty().await.ok().and_then(seconds),
        time_to_full: device.time_to_full().await.ok().and_then(seconds),
    }
}

/// Reads once, pushes, then follows `PropertiesChanged`. Every wake re-reads all five fields, as
/// `power::controller` does, keeping them consistent instead of patching one property.
///
/// One `org.freedesktop.DBus.Properties` subscription covers the object. It batches a percentage
/// move and state flip into one message, matching Quickshell's `DBusPropertyGroup` shape.
///
/// **No timer, per ADR-0080.** The replaced sysfs reader missed capacity changes the kernel did not
/// announce: on this machine a plug event arrived, then `capacity` fell 69 to 65 with zero
/// `power_supply` uevents. UPower already polls and emits refreshes for other clients.
/// UPower also emitted a spurious mains `Percentage` of 0 for one push, emptying the pill. Match
/// `BatteryService.qml`'s `_ingestPercentage`: after a nonzero reading, retain a mains zero while
/// not draining. A real on-battery zero is indistinguishable from the glitch and passes through.
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
    // Disable zbus's property cache. Its refresh task listens to the same signal, so our stream
    // can win the race and read the pre-change cache, compare equal to `previous`, and leave a
    // newly plugged charger showing `Discharging` until a later property moves. Uncached means
    // five real `Get` round trips per event, only a few times an hour.
    //
    // `power::controller` wakes on zbus's cache-backed `receive_*_changed`, so its cache is current
    // when the stream yields. That is why `power.on_battery` was instant while `battery.charging`
    // lagged by a full change.
    let device =
        match DisplayDeviceProxy::builder(&system_bus).cache_properties(zbus::proxy::CacheProperties::No).build().await
        {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("battery: no UPower DisplayDevice reachable ({err}); battery will not be reported this run");
                return;
            }
        };

    // Subscribe before the first read. A cable change during the subscription round trip must not
    // land between a read and a subscription that does not exist yet.
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

    // UPower going away drops the proxies and ends the task; parking on a dead stream leaks it.
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

    /// Each state is distinct to a user; the old `charging` boolean collapsed the middle rows.
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

    /// An unknown future state degrades to `"Unknown"` instead of dropping the payload.
    #[test]
    fn a_state_number_this_build_does_not_know_reads_as_unknown() {
        assert_eq!(BatteryStatus::from_upower(7), BatteryStatus::Unknown);
        assert_eq!(BatteryStatus::from_upower(u32::MAX), BatteryStatus::Unknown);
    }

    /// These names are the wire format and config comparisons; renaming one is breaking.
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

    /// `0` means charging or not enough history to estimate on these properties, not a duration.
    #[test]
    fn an_unestimated_or_negative_duration_is_absent_rather_than_zero() {
        assert_eq!(seconds(0), None);
        assert_eq!(seconds(-1), None);
        assert_eq!(seconds(8040), Some(8040));
    }

    /// A desktop's non-battery display device yields the default payload, not an error.
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
