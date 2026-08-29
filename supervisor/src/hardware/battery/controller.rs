//! [`BatteryController`]: the `oblisk.battery` state owner. Read-only telemetry (§ 2.2) -- no
//! write actions, the same posture `PrivacyController` already has. Split from `battery` --
//! see `battery/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use udev::MonitorSocket;

/// `oblisk.battery`'s full payload (§ 2.2). Field names are the `StateSnapshot` JSON keys
/// verbatim -- the Renderer routes them straight into the Lua `oblisk.battery` signal table by
/// name, unchanged, so they may not be renamed. `Default` (`false`, `0`, `false`) already is
/// the correct "no battery hardware" answer § 2.2 wants for a desktop, not a fabricated
/// placeholder -- no separate sentinel constant is needed the way `KeyboardState::backlight_pct`
/// needed `-1`, because a bare `bool`/`u8` triple has no missing-hardware case that overlaps a
/// real reading (unlike a backlight percent, `0%` battery and `0%` "not present" are the same
/// externally-observable state to a config, and the IDL's own `present: false` field is what
/// actually distinguishes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct BatteryState {
    pub present: bool,
    pub percent: u8,
    pub charging: bool,
}

/// One shared signal, `Changed` only (mirrors `PrivacySignal`/`KeyboardSignal`/`SysinfoSignal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatterySignal {
    Changed,
}

/// Reads and trims one sysfs attribute file under `entry_dir`. `None` covers both "file
/// missing" (e.g. no `scope` file, § 2.2's own system-scope default) and any other read error --
/// this codebase's sysfs readers don't distinguish "absent" from "unreadable" anywhere else
/// either (e.g. `keyboard::locks::read_led_on`'s own missing-file case).
fn read_attr(entry_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(entry_dir.join(name)).ok().map(|text| text.trim().to_string())
}

/// § 2.2's whole device-selection correctness problem, as one pure predicate: `entry_dir`
/// qualifies as the/a system battery only if its `type` is exactly `Battery` (a `Mains` adapter
/// or a USB power-delivery source, both real siblings on this dev machine's own
/// `/sys/class/power_supply/`, never qualify), and its `scope` is not `Device` -- a `Device`
/// scope marks a peripheral's battery (a wireless mouse, a bluetooth headset), not the system
/// battery. An absent `scope` file (this dev machine's real `BAT0` has none) means system scope,
/// the common case for a laptop's own battery, so it passes.
fn is_system_battery(entry_dir: &Path) -> bool {
    if read_attr(entry_dir, "type").as_deref() != Some("Battery") {
        return false;
    }
    read_attr(entry_dir, "scope").as_deref() != Some("Device")
}

/// Picks the one power-supply entry under `power_supply_root` that [`is_system_battery`]
/// qualifies, in sorted-by-name order (not `read_dir`'s unspecified order) so the choice is
/// deterministic across boots -- relevant on the rare two-battery laptop (`BAT0`/`BAT1`), where
/// readdir order isn't guaranteed to put `BAT0` first. `None` on a missing/unreadable root or
/// when nothing qualifies (the common desktop case, or a directory of only `Mains`/`USB`/
/// `Device`-scoped entries).
fn select_system_battery(power_supply_root: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(power_supply_root).ok()?.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    entries.into_iter().find(|entry| is_system_battery(entry))
}

/// `battery.charging`: true exactly when `status` is `Charging` or `Full` (§ 2.2's own
/// wording). An exact match, not `contains("harging")` or similar: `Not charging` -- a real
/// fourth value this dev machine's own `BAT0` currently reports, distinct from `Discharging` --
/// contains "charging" as a substring, so a substring test would invert this exact case.
fn charging_from_status(status: &str) -> bool {
    status == "Charging" || status == "Full"
}

/// `battery.percent`: `capacity` clamped to `[0, 100]` -- some firmware reports over 100 (not
/// this dev machine's own `BAT0`, which reports a plain in-range `59`, but § 2.2 doesn't
/// guarantee the file stays in range either). A missing or unparseable `capacity` reads as `0`
/// rather than panicking or carrying forward a stale value -- the same "degrade to the honest
/// default, don't fabricate" posture `percent_from_capacity`'s sibling readers already take
/// throughout this codebase.
fn percent_from_capacity(capacity: Option<&str>) -> u8 {
    capacity.and_then(|text| text.parse::<u32>().ok()).unwrap_or(0).min(100) as u8
}

