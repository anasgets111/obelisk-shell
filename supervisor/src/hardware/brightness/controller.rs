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
/// Renderer routes it straight into the Lua `oblisk.brightness` signal table by name, unchanged.
/// `Default` (`0`) is never observed outside this module: it exists only to give
/// [`BrightnessController::new`] something to put behind the mutex before the first real read
/// completes, because (per `brightness/mod.rs`'s own doc comment) no signal is ever sent, and so
/// this value is never pushed, unless a real device was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct BrightnessState {
    pub percent: u8,
}

/// One shared signal, `Changed` only (mirrors `BatterySignal`/`KeyboardSignal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrightnessSignal {
    Changed,
}

/// § 2.3's device-selection preference, as one pure ranking: lower ranks first. Ranks by `type`
/// (`Documentation/ABI/stable/sysfs-class-backlight`'s own reason that attribute exists) --
/// `firmware` (0) before `platform` (1) before `raw` (2), with anything else (an absent or
/// unrecognized `type` file) last (3) rather than excluded, so a device this ranking doesn't
/// recognize can still be picked when nothing better exists.
fn device_type_rank(entry_dir: &Path) -> u8 {
    match read_attr(entry_dir, "type").as_deref() {
        Some("firmware") => 0,
        Some("platform") => 1,
        Some("raw") => 2,
        _ => 3,
    }
}

