//! [`BrightnessController`]: the `oblisk.brightness` state owner and its one write action.
//! Split from `brightness` -- see `brightness/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use udev::MonitorSocket;

use super::super::read_attr;
use super::super::scale::{percent_from_raw, raw_from_percent};

/// `oblisk.brightness`'s full payload (§ 2.3). `percent` is the JSON key verbatim -- the
/// Renderer routes it into the Lua `oblisk.brightness` signal table by name, unchanged.
/// `Default` (`0`) is a placeholder before the first real read; never observed if no device
/// was found, since no signal is sent in that case (see `brightness/mod.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct BrightnessState {
    pub percent: u8,
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrightnessSignal {
    Changed,
}

/// § 2.3's device-selection preference: rank by `type` per
/// `Documentation/ABI/stable/sysfs-class-backlight` -- firmware (0) < platform (1) < raw (2),
/// unknown/missing last (3) rather than excluded.
fn device_type_rank(entry_dir: &Path) -> u8 {
    match read_attr(entry_dir, "type").as_deref() {
        Some("firmware") => 0,
        Some("platform") => 1,
        Some("raw") => 2,
        _ => 3,
    }
}

/// `max_brightness`, parsed; `0` (not `-1`) for a missing/unparseable reading -- the `> 0`
/// filter in [`select_backlight_device`] treats every non-positive value as unusable.
fn read_max_brightness(entry_dir: &Path) -> i32 {
    read_attr(entry_dir, "max_brightness").and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// Picks one backlight device under `backlight_root`: entries with `max_brightness > 0`
/// (docs/adr/0053 decision 6), ranked by [`device_type_rank`], ties broken by sorted
/// directory name for a deterministic choice across boots. `None` if nothing qualifies.
fn select_backlight_device(backlight_root: &Path) -> Option<(PathBuf, i32)> {
    let mut entries: Vec<(PathBuf, i32)> = std::fs::read_dir(backlight_root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|dir| {
            let max = read_max_brightness(&dir);
            (max > 0).then_some((dir, max))
        })
        .collect();
    entries.sort_by(|(dir_a, _), (dir_b, _)| device_type_rank(dir_a).cmp(&device_type_rank(dir_b)).then_with(|| dir_a.cmp(dir_b)));
    entries.into_iter().next()
}

/// `brightness.percent`: `brightness` (last requested value), not `actual_brightness`
/// (hardware readback) -- they can differ during a driver fade or a rounded request, and a
/// config that just called `set(50)` needs to see `50` come back. `0` on a missing/
/// unparseable reading. `max` must be positive (guaranteed by [`select_backlight_device`]'s
/// `> 0` filter), so the cast to `u8` is always in `[0, 100]`.
fn read_percent(device_dir: &Path, max: i32) -> u8 {
    let brightness = read_attr(device_dir, "brightness").and_then(|text| text.parse::<i32>().ok()).unwrap_or(0);
    percent_from_raw(brightness, max) as u8
}

/// `brightness:set(pct)`'s `arguments: [pct]`. No range check here (§ 3.2's `[0, 100]`) --
/// clamping happens once, in `scale::raw_from_percent`.
pub fn parse_set_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `org.freedesktop.login1.Session.SetBrightness` on the fixed `session/auto` object path,
/// which logind resolves to the caller's own session. Built fresh on each
/// [`BrightnessController::set`] call rather than cached; this call is rare, not a hot path.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait Login1Session {
    #[zbus(name = "SetBrightness")]
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

/// The device [`select_backlight_device`] chose at construction, resolved once -- a panel's
/// `max_brightness` and sysfs directory don't change at runtime. `name` is cached since
/// `set`'s `SetBrightness` call needs it on every call.
struct BacklightDevice {
    dir: PathBuf,
    name: String,
    max: i32,
}

/// Cadence for the fallback path only. Mirrors `battery::controller::POLL_INTERVAL`'s value;
/// not imported since it's a private constant there.
///
/// ponytail: this constant is the visible tip of a larger duplicate. `battery` and `brightness`
/// now have the same three-function shape (initial read, udev watch loop, poll fallback, each
/// pushing only on a real change), differing only in what they read and what state they write.
/// Two instances is not a pattern and extracting a generic sysfs-watch harness for two callers
/// with different payload types would be building the abstraction before knowing its shape. The
/// third sysfs-watched capability is the upgrade point: extract then, taking the read closure and
/// the state type as parameters, and this constant goes with it.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// `Clone`: `main.rs`'s `brightness:set` dispatch needs a cheap `Arc`-backed copy to hand to
/// the `tokio::spawn`ed task the D-Bus call runs in.
#[derive(Clone)]
pub struct BrightnessController {
    state: Arc<Mutex<BrightnessState>>,
    device: Arc<Option<BacklightDevice>>,
    system_bus: zbus::Connection,
}

impl BrightnessController {
    /// `backlight_root` (real default `/sys/class/backlight`) is injected for testability.
    /// `system_bus` is the Supervisor's already-established connection;
    /// [`Login1SessionProxy`] rides it directly.
    ///
    /// Returns immediately. No usable device under `backlight_root` leaves `device` at
    /// `None` and never spawns the read task -- no signal is ever sent in that case either
    /// (see `brightness/mod.rs`).
    pub fn new(backlight_root: PathBuf, system_bus: zbus::Connection, events: UnboundedSender<BrightnessSignal>) -> Self {
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
            None => eprintln!("brightness: no usable backlight device found under {backlight_root:?}; brightness reporting disabled for this run"),
        }
        Self { state, device: Arc::new(device), system_bus }
    }

    pub fn snapshot(&self) -> BrightnessState {
        *self.state.lock().expect("brightness state mutex poisoned")
    }

    /// `brightness:set(pct)`. A silent no-op (logged once) when this machine has no backlight
    /// device.
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
        // logind refuses SetBrightness from a session that isn't the seat's active session
        // (e.g. a background VT); that surfaces as an `Err` here and is logged, not retried.
        if let Err(err) = proxy.set_brightness("backlight", &device.name, raw).await {
            eprintln!("brightness: SetBrightness(backlight, {}, {raw}) failed: {err}", device.name);
        }
        // No optimistic local update: state changes flow through the signal, off the udev
        // watch/poll loop.
    }
}

