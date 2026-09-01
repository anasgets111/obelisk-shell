//! [`BatteryController`]: the `oblisk.battery` state owner. Read-only telemetry (§ 2.2) --
//! no write actions. Split from `battery` -- see `battery/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use udev::MonitorSocket;

use super::super::read_attr;

/// `oblisk.battery`'s full payload (§ 2.2). Field names are the `StateSnapshot` JSON keys
/// verbatim -- may not be renamed. `Default` (`false`, `0`, `false`) is itself the correct
/// "no battery hardware" answer for a desktop, not a placeholder needing a sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, schemars::JsonSchema)]
pub struct BatteryState {
    pub present: bool,
    pub percent: u8,
    pub charging: bool,
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatterySignal {
    Changed,
}

/// § 2.2's battery-selection predicate: `type` must be exactly `Battery` (excludes `Mains`
/// adapters and USB-PD sources), and `scope` must not be `Device` (a peripheral's battery,
/// not the system's). An absent `scope` file means system scope.
fn is_system_battery(entry_dir: &Path) -> bool {
    if read_attr(entry_dir, "type").as_deref() != Some("Battery") {
        return false;
    }
    read_attr(entry_dir, "scope").as_deref() != Some("Device")
}

/// Picks the one entry [`is_system_battery`] qualifies, in sorted-by-name order (not
/// `read_dir`'s unspecified order) for a deterministic choice across boots on a two-battery
/// laptop. `None` if nothing qualifies.
fn select_system_battery(power_supply_root: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(power_supply_root).ok()?.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    entries.into_iter().find(|entry| is_system_battery(entry))
}

/// `battery.charging`: true exactly when `status` is `Charging` or `Full`. An exact match,
/// not a substring test -- `Not charging` (a real fourth status value) contains "charging"
/// as a substring and would wrongly match.
fn charging_from_status(status: &str) -> bool {
    status == "Charging" || status == "Full"
}

/// `battery.percent`: `capacity` clamped to `[0, 100]` -- some firmware reports over 100.
/// Missing/unparseable `capacity` reads as `0` rather than panicking.
fn percent_from_capacity(capacity: Option<&str>) -> u8 {
    capacity.and_then(|text| text.parse::<u32>().ok()).unwrap_or(0).min(100) as u8
}

/// Reads the whole `oblisk.battery` state in one pass: [`select_system_battery`] first, then
/// `capacity`/`status` off the winner. No qualifying entry is `BatteryState::default()`, the
/// correct answer, not an error.
pub fn read_battery_state(power_supply_root: &Path) -> BatteryState {
    let Some(entry) = select_system_battery(power_supply_root) else {
        return BatteryState::default();
    };
    let percent = percent_from_capacity(read_attr(&entry, "capacity").as_deref());
    let charging = read_attr(&entry, "status").is_some_and(|status| charging_from_status(&status));
    BatteryState { present: true, percent, charging }
}

/// The floor the state is re-read at, whatever the udev watch does. The watch stays primary --
/// `charging` flips within a frame of a plug or unplug, which a 30s timer cannot promise -- but it
/// is a floor and not a fallback, and that distinction is the whole of docs/adr/0080.
///
/// This used to run only when [`build_power_supply_watch`] failed to stand up, on the assumption
/// that a working watch reports every change. It does not. Measured on this dev machine while
/// discharging, with `udevadm monitor --udev --subsystem-match=power_supply` running alongside a
/// one-second sampler: `capacity` fell 69 to 65 and the socket delivered **zero** events. UPower,
/// watching the same battery, tracked every point, because UPower polls the hardware itself
/// (`upower -i` reported "updated: 13 seconds ago"). The kernel's ACPI battery driver emits a
/// uevent on a plug or unplug and, on this hardware, on nothing else.
///
/// 30s is UPower's own cadence for a battery that needs polling, which is the number to match: a
/// bar showing a percentage two points stale is what a user reports, and no config can ask for
/// tighter than the source updates. If it ever needs to be user-tunable,
/// `SysinfoController`'s configurable watch-channel interval (docs/adr/0035) is the pattern.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Not `Clone`: § 2.2 has no write action, so nothing needs a second handle.
pub struct BatteryController {
    state: Arc<Mutex<BatteryState>>,
}

impl BatteryController {
    /// `power_supply_root` (real default `/sys/class/power_supply`) is injected for
    /// testability. Returns immediately; [`run_battery_task`] does the real reading in a
    /// spawned task.
    pub fn new(power_supply_root: PathBuf, events: UnboundedSender<BatterySignal>) -> Self {
        let state = Arc::new(Mutex::new(BatteryState::default()));
        tokio::spawn(run_battery_task(power_supply_root, Arc::clone(&state), events));
        Self { state }
    }