/// Reads the whole `oblisk.battery` state from a real (or fixture) `/sys/class/power_supply/`
/// tree in one pass: [`select_system_battery`] first, then `capacity`/`status` off the winner.
/// No qualifying entry -- the common desktop case -- is `BatteryState::default()`, the correct
/// answer § 2.2 wants, not an error to log or a state to special-case.
pub fn read_battery_state(power_supply_root: &Path) -> BatteryState {
    let Some(entry) = select_system_battery(power_supply_root) else {
        return BatteryState::default();
    };
    let percent = percent_from_capacity(read_attr(&entry, "capacity").as_deref());
    let charging = read_attr(&entry, "status").is_some_and(|status| charging_from_status(&status));
    BatteryState { present: true, percent, charging }
}

/// Cadence for the fallback path only -- [`run_battery_poll_loop`], used when
/// [`build_power_supply_watch`] can't stand up the real udev `power_supply` watch at all, or
/// [`run_battery_watch_loop`]'s own watch fd errors mid-run (both now rare: `udev`'s `send`
/// feature is enabled in `Cargo.toml`, which is what makes `MonitorBuilder`/`MonitorSocket`
/// `Send` and lets the watch below exist as a `tokio::spawn`ed task in the first place -- see
/// `build_power_supply_watch`'s own doc comment for the mechanics). The primary path is the
/// udev watch, not this: a battery's `percent` drifts slowly, but `charging` flips the instant
/// a charger is plugged or unplugged, and that instant matters to a config in a way `percent`'s
/// drift doesn't.
///
/// ponytail: 30s is a round-number guess for the fallback's staleness budget, not measured
/// against anything -- it only matters on the already-degraded path (no working udev watch), so
/// getting it exactly right matters far less than the primary path above does. If it ever needs
/// to be tighter or user-tunable, `SysinfoController`'s configurable watch-channel interval
/// (docs/adr/0035) is the pattern to reach for rather than hardcoding a different constant here.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// `Clone` (mirrors `KeyboardController`) even though nothing here currently spawns a second
/// task off a cloned handle. Not `Clone`, matching `PrivacyController` and `LockController`:
/// § 2.2 has no write action, so nothing needs a second handle, and deriving it "for parity"
/// would be a capability with no caller.
pub struct BatteryController {
    state: Arc<Mutex<BatteryState>>,
}

impl BatteryController {
    /// `power_supply_root` (real default `/sys/class/power_supply`) follows this codebase's
    /// sysfs-root-injection convention (`proc_root`/`hwmon_root`/`leds_root`/
    /// `video4linux_root`), the same thing that makes [`read_battery_state`] testable against a
    /// fixture directory. Returns immediately; [`run_battery_task`] does the real reading (udev-
    /// watch-driven, with a poll fallback -- see its own doc comment) in a spawned task, matching
    /// every other event-driven controller's "construction never blocks on I/O" shape.
    pub fn new(power_supply_root: PathBuf, events: UnboundedSender<BatterySignal>) -> Self {
        let state = Arc::new(Mutex::new(BatteryState::default()));
        tokio::spawn(run_battery_task(power_supply_root, Arc::clone(&state), events));
        Self { state }
    }

    pub fn snapshot(&self) -> BatteryState {
        *self.state.lock().expect("battery state mutex poisoned")
    }
}

/// Reads the initial state before entering either wait mode (a config needs battery state
/// immediately, not after the first watch event or poll tick -- matches
/// `privacy::controller::run_camera_task`'s own "initial scan, then the event loop" ordering),
/// then hands off to [`run_battery_watch_loop`], the real udev-driven primary path. Degrades to
/// [`run_battery_poll_loop`] only if [`build_power_supply_watch`] itself fails to stand up --
/// a laptop with a working battery and a broken udev socket (no permission to open a netlink
/// socket, an exhausted fd table, or similar) must still show a battery, not go permanently
/// dark just because its preferred update path didn't come up.
async fn run_battery_task(power_supply_root: PathBuf, state: Arc<Mutex<BatteryState>>, events: UnboundedSender<BatterySignal>) {
    let initial = read_battery_state(&power_supply_root);
    *state.lock().expect("battery state mutex poisoned") = initial;
    if events.send(BatterySignal::Changed).is_err() {
        return;
    }

    match build_power_supply_watch() {
        Ok(watch) => run_battery_watch_loop(watch, power_supply_root, initial, state, events).await,
        Err(err) => {
            eprintln!("battery: failed to set up the udev power_supply watch ({err}); falling back to a {POLL_INTERVAL:?} poll");
            run_battery_poll_loop(power_supply_root, initial, state, events).await;
        }
    }
}

