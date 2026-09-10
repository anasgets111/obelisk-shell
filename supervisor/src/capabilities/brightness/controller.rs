//! [`BrightnessController`] owns `oblisk.brightness` and its write action.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use udev::MonitorSocket;

use super::super::read_attr;
use super::super::scale::{percent_from_raw, raw_from_percent};

/// `oblisk.brightness`'s full payload (§ 2.3). `percent` is the unchanged `StateSnapshot` JSON
/// key. `Default` (`0`) precedes the first read, but no-device construction emits no signal, so
/// Lua never observes the placeholder (see `brightness/mod.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, schemars::JsonSchema)]
pub struct BrightnessState {
    /// Screen backlight, `0` to `100`, from sysfs `brightness` (the requested value), not
    /// `actual_brightness`, which can lag during a hardware fade.
    pub percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrightnessSignal {
    Changed,
}

/// § 2.3 device preference from `Documentation/ABI/stable/sysfs-class-backlight`: firmware (0) <
/// platform (1) < raw (2), with unknown/missing last (3), not excluded.
fn device_type_rank(entry_dir: &Path) -> u8 {
    match read_attr(entry_dir, "type").as_deref() {
        Some("firmware") => 0,
        Some("platform") => 1,
        Some("raw") => 2,
        _ => 3,
    }
}

/// Parsed `max_brightness`; missing or malformed values become `0`, and the `> 0` filter in
/// [`select_backlight_device`] rejects every non-positive value.
fn read_max_brightness(entry_dir: &Path) -> i32 {
    read_attr(entry_dir, "max_brightness").and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// Picks one `max_brightness > 0` device (ADR-0053), ranked by [`device_type_rank`] and then
/// sorted directory name for deterministic boot-to-boot selection. `None` if none qualifies.
fn select_backlight_device(backlight_root: &Path) -> Option<(PathBuf, i32)> {
    std::fs::read_dir(backlight_root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|dir| {
            let max = read_max_brightness(&dir);
            (max > 0).then_some((dir, max))
        })
        .min_by(|(dir_a, _), (dir_b, _)| {
            device_type_rank(dir_a).cmp(&device_type_rank(dir_b)).then_with(|| dir_a.cmp(dir_b))
        })
}

/// Reads `brightness` (the last requested value), not `actual_brightness`: a driver fade or
/// rounded request can differ, and `set(50)` must read back `50`. Missing/malformed reads are `0`.
/// [`select_backlight_device`] guarantees positive `max`, so the `u8` result is `[0, 100]`.
fn read_percent(device_dir: &Path, max: i32) -> u8 {
    let brightness = read_attr(device_dir, "brightness").and_then(|text| text.parse::<i32>().ok()).unwrap_or(0);
    percent_from_raw(brightness, max) as u8
}

/// `brightness:set(pct)`'s `arguments: [pct]`; § 3.2 range validation is deferred to
/// `scale::raw_from_percent`.
pub fn parse_set_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `org.freedesktop.login1.Session.SetBrightness` on fixed `session/auto`, which logind resolves
/// to the caller's session. Built per [`BrightnessController::set`] call because writes are rare.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait Login1Session {
    #[zbus(name = "SetBrightness")]
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

/// The device [`select_backlight_device`] chose at construction. Its `max_brightness` and sysfs
/// directory do not change at runtime; `name` is cached for each `SetBrightness` call.
struct BacklightDevice {
    dir: PathBuf,
    name: String,
    max: i32,
}

/// Fallback cadence, matching private `battery::controller::POLL_INTERVAL`.
///
/// ponytail: `battery` and `brightness` duplicate the initial-read, udev-watch, poll-fallback
/// shape, but only two callers have different payload types. Extract a harness at the third
/// sysfs-watched capability, parameterized by the read closure and state type; move this constant
/// with it.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct BrightnessController {
    state: Arc<Mutex<BrightnessState>>,
    device: Arc<Option<BacklightDevice>>,
    system_bus: zbus::Connection,
}

impl BrightnessController {
    /// `backlight_root` (default `/sys/class/backlight`) is injected for tests. `system_bus` is
    /// the Supervisor's existing connection used by [`Login1SessionProxy`]. No usable device
    /// leaves `device` as `None`, skips the read task, and emits no signal (see
    /// `brightness/mod.rs`).
    pub fn new(
        backlight_root: PathBuf,
        system_bus: zbus::Connection,
        events: UnboundedSender<BrightnessSignal>,
    ) -> Self {
        let state = Arc::new(Mutex::new(BrightnessState::default()));
        let selected = select_backlight_device(&backlight_root);
        let device = selected.map(|(dir, max)| {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            BacklightDevice { dir, name, max }
        });
        match &device {
            Some(device) => {
                tokio::spawn(run_brightness_task(device.dir.clone(), device.max, Arc::clone(&state), events));
            }
            None => eprintln!(
                "brightness: no usable backlight device found under {backlight_root:?}; brightness reporting disabled for this run"
            ),
        }
        Self { state, device: Arc::new(device), system_bus }
    }