    pub fn snapshot(&self) -> BatteryState {
        *self.state.lock().expect("battery state mutex poisoned")
    }
}

/// Reads the initial state, sends it, then hands off to [`run_battery_loop`]. A udev watch that
/// fails to stand up is not fatal: the loop runs on its timer alone, which is the same cadence a
/// silent watch already leaves it on.
async fn run_battery_task(
    power_supply_root: PathBuf,
    state: Arc<Mutex<BatteryState>>,
    events: UnboundedSender<BatterySignal>,
) {
    let initial = read_battery_state(&power_supply_root);
    *state.lock().expect("battery state mutex poisoned") = initial;
    if events.send(BatterySignal::Changed).is_err() {
        return;
    }

    let watch = build_power_supply_watch()
        .map_err(|err| {
            eprintln!(
                "battery: failed to set up the udev power_supply watch ({err}); a plug or unplug will show up on the {POLL_INTERVAL:?} read instead of instantly"
            );
        })
        .ok();
    run_battery_loop(watch, power_supply_root, initial, state, events).await;
}

/// Builds the `power_supply` subsystem udev watch (§ 2.2; docs/build-steps.md line 98). Needs
/// `udev`'s `send` feature (Cargo.toml) to typecheck -- `MonitorBuilder`/`MonitorSocket` must
/// be `Send` to live inside the `tokio::spawn`ed future.
fn build_power_supply_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("power_supply")?.listen()?;
    AsyncFd::new(socket)
}