/// Builds the real `power_supply` subsystem udev watch (§ 2.2; docs/build-steps.md line 98's
/// original justification for the `udev` dependency -- this is its first real caller): a
/// monitor filtered to just that one subsystem, `listen()`ed, with its netlink socket's raw fd
/// registered against tokio's IO driver via `AsyncFd` so [`run_battery_watch_loop`] can `await`
/// readability instead of ticking. Needs `udev`'s `send` feature (Cargo.toml) to typecheck at
/// all -- `MonitorBuilder`/`MonitorSocket` must be `Send` to live inside the `tokio::spawn`ed
/// future `run_battery_task` hands them to. Fails at whichever of socket allocation, the
/// subsystem filter, or `AsyncFd` registration breaks first; the caller treats any of the three
/// the same way, by falling back to [`POLL_INTERVAL`] polling.
fn build_power_supply_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("power_supply")?.listen()?;
    AsyncFd::new(socket)
}

/// The primary path: awaits the udev `power_supply` watch's fd becoming readable, drains every
/// pending netlink message (a single plug/unplug can fire more than one message for the one
/// subsystem change, and level-triggered readiness would otherwise immediately re-fire on
/// whatever's left undrained), then re-reads the full state and only writes `state`/sends
/// [`BatterySignal::Changed`] when it actually differs from `previous`. This push-on-change
/// filter is doing more work here than it did under the old poll-only design: udev wakes this
/// loop on *every* `power_supply` subsystem change, including the many this capability doesn't
/// report at all (`voltage_now`, `energy_now`, and similar attributes churn on a live battery
/// far more often than `capacity`/`status` do), so most wakeups are expected to be filtered out
/// here, not most ticks being redundant the way the old poll loop's filter mostly saw. Falls
/// back to [`run_battery_poll_loop`] if the watch's fd itself ever errors mid-run (rare --
/// effectively only a reactor shutdown), the same "degrade, don't go dark" reason
/// [`build_power_supply_watch`]'s own failure does.
///
/// `watch` is taken `mut` and polled with `readable_mut` rather than `readable`: `readable`
/// takes `&self` and its guard borrows `&AsyncFd<MonitorSocket>`, which needs `MonitorSocket:
/// Sync` to be `Send` across the `.await` inside this loop's error arm -- and only `udev`'s
/// `send` feature is enabled in `Cargo.toml`, not `sync` (confirmed live: `readable` fails
/// `tokio::spawn`'s `Send` bound with exactly that error). `readable_mut` takes `&mut self`
/// instead, so its guard only needs `MonitorSocket: Send`, which the enabled feature already
/// gives it -- no further Cargo.toml change needed.
async fn run_battery_watch_loop(
    mut watch: AsyncFd<MonitorSocket>,
    power_supply_root: PathBuf,
    mut previous: BatteryState,
    state: Arc<Mutex<BatteryState>>,
    events: UnboundedSender<BatterySignal>,
) {
    loop {
        let mut guard = match watch.readable_mut().await {
            Ok(guard) => guard,
            Err(err) => {
                eprintln!("battery: the udev power_supply watch's fd errored ({err}); falling back to a {POLL_INTERVAL:?} poll for the rest of this run");
                return run_battery_poll_loop(power_supply_root, previous, state, events).await;
            }
        };
        for _event in guard.get_inner().iter() {}
        guard.clear_ready();

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

/// The fallback path (see [`POLL_INTERVAL`]'s own doc comment for the two ways this gets
/// reached instead of [`run_battery_watch_loop`]): re-reads on a fixed timer instead of a real
/// event, with the same push-on-change filter as the primary path, so the two paths are
/// observably identical to a config -- just at different latency.
async fn run_battery_poll_loop(power_supply_root: PathBuf, mut previous: BatteryState, state: Arc<Mutex<BatteryState>>, events: UnboundedSender<BatterySignal>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.tick().await; // tokio::time::interval's first tick fires immediately; the caller's initial (or pre-fallback) read already covers it

    loop {
        ticker.tick().await;
        let current = read_battery_state(&power_supply_root);
        if current != previous {
            *state.lock().expect("battery state mutex poisoned") = current;
            previous = current;
            if events.send(BatterySignal::Changed).is_err() {
                break;
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
        write_entry(root.path(), "ucsi-source-psy-USBC000:001", &[("type", "USB"), ("scope", "System"), ("status", "Not charging")]);

        assert_eq!(read_battery_state(root.path()), BatteryState { present: true, percent: 59, charging: false });
    }

    #[test]
    fn read_battery_state_excludes_a_device_scoped_peripheral_battery() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "hid-aa-bb-battery", &[("type", "Battery"), ("scope", "Device"), ("capacity", "80"), ("status", "Discharging")]);

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
        assert_eq!(signal, Ok(Some(BatterySignal::Changed)), "must announce the initial state without waiting for the first poll tick");
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