    pub fn snapshot(&self) -> BrightnessState {
        *self.state.lock().expect("brightness state mutex poisoned")
    }

    /// `brightness:set(pct)`. Logs and returns when this machine has no backlight device.
    pub async fn set(&self, pct: u64) {
        let Some(device) = self.device.as_ref() else {
            eprintln!("brightness: set called but no backlight device was found; ignored");
            return;
        };
        let proxy = match Login1SessionProxy::new(&self.system_bus).await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("brightness: failed to build the login1 Session proxy: {err}");
                return;
            }
        };
        let raw = raw_from_percent(pct, device.max) as u32;
        // logind refuses SetBrightness from a non-active session (for example a background VT).
        // Log that error; do not retry it.
        if let Err(err) = proxy.set_brightness("backlight", &device.name, raw).await {
            eprintln!("brightness: SetBrightness(backlight, {}, {raw}) failed: {err}", device.name);
        }
        // State changes arrive through the udev watch/poll loop, not an optimistic local update.
    }
}

/// Reads and sends the initial percent, then uses [`run_brightness_watch_loop`], falling back to
/// [`run_brightness_poll_loop`] only when [`build_backlight_watch`] cannot start. Construction has
/// already found a usable device, so this task cannot stay at the default forever.
async fn run_brightness_task(
    device_dir: PathBuf,
    max: i32,
    state: Arc<Mutex<BrightnessState>>,
    events: UnboundedSender<BrightnessSignal>,
) {
    let initial = read_percent(&device_dir, max);
    state.lock().expect("brightness state mutex poisoned").percent = initial;
    if events.send(BrightnessSignal::Changed).is_err() {
        return;
    }

    match build_backlight_watch() {
        Ok(watch) => run_brightness_watch_loop(watch, device_dir, max, initial, state, events).await,
        Err(err) => {
            eprintln!(
                "brightness: failed to set up the udev backlight watch ({err}); falling back to a {POLL_INTERVAL:?} poll"
            );
            run_brightness_poll_loop(device_dir, max, initial, state, events).await;
        }
    }
}

/// Builds the `backlight` udev watch, like `battery::controller::build_power_supply_watch`.
///
/// inotify misses sysfs attribute writes. `udevadm monitor --udev --subsystem-match=backlight`
/// confirmed that brightness changes emit a `change` uevent on the `backlight` subsystem instead.
fn build_backlight_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("backlight")?.listen()?;
    AsyncFd::new(socket)
}

/// Awaits a readable udev fd, drains netlink messages, then pushes only a changed reading. Uses
/// `readable_mut`, not `readable`, because only udev's `send` feature is enabled, not `sync`.
async fn run_brightness_watch_loop(
    mut watch: AsyncFd<MonitorSocket>,
    device_dir: PathBuf,
    max: i32,
    mut previous: u8,
    state: Arc<Mutex<BrightnessState>>,
    events: UnboundedSender<BrightnessSignal>,
) {
    loop {
        let mut guard = match watch.readable_mut().await {
            Ok(guard) => guard,
            Err(err) => {
                eprintln!(
                    "brightness: the udev backlight watch's fd errored ({err}); falling back to a {POLL_INTERVAL:?} poll for the rest of this run"
                );
                return run_brightness_poll_loop(device_dir, max, previous, state, events).await;
            }
        };
        for _event in guard.get_inner().iter() {}
        guard.clear_ready();

        let current = read_percent(&device_dir, max);
        if current != previous {
            state.lock().expect("brightness state mutex poisoned").percent = current;
            previous = current;
            if events.send(BrightnessSignal::Changed).is_err() {
                return;
            }
        }
    }
}