/// One loop, two wakeup sources, one read. A readable udev fd or a [`POLL_INTERVAL`] tick both
/// mean the same thing here -- go look again -- and only the comparison below decides whether
/// anything is pushed. udev fires on every `power_supply` change, not just the ones this
/// capability reports, so most wakeups get filtered out either way.
///
/// `watch` is `None` when the socket could not be built, and becomes `None` when its fd errors:
/// the loop keeps running on the timer rather than returning, because a broken netlink socket must
/// not leave a working battery unreported.
///
/// Drains the pending messages on each readiness, since one plug can fire more than one and
/// level-triggered readiness would re-fire on anything left undrained.
///
/// Uses `readable_mut` (not `readable`): only `udev`'s `send` feature is enabled (Cargo.toml),
/// not `sync`, and `readable`'s guard needs `MonitorSocket: Sync` to be `Send` across `.await` --
/// `readable_mut`'s guard only needs `MonitorSocket: Send`, which is already enabled.
async fn run_battery_loop(
    mut watch: Option<AsyncFd<MonitorSocket>>,
    power_supply_root: PathBuf,
    mut previous: BatteryState,
    state: Arc<Mutex<BatteryState>>,
    events: UnboundedSender<BatterySignal>,
) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.tick().await; // tokio::time::interval's first tick fires immediately; the caller's initial read already covers it

    loop {
        let watch_errored = tokio::select! {
            // The `if` guard is what makes the `expect` sound: `select!` evaluates a branch's
            // precondition before it ever polls that branch's future, so this arm cannot run
            // while the socket is gone.
            readiness = async { watch.as_mut().expect("guarded by watch.is_some()").readable_mut().await },
                if watch.is_some() =>
            {
                match readiness {
                    Ok(mut guard) => {
                        for _event in guard.get_inner().iter() {}
                        guard.clear_ready();
                        false
                    }
                    Err(err) => {
                        eprintln!(
                            "battery: the udev power_supply watch's fd errored ({err}); reading on the {POLL_INTERVAL:?} timer alone for the rest of this run"
                        );
                        true
                    }
                }
            }
            _ = ticker.tick() => false,
        };
        if watch_errored {
            watch = None;
        }

        let current = read_battery_state(&power_supply_root);
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

    fn write_entry(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (attr, value) in attrs {
            std::fs::write(dir.join(attr), value).unwrap();
        }
    }

    // ---- run_battery_loop ----

    /// The bug docs/adr/0080 was written for. A udev watch that never fires is indistinguishable
    /// from no watch at all, so this drives the loop with `None` and lets the timer be the only
    /// wakeup. Before the timer was a floor rather than a fallback, this hung: the watch loop
    /// awaited an fd that had nothing to say and the poll loop was only reachable when the socket
    /// failed to build.
    #[tokio::test(start_paused = true)]
    async fn a_capacity_change_with_no_udev_event_still_reaches_the_signal() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "69"), ("status", "Discharging")]);

        let initial = read_battery_state(root.path());
        assert_eq!(initial, BatteryState { present: true, percent: 69, charging: false });

        let state = Arc::new(Mutex::new(initial));
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task =
            tokio::spawn(run_battery_loop(None, root.path().to_path_buf(), initial, Arc::clone(&state), events));

        // The four points this dev machine lost between two udev events that never arrived.
        std::fs::write(root.path().join("BAT0").join("capacity"), "65").unwrap();

        assert_eq!(received.recv().await, Some(BatterySignal::Changed));
        assert_eq!(state.lock().unwrap().percent, 65);
        task.abort();
    }

    /// The filter survives the change: a tick that finds nothing new pushes nothing, or every
    /// `POLL_INTERVAL` would wake the renderer for a battery that has not moved.
    #[tokio::test(start_paused = true)]
    async fn a_tick_that_finds_nothing_new_pushes_nothing() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "69"), ("status", "Discharging")]);

        let initial = read_battery_state(root.path());
        let state = Arc::new(Mutex::new(initial));
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task =
            tokio::spawn(run_battery_loop(None, root.path().to_path_buf(), initial, Arc::clone(&state), events));

        // Long enough for many ticks under paused time, all of them reading the same files.
        let quiet = tokio::time::timeout(POLL_INTERVAL * 10, received.recv()).await;
        assert!(quiet.is_err(), "an unchanged battery pushed {quiet:?}");
        task.abort();
    }

    // ---- is_system_battery ----

    #[test]
    fn is_system_battery_excludes_mains_and_usb_types() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "AC0", &[("type", "Mains")]);
        write_entry(root.path(), "ucsi-source-psy-USBC000:001", &[("type", "USB"), ("scope", "System")]);

        assert!(!is_system_battery(&root.path().join("AC0")));
        assert!(!is_system_battery(&root.path().join("ucsi-source-psy-USBC000:001")));
    }

    #[test]
    fn is_system_battery_treats_an_absent_scope_file_as_system_scope() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery")]); // real BAT0 on this dev machine has no scope file

        assert!(is_system_battery(&root.path().join("BAT0")));
    }

    #[test]
    fn is_system_battery_excludes_a_device_scoped_peripheral_battery() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "hid-aa-bb-battery", &[("type", "Battery"), ("scope", "Device")]);

        assert!(!is_system_battery(&root.path().join("hid-aa-bb-battery")));
    }

    // ---- charging_from_status ----

    #[test]
    fn charging_from_status_is_true_only_for_charging_and_full() {
        assert!(charging_from_status("Charging"));
        assert!(charging_from_status("Full"));
        assert!(!charging_from_status("Discharging"));
        // "Not charging" contains "charging" as a substring -- must not match via a contains check.
        assert!(!charging_from_status("Not charging"));
    }

    // ---- percent_from_capacity ----

    #[test]
    fn percent_from_capacity_clamps_a_value_over_one_hundred() {
        assert_eq!(percent_from_capacity(Some("105")), 100);
    }

    #[test]
    fn percent_from_capacity_is_zero_for_a_missing_or_malformed_reading() {
        assert_eq!(percent_from_capacity(None), 0);
        assert_eq!(percent_from_capacity(Some("not-a-number")), 0);
    }

    // ---- select_system_battery ----

    #[test]
    fn select_system_battery_picks_bat0_over_bat1_by_sorted_name() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT1", &[("type", "Battery"), ("capacity", "40"), ("status", "Discharging")]);
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "90"), ("status", "Charging")]);

        assert_eq!(select_system_battery(root.path()), Some(root.path().join("BAT0")));
    }

    #[test]
    fn select_system_battery_is_none_against_an_empty_or_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(select_system_battery(root.path()), None);
        assert_eq!(select_system_battery(&root.path().join("does-not-exist")), None);
    }

    // ---- read_battery_state ----

    #[test]
    fn read_battery_state_picks_bat0_over_the_mains_adapter_and_the_usb_pd_source() {
        // Real captured shape from this dev machine's own /sys/class/power_supply/.
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "AC0", &[("type", "Mains")]);
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "59"), ("status", "Not charging")]);
        write_entry(
            root.path(),
            "ucsi-source-psy-USBC000:001",
            &[("type", "USB"), ("scope", "System"), ("status", "Not charging")],
        );

        assert_eq!(read_battery_state(root.path()), BatteryState { present: true, percent: 59, charging: false });
    }

    #[test]
    fn read_battery_state_excludes_a_device_scoped_peripheral_battery() {
        let root = tempfile::tempdir().unwrap();
        write_entry(
            root.path(),
            "hid-aa-bb-battery",
            &[("type", "Battery"), ("scope", "Device"), ("capacity", "80"), ("status", "Discharging")],
        );

        assert_eq!(read_battery_state(root.path()), BatteryState::default());
    }

    #[test]
    fn read_battery_state_reads_charging_true_for_charging_and_full() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "20"), ("status", "Charging")]);
        assert!(read_battery_state(root.path()).charging);

        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "100"), ("status", "Full")]);
        assert!(read_battery_state(root.path()).charging);
    }

    #[test]
    fn read_battery_state_reads_charging_false_for_discharging_and_not_charging() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "80"), ("status", "Discharging")]);
        assert!(!read_battery_state(root.path()).charging);

        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "59"), ("status", "Not charging")]);
        assert!(!read_battery_state(root.path()).charging);
    }

    #[test]
    fn read_battery_state_is_the_default_sentinel_for_an_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_battery_state(root.path()), BatteryState { present: false, percent: 0, charging: false });
    }

    #[test]
    fn read_battery_state_is_the_default_sentinel_against_a_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_battery_state(&root.path().join("does-not-exist")), BatteryState::default());
    }

    #[test]
    fn read_battery_state_clamps_an_over_range_capacity_reading() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "105"), ("status", "Full")]);

        assert_eq!(read_battery_state(root.path()), BatteryState { present: true, percent: 100, charging: true });
    }

    #[test]
    fn read_battery_state_picks_bat0_over_bat1_deterministically() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT1", &[("type", "Battery"), ("capacity", "40"), ("status", "Discharging")]);
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "90"), ("status", "Charging")]);

        assert_eq!(read_battery_state(root.path()), BatteryState { present: true, percent: 90, charging: true });
    }

    // ---- BatteryController (construction/task wiring) ----

    #[tokio::test]
    async fn battery_controller_pushes_the_initial_state_before_the_first_poll_tick() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "BAT0", &[("type", "Battery"), ("capacity", "59"), ("status", "Not charging")]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BatteryController::new(root.path().to_path_buf(), events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(
            signal,
            Ok(Some(BatterySignal::Changed)),
            "must announce the initial state without waiting for the first poll tick"
        );
        assert_eq!(controller.snapshot(), BatteryState { present: true, percent: 59, charging: false });
    }

    #[tokio::test]
    async fn battery_controller_still_pushes_one_signal_when_no_battery_hardware_exists() {
        let root = tempfile::tempdir().unwrap(); // empty -- the desktop case
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BatteryController::new(root.path().to_path_buf(), events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(Some(BatterySignal::Changed)));
        assert_eq!(controller.snapshot(), BatteryState::default());
    }
}