/// Reads the initial percent, sends it, then hands off to [`run_brightness_watch_loop`].
/// Falls back to [`run_brightness_poll_loop`] only if [`build_backlight_watch`] fails to
/// stand up. This task only spawns once [`select_backlight_device`] already found a usable
/// device, so there's no "stay at default forever" branch to reach here.
async fn run_brightness_task(device_dir: PathBuf, max: i32, state: Arc<Mutex<BrightnessState>>, events: UnboundedSender<BrightnessSignal>) {
    let initial = read_percent(&device_dir, max);
    state.lock().expect("brightness state mutex poisoned").percent = initial;
    if events.send(BrightnessSignal::Changed).is_err() {
        return;
    }

    match build_backlight_watch() {
        Ok(watch) => run_brightness_watch_loop(watch, device_dir, max, initial, state, events).await,
        Err(err) => {
            eprintln!("brightness: failed to set up the udev backlight watch ({err}); falling back to a {POLL_INTERVAL:?} poll");
            run_brightness_poll_loop(device_dir, max, initial, state, events).await;
        }
    }
}

/// Builds the `backlight` subsystem udev watch (same construction as
/// `battery::controller::build_power_supply_watch`).
///
/// Corrects docs/build-steps.md line 98: inotify does not fire reliably on a sysfs attribute
/// write, confirmed via `udevadm monitor --udev --subsystem-match=backlight` while changing
/// brightness, which does show a `change` uevent on the `backlight` subsystem.
fn build_backlight_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("backlight")?.listen()?;
    AsyncFd::new(socket)
}

/// Awaits the udev watch's fd becoming readable, drains pending netlink messages, then
/// re-reads and pushes only on an actual change. Uses `readable_mut` (not `readable`) since
/// only `udev`'s `send` feature is enabled, not `sync`.
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
                eprintln!("brightness: the udev backlight watch's fd errored ({err}); falling back to a {POLL_INTERVAL:?} poll for the rest of this run");
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

/// The fallback path: re-reads on a fixed timer instead of a real event, same push-on-change
/// filter as the primary path.
async fn run_brightness_poll_loop(device_dir: PathBuf, max: i32, mut previous: u8, state: Arc<Mutex<BrightnessState>>, events: UnboundedSender<BrightnessSignal>) {
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

    fn write_entry(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (attr, value) in attrs {
            std::fs::write(dir.join(attr), value).unwrap();
        }
    }

    // ---- device_type_rank ----

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

    // ---- select_backlight_device ----

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

    // ---- read_percent ----

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

    // ---- parse_set_args ----

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

    // ---- BrightnessController (construction/task wiring) ----

    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = tokio::net::UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder = zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    #[tokio::test]
    async fn brightness_controller_pushes_the_initial_state_before_the_first_poll_tick() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "intel_backlight", &[("type", "raw"), ("max_brightness", "19200"), ("brightness", "9600")]);
        let (_service_side, caller_side) = p2p_pair().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BrightnessController::new(root.path().to_path_buf(), caller_side, events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(Some(BrightnessSignal::Changed)), "must announce the initial state without waiting for the first poll tick");
        assert_eq!(controller.snapshot(), BrightnessState { percent: 50 });
    }

    #[tokio::test]
    async fn brightness_controller_never_signals_when_no_backlight_device_exists() {
        let root = tempfile::tempdir().unwrap(); // empty -- the desktop case
        let (_service_side, caller_side) = p2p_pair().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BrightnessController::new(root.path().to_path_buf(), caller_side, events_tx);

        // No device means `new` never spawns the read task, so `events` is dropped at the end
        // of `new` -- `recv` returns `None` immediately rather than hanging.
        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(None), "no device means no signal is ever sent, not even the default state");
        assert_eq!(controller.snapshot(), BrightnessState::default());
    }
}