/// Fallback: fixed-timer reads with the primary path's push-on-change filter.
async fn run_brightness_poll_loop(
    device_dir: PathBuf,
    max: i32,
    mut previous: u8,
    state: Arc<Mutex<BrightnessState>>,
    events: UnboundedSender<BrightnessSignal>,
) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.tick().await; // tokio::time::interval's first tick fires immediately; the caller's initial (or pre-fallback) read already covers it

    loop {
        ticker.tick().await;
        let current = read_percent(&device_dir, max);
        if current != previous {
            state.lock().expect("brightness state mutex poisoned").percent = current;
            previous = current;
            if events.send(BrightnessSignal::Changed).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    fn write_entry(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (attr, value) in attrs {
            std::fs::write(dir.join(attr), value).unwrap();
        }
    }

    #[test]
    fn device_type_rank_orders_firmware_before_platform_before_raw_before_unknown() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "fw", &[("type", "firmware")]);
        write_entry(root.path(), "pf", &[("type", "platform")]);
        write_entry(root.path(), "rw", &[("type", "raw")]);
        write_entry(root.path(), "other", &[("type", "something-else")]);

        assert!(device_type_rank(&root.path().join("fw")) < device_type_rank(&root.path().join("pf")));
        assert!(device_type_rank(&root.path().join("pf")) < device_type_rank(&root.path().join("rw")));
        assert!(device_type_rank(&root.path().join("rw")) < device_type_rank(&root.path().join("other")));
    }

    #[test]
    fn select_backlight_device_prefers_firmware_over_platform_over_raw() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "acpi_video0", &[("type", "platform"), ("max_brightness", "100")]);
        write_entry(root.path(), "intel_backlight", &[("type", "raw"), ("max_brightness", "19200")]);
        write_entry(root.path(), "some_fw_backlight", &[("type", "firmware"), ("max_brightness", "255")]);

        let (dir, max) = select_backlight_device(root.path()).expect("expected a device to be selected");
        assert_eq!(dir, root.path().join("some_fw_backlight"));
        assert_eq!(max, 255);
    }

    #[test]
    fn select_backlight_device_ties_broken_by_sorted_directory_name() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "raw_b", &[("type", "raw"), ("max_brightness", "10")]);
        write_entry(root.path(), "raw_a", &[("type", "raw"), ("max_brightness", "20")]);

        let (dir, _) = select_backlight_device(root.path()).expect("expected a device to be selected");
        assert_eq!(dir, root.path().join("raw_a"));
    }

    #[test]
    fn select_backlight_device_skips_a_device_with_non_positive_max_brightness() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "broken", &[("type", "raw"), ("max_brightness", "0")]);
        write_entry(root.path(), "usable", &[("type", "raw"), ("max_brightness", "100")]);

        let (dir, max) = select_backlight_device(root.path()).expect("expected the usable device to be selected");
        assert_eq!(dir, root.path().join("usable"));
        assert_eq!(max, 100);
    }

    #[test]
    fn select_backlight_device_is_none_against_an_empty_or_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(select_backlight_device(root.path()), None);
        assert_eq!(select_backlight_device(&root.path().join("does-not-exist")), None);
    }

    #[test]
    fn read_percent_reads_the_requested_brightness_not_the_actual_one() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "intel_backlight", &[("brightness", "9600"), ("actual_brightness", "9601")]);

        assert_eq!(read_percent(&root.path().join("intel_backlight"), 19200), 50);
    }

    #[test]
    fn read_percent_is_zero_for_a_missing_or_unparseable_reading() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "no_attr", &[]);
        assert_eq!(read_percent(&root.path().join("no_attr"), 100), 0);

        write_entry(root.path(), "bad_attr", &[("brightness", "not-a-number")]);
        assert_eq!(read_percent(&root.path().join("bad_attr"), 100), 0);
    }

    #[test]
    fn parse_set_args_reads_the_first_argument_as_a_percent() {
        let args = vec![serde_json::json!(42)];
        assert_eq!(parse_set_args(&args), Some(42));
    }

    #[test]
    fn parse_set_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_set_args(&[]), None);
        let args = vec![serde_json::json!("not a number")];
        assert_eq!(parse_set_args(&args), None);
    }

    #[tokio::test]
    async fn brightness_controller_pushes_the_initial_state_before_the_first_poll_tick() {
        let root = tempfile::tempdir().unwrap();
        write_entry(
            root.path(),
            "intel_backlight",
            &[("type", "raw"), ("max_brightness", "19200"), ("brightness", "9600")],
        );
        let (_service_side, caller_side) = p2p_pair().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BrightnessController::new(root.path().to_path_buf(), caller_side, events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(
            signal,
            Ok(Some(BrightnessSignal::Changed)),
            "must announce the initial state without waiting for the first poll tick"
        );
        assert_eq!(controller.snapshot(), BrightnessState { percent: 50 });
    }

    #[tokio::test]
    async fn brightness_controller_never_signals_when_no_backlight_device_exists() {
        let root = tempfile::tempdir().unwrap(); // empty -- the desktop case
        let (_service_side, caller_side) = p2p_pair().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BrightnessController::new(root.path().to_path_buf(), caller_side, events_tx);

        // No device means `new` drops `events`; `recv` returns `None` instead of hanging.
        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(None), "no device means no signal is ever sent, not even the default state");
        assert_eq!(controller.snapshot(), BrightnessState::default());
    }
}