/// `max_brightness`, parsed, or `0` for a missing/unparseable reading -- `0` (not `-1` or any
/// other sentinel) because [`select_backlight_device`]'s own `> 0` filter is the only caller and
/// treats every non-positive value identically, as "unusable".
fn read_max_brightness(entry_dir: &Path) -> i32 {
    read_attr(entry_dir, "max_brightness").and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// Picks one backlight device under `backlight_root`: every entry whose `max_brightness` is
/// usable (`> 0` -- docs/adr/0053 decision 6, since this capability has no sentinel to report for
/// an unusable one, unlike `keyboard::percent_from_raw`'s `-1`), ranked by [`device_type_rank`]
/// and tie-broken by sorted directory name for a deterministic choice across boots (the same
/// reason `battery::controller::select_system_battery` sorts first). `None` on a missing/
/// unreadable root or when nothing qualifies (the common desktop case, or a directory of only
/// unusable entries).
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

/// `brightness.percent`: `brightness` (the last requested value), not `actual_brightness` (the
/// hardware readback) -- both attributes exist, and they differ while a driver fade is in flight
/// or when the driver rounds a requested value to a step its hardware can't hit exactly. A
/// config calling `brightness:set(50)` and then reading `oblisk.brightness.percent` needs to see
/// `50` come back, which is `brightness`'s job, not `actual_brightness`'s. A missing or
/// unparseable reading is `0` -- the same "degrade to the honest default, don't fabricate"
/// posture `battery::controller::percent_from_capacity` already takes for its own missing/
/// malformed case. `max` is trusted to be positive: every caller reaches this only through
/// [`select_backlight_device`]'s own `> 0` filter, so `scale::percent_from_raw`'s `-1` sentinel
/// branch is unreachable here and the cast to `u8` is always in `[0, 100]`.
fn read_percent(device_dir: &Path, max: i32) -> u8 {
    let brightness = read_attr(device_dir, "brightness").and_then(|text| text.parse::<i32>().ok()).unwrap_or(0);
    percent_from_raw(brightness, max) as u8
}

/// `brightness:set(pct)`'s `arguments: [pct]`. Validation stops at the shape check (§ 3.2's
/// `[0, 100]` integer range is not re-checked here, matching every other numeric `parse_*_args`
/// in this codebase -- e.g. `keyboard::parse_set_backlight_args`'s own doc comment): clamping
/// happens once, in `scale::raw_from_percent`, the one place that actually needs the bound.
pub fn parse_set_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `org.freedesktop.login1.Session.SetBrightness` on the fixed `session/auto` object path, which
/// logind resolves to the caller's own session -- reading `$XDG_SESSION_ID` and building the path
/// by hand would be a second source of truth for the same answer. Built fresh on every
/// [`BrightnessController::set`] call rather than cached, the same shape `idle`'s
/// `Login1ManagerProxy` already uses for its own login1 call (see `idle/inhibit.rs`'s own doc
/// comment) -- a cached `'static` proxy would need to outlive the connection it borrows for no
/// benefit, since this call is rare (a user adjusting a brightness slider, not a hot path).
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait Login1Session {
    #[zbus(name = "SetBrightness")]
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

/// The one backlight device [`select_backlight_device`] chose at construction, resolved once
/// (mirrors `keyboard::Backlight::Live`'s own "a keyboard's brightness step count doesn't change
/// at runtime" precedent -- a laptop panel's `max_brightness` doesn't either, and neither does
/// which sysfs directory represents it). `name` is `dir`'s own file name, cached because
/// `set`'s `SetBrightness` call needs it on every call and `Path::file_name` is a cheap-but-not-
/// free OsStr slice each time.
struct BacklightDevice {
    dir: PathBuf,
    name: String,
    max: i32,
}

/// Cadence for the fallback path only -- mirrors `battery::controller::POLL_INTERVAL` in both
/// name and value (that module's own doc comment covers the reasoning: primary is the udev
/// watch, this only covers the rare case where the watch itself can't stand up or errors out
/// mid-run). Not imported from `battery` directly -- it is a private constant there, and
/// duplicating one `Duration` literal here is cheaper than making it `pub(crate)` for a single
/// cross-module read.
///
/// ponytail: this constant is the visible tip of a larger duplicate. `battery` and `brightness`
/// now have the same three-function shape (initial read, udev watch loop, poll fallback, each
/// pushing only on a real change), differing only in what they read and what state they write.
/// Two instances is not a pattern and extracting a generic sysfs-watch harness for two callers
/// with different payload types would be building the abstraction before knowing its shape. The
/// third sysfs-watched capability is the upgrade point: extract then, taking the read closure and
/// the state type as parameters, and this constant goes with it.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// `Clone` (mirrors `KeyboardController`): `main.rs`'s `brightness:set` dispatch arm needs a
/// cheap `Arc`-backed copy to hand to the `tokio::spawn`ed task the D-Bus call runs in, since
/// `set` can't run inline in `main.rs`'s `select!` without blocking every other arm on it.
#[derive(Clone)]
pub struct BrightnessController {
    state: Arc<Mutex<BrightnessState>>,
    device: Arc<Option<BacklightDevice>>,
    system_bus: zbus::Connection,
}

impl BrightnessController {
    /// `backlight_root` (real default `/sys/class/backlight`) follows this codebase's sysfs-
    /// root-injection convention (`power_supply_root`/`proc_root`/`hwmon_root`/`leds_root`), the
    /// same thing that makes [`select_backlight_device`] testable against a fixture directory.
    /// `system_bus` is the Supervisor's already-established `zbus::Connection::system()` --
    /// [`Login1SessionProxy`] rides it directly, no new connection, the same reuse `keyboard`'s
    /// own `KbdBacklightProxy` already does.
    ///
    /// Returns immediately. No device found under `backlight_root` (a desktop with no panel, or
    /// every entry's `max_brightness` unusable) leaves `device` at `None` and never spawns the
    /// read task at all -- there is nothing for it to watch, and per `brightness/mod.rs`'s own
    /// doc comment, no signal is ever sent in that case either.
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

    /// `brightness:set(pct)`. A silent no-op (logged once per call, matching
    /// `KeyboardController::set_backlight`'s own missing-hardware posture) when this machine has
    /// no backlight device -- there is nothing to set.
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
        // logind refuses SetBrightness from a session that isn't the seat's active session (e.g.
        // a background VT) -- that failure surfaces here as an `Err` and is logged, not retried
        // or worked around, because it's logind correctly protecting the display a non-active
        // session doesn't own.
        if let Err(err) = proxy.set_brightness("backlight", &device.name, raw).await {
            eprintln!("brightness: SetBrightness(backlight, {}, {raw}) failed: {err}", device.name);
        }
        // No optimistic local update: the real state update happens off the udev watch/poll
        // loop, the same "state changes flow through the signal, not the write call" shape
        // `KeyboardController::set_backlight`'s own doc comment already established.
    }
}

/// Reads the initial percent before entering either wait mode (a config needs a value
/// immediately, not after the first watch event or poll tick -- matches
/// `battery::controller::run_battery_task`'s own "initial read, then the event loop" ordering),
/// then hands off to [`run_brightness_watch_loop`], the real udev-driven primary path. Degrades
/// to [`run_brightness_poll_loop`] only if [`build_backlight_watch`] itself fails to stand up.
/// Unlike `battery`, this task is only ever spawned when [`select_backlight_device`] already
/// found a usable device -- so unlike battery's "no hardware" case, there is no "keep running and
/// stay at the default forever" branch to reach here at all.
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

/// Builds the real `backlight` subsystem udev watch, the same construction battery's own
/// `build_power_supply_watch` uses (`MonitorBuilder` filtered to one subsystem, `listen()`ed,
/// registered against tokio's IO driver via `AsyncFd`) -- see that function's own doc comment
/// for the `udev` `send`-feature mechanics this needs to typecheck at all.
///
/// This corrects docs/build-steps.md line 98's prediction, which justified the `inotify`
/// dependency for "§ 1.2's backlight watch": inotify does not fire reliably on a sysfs attribute
/// write. `keyboard::locks`'s own doc comment already recorded this for the LED-state files
/// (confirmed there by toggling Caps Lock under `inotifywait -m` and seeing nothing); the same
/// failure was independently confirmed here for backlight, using `udevadm monitor --udev
/// --subsystem-match=backlight` while changing brightness -- that command shows a `change` uevent
/// on the `backlight` subsystem, which is the mechanism this watch actually uses.
fn build_backlight_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("backlight")?.listen()?;
    AsyncFd::new(socket)
}

/// The primary path: awaits the udev `backlight` watch's fd becoming readable, drains every
/// pending netlink message, then re-reads `percent` off `device_dir` and only writes `state`/
/// sends [`BrightnessSignal::Changed`] when it actually differs from `previous` -- same shape as
/// `battery::controller::run_battery_watch_loop`, including its `readable_mut` (not `readable`)
/// requirement (see that function's own doc comment for why: only `udev`'s `send` feature is
/// enabled, not `sync`).
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

/// The fallback path (see [`POLL_INTERVAL`]'s own doc comment for the two ways this gets reached
/// instead of [`run_brightness_watch_loop`]): re-reads on a fixed timer instead of a real event,
/// same push-on-change filter as the primary path.
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

        // No device means `new` never spawns the read task, so the `events` sender it was
        // handed is simply dropped at the end of `new` -- `recv` returns `None` (channel
        // closed) rather than ever seeing a `Changed`, and it does so immediately rather than
        // hanging, so this assertion doesn't need a generous timeout the way a genuine "nothing
        // happens" wait would.
        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(None), "no device means no signal is ever sent, not even the default state");
        assert_eq!(controller.snapshot(), BrightnessState::default());
    }
}
