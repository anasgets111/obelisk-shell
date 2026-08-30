mod audio;
mod dbus;
mod hardware;
mod lock;
mod memory;
mod pam_worker;
mod privacy;
mod process;
mod reload;
mod reload_link;
mod snapshot;
mod socket;
mod system;
mod updates;
mod watcher;
mod workspaces;

use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use dbus::bluetooth::{self, BluetoothController, BluetoothSignal};
use dbus::network::{self, NetworkController, NetworkSignal};
use dbus::notifications::{self, NotificationsController, NotificationsSignal};
use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use dbus::power::{self, PowerController, PowerSignal};
use dbus::tray::{self, TrayController, TraySignal};
use hardware::battery::{BatteryController, BatterySignal};
use hardware::brightness::{self, BrightnessController, BrightnessSignal};
use hardware::idle::{self, IdleController};
use hardware::keyboard::{self, KeyboardController, KeyboardSignal};
use hardware::sysinfo::{self, SysinfoController, SysinfoSignal};
use lock::LockController;
use privacy::{PrivacyController, PrivacySignal};
use system::{SystemController, SystemSignal};
use updates::{UpdatesController, UpdatesSignal};
use workspaces::{WorkspacesController, WorkspacesSignal};
use process::registry::{LiveProcesses, reap_all_processes, reap_generations_processes, take_exited_process, wait_and_report_exit};
use reload_link::SocketCandidateLink;
use shared::{
    ApplyPendingReload, DeselectInput, PromoteGeneration, RendererFrame, ReevaluateReport, ReevaluateRequest, SupervisorFrame, Zeroize,
};
use snapshot::push_snapshot;

/// How long the Watcher waits after the *last* relevant `shell.lua` change before dispatching a
/// reload -- coalesces an editor's multi-event save into a single round trip. Fixed, not
/// configurable (docs/adr/0024 item 6).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// § 15.2/15.3's ready-signal and evidence-verification deadlines (`reload::PbaTimings`).
/// Seconds, not minutes, matching `reload.rs`'s own test constants' order of magnitude scaled up
/// for a real Candidate that has to actually bind Wayland/EGL rather than a fake resolving
/// immediately -- generous enough that a healthy Candidate never trips them, tight enough that a
/// wedged one doesn't leave a config edit hanging for a long time. `reap_grace` reuses
/// `process::DEFAULT_REAP_GRACE`, this constant's first real caller alongside `main`'s own
/// superseded-generation reap below.
const PBA_TIMINGS: reload::PbaTimings =
    reload::PbaTimings { ready_timeout: Duration::from_secs(2), evidence_timeout: Duration::from_secs(3), reap_grace: process::DEFAULT_REAP_GRACE };

/// Whether an `Unchanged` report's `sequence` still names the most recently sent `Reevaluate`
/// (`next_sequence`). A mismatch means a newer `Reevaluate` has already been sent for this
/// generation since this report's request went out (the debounced watcher fired again before
/// this round trip completed) -- the go-ahead must not be sent for a superseded evaluation
/// (Correctness review, docs/adr/0024 item 2).
fn is_current_reload(report_sequence: u64, next_sequence: u64) -> bool {
    report_sequence == next_sequence
}

/// Starts one reload cycle on `generation_id`: bump the sequence this Supervisor owns and send
/// the `Reevaluate` carrying it. Everything downstream -- the Renderer's own topology diff, the
/// `Unchanged`/`TopologyChanged`/`Failed` verdict, and the in-place-versus-swap decision this
/// file makes from it -- is unchanged (docs/adr/0024, docs/adr/0041 decision 4).
///
/// Two triggers reach it, and that is the only reason it is a function rather than two inline
/// statements: the `inotify` watcher's debounced file change, and (since docs/adr/0041 decision 4)
/// a Renderer's `RendererFrame::RequestReload` after a `wl_output` appeared or disappeared. The
/// sequence stays here in both cases -- `is_current_reload` above rejects any report that does not
/// name the most recently sent one, so a Renderer that fabricated its own would have its report
/// dropped as stale.
fn begin_reload(registry: &socket::GenerationRegistry, generation_id: u32, next_sequence: &mut u64) {
    *next_sequence += 1;
    send_frame_logged(registry, generation_id, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: *next_sequence }));
}

/// Sends `frame` to `generation_id`, logging (not propagating) a failure. The one place every
/// `SupervisorFrame` send in this file's main loop goes through -- previously each call site
/// either hand-wrote its own `if let Err(err) = ... { eprintln!(...) }` (duplicated three times)
/// or, for the Swap messages, silently discarded the `Result` with `let _ =` entirely (Standards
/// + Correctness review: the only sends in this function whose failure went unlogged).
///
/// A `NoConnection` failure here is logged and dropped, not retried -- still possible for a
/// one-off frame sent to a generation that's simply disconnected. The boot-time version of this
/// (`network`'s/`bluetooth`'s controllers can start pushing before the boot Renderer's connection
/// even exists, since they're constructed before `socket::spawn_listener` runs) no longer loses
/// state permanently: the `connected.recv()` arm in `run_supervisor`'s main loop replays
/// `last_snapshots` to a generation the instant it registers, so anything captured before that
/// point still arrives.
fn send_frame_logged(registry: &socket::GenerationRegistry, generation_id: u32, frame: &SupervisorFrame) {
    if let Err(err) = registry.send_frame(generation_id, frame) {
        eprintln!("failed to push {frame:?} to generation {generation_id}: {err}");
    }
}

/// The one malformed-arguments log line every capability's `dispatch` adapter shares (ADR-0037)
/// -- same wording the old per-arm `eprintln!`s each hand-rolled.
pub(crate) fn log_malformed_command(params: &shared::CommandParams) {
    eprintln!(
        "malformed {}.{} command from generation {}: {:?}",
        params.capability, params.action, params.generation_id, params.arguments
    );
}

/// [`log_malformed_command`]'s sibling for a `dispatch` adapter's unmatched-action fallback --
/// the capability name comes from the envelope, not a hand-typed prefix.
pub(crate) fn log_unknown_action(params: &shared::CommandParams) {
    eprintln!("{}: unknown action {:?} from generation {}", params.capability, params.action, params.generation_id);
}

/// Resolves the Renderer binary's path as a sibling of the currently-running Supervisor binary
/// (`Path::with_file_name` swaps the last path component, i.e. `target/{profile}/supervisor` ->
/// `target/{profile}/renderer` -- the standard same-workspace cargo layout). No packaging or
/// install-path configuration exists yet (docs/adr/0025) -- this assumption is the only one
/// available until one does.
fn renderer_binary_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("renderer"))
}

/// One generation's identity and process handle while it's authoritative. Reassigned wholesale
/// on a successful swap -- real generation-ID assignment tied to process spawning (this phase)
/// replaces the old hardcoded `RENDERER_GENERATION_ID` constant every prior phase used.
struct Authoritative {
    generation_id: u32,
    child: tokio::process::Child,
}

/// How the authoritative Renderer's process ended (docs/adr/0058 decision 2).
///
/// Three variants rather than a bare `ExitStatus` because the message a human needs differs by
/// variant, and because `Clean` is not a crash: `main()`'s own shutdown reaps the Renderer and that
/// reap must never be reported as a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendererDeparture {
    Clean,
    Failed { code: i32 },
    Signalled { signal: i32 },
}

/// Signal before code, and the order is the decision: a signalled child has no exit code at all, so
/// asking `code()` first returns `None` and throws away the only fact that says what happened.
fn classify_departure(status: std::process::ExitStatus) -> RendererDeparture {
    use std::os::unix::process::ExitStatusExt;

    if let Some(signal) = status.signal() {
        return RendererDeparture::Signalled { signal };
    }
    match status.code() {
        Some(0) => RendererDeparture::Clean,
        Some(code) => RendererDeparture::Failed { code },
        // Neither a code nor a signal is not a shape `wait(2)` produces on Linux. Named rather than
        // left to an `unreachable!()`, because this runs on the path that handles a crash and a
        // panic here would answer a dead Renderer by killing the process that can still recover it.
        None => RendererDeparture::Failed { code: -1 },
    }
}

/// The line a human reads when the Renderer goes away, carrying whether a lock was live at the time
/// (docs/adr/0058 decision 2).
///
/// The lock clause is the point. A Renderer that dies unlocked costs a bar and the log can be read
/// tomorrow. One that dies holding `ext_session_lock_v1` costs the session now: the compositor is
/// required not to unlock when a lock client dies, so nothing short of a replacement taking the
/// lock over, or a VT switch, gets the user back in.
fn departure_report(departure: RendererDeparture, generation_id: u32, lock_active: bool) -> String {
    let what = match departure {
        RendererDeparture::Clean => "exited cleanly".to_string(),
        RendererDeparture::Failed { code } => format!("exited with code {code}"),
        RendererDeparture::Signalled { signal } => format!("was killed by signal {signal}"),
    };
    let lock = if lock_active {
        ", and it held the session lock: the compositor does not unlock when a lock client dies, so the session stays \
         locked until a replacement takes the lock over (docs/adr/0058)"
    } else {
        ""
    };
    format!("generation {generation_id}'s renderer {what}{lock}")
}


/// Branches into the PAM worker's own minimal, tokio-free code path (ADR-0028) before falling
/// through to the normal Supervisor. This must run *before* any D-Bus/tokio-runtime/audio-thread
/// setup -- `run_supervisor`'s `tokio::runtime::Runtime::new()` (equivalent to what
/// `#[tokio::main]`'s default multi-thread flavor built) must not even be constructed on the
/// worker path.
fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("OBLISK_PAM_WORKER").is_some() {
        return pam_worker::run_worker();
    }
    tokio::runtime::Runtime::new()?.block_on(run_supervisor())
}

async fn run_supervisor() -> Result<(), Box<dyn Error>> {
    let connection = zbus::Connection::system().await?;
    let subject = current_session_subject()?;

    let (tx, mut challenges) = tokio::sync::mpsc::unbounded_channel();
    let agent = AuthenticationAgent::new(tx);
    register_agent(&connection, agent, &subject, "en_US.UTF-8", AGENT_OBJECT_PATH).await?;
    // Long-lived proxy for responding to polkitd later, once a pending challenge's PAM
    // conversation finishes (docs/adr/0028) -- built once here rather than per-challenge.
    let authority = zbus_polkit::policykit1::AuthorityProxy::new(&connection).await?;

    // NetworkManager runs on the same system bus as polkit -- reuses `connection` rather than
    // opening a second one (build-steps.md Phase 16; docs/adr/0029).
    //
    // This and `BluetoothController::new` below both construct (and can start hydrating/pushing)
    // before `socket::spawn_listener` runs further down -- any push that lands before the boot
    // Renderer's connection registers gets replayed once it does, by the `connected.recv()` arm
    // in this function's main loop (see `send_frame_logged`'s doc comment).
    // The channel is threaded into the constructor (ADR-0037, matching BluetoothController's
    // shape below): the controller spawns the Wi-Fi signal forwarder itself and keeps a sender
    // clone alive even when there's no Wi-Fi device, so the channel never closes (a closed
    // channel would make `.recv()` resolve to `None` on every poll below, busy-looping that arm
    // instead of idling).
    let (network_signal_tx, mut network_signals) = tokio::sync::mpsc::unbounded_channel::<NetworkSignal>();
    let network = NetworkController::new(connection.clone(), network_signal_tx).await?;

    // BlueZ runs on the same system bus too (docs/adr/0030). The channel is threaded into the
    // constructor -- see BluetoothController::new's own doc comment for why (startup hydration
    // itself needs it, not just the forwarders spawned after construction).
    let (bluetooth_signal_tx, mut bluetooth_signals) = tokio::sync::mpsc::unbounded_channel::<BluetoothSignal>();
    let bluetooth = BluetoothController::new(connection.clone(), bluetooth_signal_tx).await;

    // The tray host is a *session*-bus protocol (org.kde.StatusNotifierItem/Watcher and
    // com.canonical.dbusmenu are session-bus conventions by construction -- every real tray item,
    // nm-applet/Discord/Slack/etc., registers there, never on the system bus): docs/oblisk-
    // supervisor-services-dbus.md Sec.2 and this module's own doc comment. Unlike NetworkManager/
    // BlueZ/polkit above, which are genuine system-bus services and correctly share `connection`,
    // the tray host needs its own, separate session-bus connection -- reusing the system-bus
    // `connection` here would silently make it watch a bus no real tray item ever registers on.
    // Mirrors BluetoothController's own "thread the channel into the constructor" shape (not a
    // separate `*_signal_source()` getter) -- item hydration at registration time needs it
    // immediately, same reasoning as BlueZ's device registry. A session bus genuinely not being
    // available in some environment degrades to `TrayController::inert` rather than aborting
    // Supervisor boot -- same "degrade to inert" precedent `BluetoothController::new`/
    // `TrayController::new` already established for a missing daemon/lost `RequestName` race.
    let (tray_signal_tx, mut tray_signals) = tokio::sync::mpsc::unbounded_channel::<TraySignal>();
    let tray = match zbus::Connection::session().await {
        Ok(tray_connection) => TrayController::new(tray_connection, tray_signal_tx).await,
        Err(err) => {
            eprintln!("tray: failed to connect to the session bus; tray host disabled for this run: {err}");
            TrayController::inert(tray_signal_tx)
        }
    };

    let (audio_tx, mut audio_apps) = tokio::sync::mpsc::unbounded_channel();
    // `video_tx` feeds `oblisk.privacy`'s PipeWire name-enrichment (docs/adr/0034) -- the same
    // registry thread, one PipeWire connection, not a second one.
    let (video_tx, video_sources) = tokio::sync::mpsc::unbounded_channel();
    // pipewire-rs's event loop is Rc-based and single-threaded (not Send) -- it needs its
    // own OS thread, not a tokio task.
    // The write half of § 3.2's audio actions. A `pipewire::channel` rather than a controller
    // handle, because every proxy the mixer thread holds is `!Send` and that thread is inside a
    // blocking `main_loop.run()`; the channel hands the loop an eventfd to poll beside its own
    // sources. See `audio::mixer::AudioCommand`.
    let (audio_commands, audio_command_rx) = audio::mixer::command_channel();
    std::thread::spawn(move || audio::mixer::run(audio_tx, video_tx, audio_command_rx));

    // Notifications (docs/oblisk-supervisor-services-dbus.md §1; ADR-0033): its own, separate
    // session-bus connection, independent of tray's -- a real desktop might already run mako/dunst
    // owning org.freedesktop.Notifications, a genuine "someone else already provides this" outcome
    // this controller degrades to inert for (`RequestName`'s `DoNotQueue` flag), not a dual-role
    // dance like tray's. The sound-playback thread is a second, unrelated "own OS thread" for the
    // same "pipewire-rs's loop is `!Send`" reason as `audio::mixer::run` above -- a different
    // pipewire-rs API surface (a playback `pw::stream::Stream`, not a registry listener), so it
    // gets its own dedicated thread rather than sharing the mixer's.
    let (sound_tx, sound_rx) = std::sync::mpsc::channel::<PathBuf>();
    std::thread::spawn(move || notifications::run_sound_player(sound_rx));
    let (notifications_signal_tx, mut notifications_signals) = tokio::sync::mpsc::unbounded_channel::<NotificationsSignal>();
    let notifications = match zbus::Connection::session().await {
        Ok(notifications_connection) => NotificationsController::new(notifications_connection, notifications_signal_tx, sound_tx.clone()).await,
        Err(err) => {
            eprintln!("notifications: failed to connect to the session bus; notifications server disabled for this run: {err}");
            NotificationsController::inert(notifications_signal_tx, sound_tx.clone())
        }
    };

    // MPRIS (docs/oblisk-supervisor-services-dbus.md §3; ADR-0036): its own, separate session-bus
    // connection, same reasoning as tray/notifications above -- a session-bus protocol, distinct
    // from every system-bus controller. `MprisController::new` is not `async` (unlike
    // `TrayController::new`/`NotificationsController::new`): it spawns discovery as a background
    // task and returns immediately, same shape as `IdleController`/`SysinfoController`.
    let (mpris_signal_tx, mut mpris_signals) = tokio::sync::mpsc::unbounded_channel::<dbus::mpris::MprisSignal>();
    let mpris = match zbus::Connection::session().await {
        Ok(mpris_connection) => dbus::mpris::MprisController::new(mpris_connection, mpris_signal_tx),
        Err(err) => {
            eprintln!("mpris: failed to connect to the session bus; player discovery disabled for this run: {err}");
            dbus::mpris::MprisController::inert(mpris_signal_tx)
        }
    };

    let socket_path = shared::control_socket_path()?;
    let (registry, mut inbound_frames, mut connected) = socket::spawn_listener(&socket_path)?;

    // Idle capability (docs/oblisk-supervisor-services-dbus.md §7; ADR-0032): notify rides its
    // own dedicated Wayland connection (ADR-0010's sibling to lock authority -- idle authority
    // must survive a Renderer crash or reload too), inhibit rides the same system-bus
    // `connection` NetworkManager/BlueZ/polkit already share (no new connection, no degrade path
    // -- ADR-0032). Constructed after `socket::spawn_listener`, not before: `IdleController::new`
    // itself returns immediately (notify setup runs in its own `spawn_blocking`-wrapped,
    // timeout-bounded background task -- see `hardware::idle`'s module doc comment for the live-
    // observed hang this defends against), but this ordering is kept as a second, independent
    // guarantee that a future change to that constructor can't silently reintroduce a control-
    // socket-blocking boot dependency. Unlike tray, there's no fallible outer `match` here since
    // inhibit always constructs successfully against an already-established connection.
    let (idle_signal_tx, mut idle_signals) = tokio::sync::mpsc::unbounded_channel::<shared::IdleEvent>();
    let idle = IdleController::new(connection.clone(), idle_signal_tx).await;

    // sysinfo capability (docs/oblisk-supervisor-services-dbus.md §11; docs/oblisk-hardware-
    // event-pipeline.md §7; docs/adr/0035): three independently-configurable, watch-driven
    // poll tasks, all starting dormant -- nothing polls `/proc`/`/sys` until Lua calls
    // `sysinfo:configure` at least once. No D-Bus connection, no degrade-to-inert path (unlike
    // tray/notifications/idle-notify above) -- pure sysfs/procfs parsing always succeeds at
    // construction time regardless of what the real filesystem happens to expose.
    let (sysinfo_signal_tx, mut sysinfo_signals) = tokio::sync::mpsc::unbounded_channel::<SysinfoSignal>();
    let sysinfo = SysinfoController::new(PathBuf::from("/proc"), PathBuf::from("/sys/class/hwmon"), sysinfo_signal_tx);

    // keyboard capability (docs/adr/0034, as corrected against this dev machine's real UPower
    // introspection and real evdev/sysfs lock-state behavior): backlight and lock state (caps/
    // num/scroll) are both wired in; backlight rides the shared system-bus `connection`
    // NetworkManager/BlueZ/polkit/idle-inhibit already share (no new connection), lock state
    // resolves evdev primary with a sysfs fallback under `leds_root`. No degrade-to-inert path
    // needed -- a missing `KbdBacklight` object degrades in place to `backlight_pct: -1`, and a
    // missing lock-state source degrades in place to `false`, both inside `KeyboardController::
    // new` itself, not a fallible outer `match`.
    let (keyboard_signal_tx, mut keyboard_signals) = tokio::sync::mpsc::unbounded_channel::<KeyboardSignal>();
    let keyboard = KeyboardController::new(connection.clone(), &PathBuf::from("/sys/class/leds"), keyboard_signal_tx).await;

    // privacy capability (docs/adr/0034): kernel-level /dev/videoN opener detection via inotify
    // OPEN/CLOSE plus a /proc fd-scan, enriched by `video_sources` (the mixer thread's
    // Video/Source feed constructed above). No D-Bus, no degrade-to-inert path -- an empty
    // /sys/class/video4linux (no camera hardware) degrades in place to an empty camera_users.
    let (privacy_signal_tx, mut privacy_signals) = tokio::sync::mpsc::unbounded_channel::<PrivacySignal>();
    let privacy = PrivacyController::new(PathBuf::from("/proc"), &PathBuf::from("/sys/class/video4linux"), video_sources, privacy_signal_tx);

    // updates capability (docs/adr/0034): `alpm`-based Arch update checking and installation,
    // fully separate from sysinfo's own scheduler (same interval-suspend-at-zero shape, zero
    // shared code -- the ADR's own instruction). No D-Bus, no degrade-to-inert path -- a missing/
    // unparseable /etc/pacman.conf degrades in place to an empty repo list, surfaced as
    // `check_error` on the first check rather than failing construction.
    let (updates_signal_tx, mut updates_signals) = tokio::sync::mpsc::unbounded_channel::<UpdatesSignal>();
    let updates = UpdatesController::new(PathBuf::from("/etc/pacman.conf"), PathBuf::from("/var/lib/pacman"), updates_signal_tx);

    // battery capability (docs/adr/0053, § 2.2): `/sys/class/power_supply`, filtered to the one
    // system battery. The root is a parameter rather than a constant for the same reason
    // `PrivacyController`'s is -- it is what makes the device-selection rule testable against a
    // fixture directory instead of against whatever hardware the test machine happens to have.
    let (battery_signal_tx, mut battery_signals) = tokio::sync::mpsc::unbounded_channel::<BatterySignal>();
    let battery = BatteryController::new(PathBuf::from("/sys/class/power_supply"), battery_signal_tx);

    // brightness capability (docs/adr/0053, § 2.3): `/sys/class/backlight`, ranked by `type`
    // (firmware over platform over raw), read via a udev `backlight` subsystem watch, written
    // through `org.freedesktop.login1.Session.SetBrightness` on the shared system-bus
    // `connection` NetworkManager/BlueZ/polkit/idle-inhibit/keyboard already share (no new
    // connection). No device found degrades in place to never pushing at all -- see
    // `hardware::brightness`'s own module doc comment for why that's the correct answer for a
    // spec with no absence sentinel, not a bug.
    let (brightness_signal_tx, mut brightness_signals) = tokio::sync::mpsc::unbounded_channel::<BrightnessSignal>();
    let brightness = BrightnessController::new(PathBuf::from("/sys/class/backlight"), connection.clone(), brightness_signal_tx);

    // workspaces capability (docs/adr/0056, § 2.9): niri's IPC event stream, reduced through
    // `niri_ipc::state`'s own two state parts. No sysfs root and no D-Bus connection to inject --
    // the socket path comes from `$NIRI_SOCKET`, which niri sets for every process in its own
    // session, so there is nothing here for a test to point somewhere else (the mapping is a pure
    // function and is tested directly instead). A session that is not niri never pushes at all.
    let (workspaces_signal_tx, mut workspaces_signals) = tokio::sync::mpsc::unbounded_channel::<WorkspacesSignal>();
    let workspaces = WorkspacesController::new(workspaces_signal_tx);

    // power capability (§ 2.13, docs/adr/0053's own `power` amendment): UPower for `on_battery`
    // and `energy_rate`, power-profiles-daemon for `active_profile` and `profiles`, both on the
    // shared system-bus `connection`. Either service can be missing and the other still reports:
    // every field is optional and an unanswerable one is omitted rather than filled in.
    let (power_signal_tx, mut power_signals) = tokio::sync::mpsc::unbounded_channel::<PowerSignal>();
    let power = PowerController::new(connection.clone(), power_signal_tx);

    // system capability (docs/adr/0053, § 2.11): the 1 Hz clock a config needs to draw a time that
    // moves, plus the persisted `state.json` dictionary. Both env reads happen here rather than
    // inside the module, matching how `resolve_state_path` was written to take them as arguments.
    // A missing `$HOME` degrades to `/` instead of failing the boot: it makes `state` empty, which
    // is already the correct answer for a first run, and a shell that refuses to start because it
    // could not find a preferences file it does not need would be the worse trade.
    let (system_signal_tx, mut system_signals) = tokio::sync::mpsc::unbounded_channel::<SystemSignal>();
    let system = SystemController::new(
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/"))),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        system_signal_tx,
    );

    // lock capability (docs/adr/0042, docs/adr/0052): the Renderer holds `ext_session_lock_v1`
    // and paints it; this side owns the decision to take it, the state a lock screen reads, and
    // the one call site allowed to order an unlock. No D-Bus and no hardware, so no degrade path
    // -- the whole capability is a state machine plus a channel. The channel exists because the
    // controller must not cache the authoritative generation id (a swap reassigns it): each
    // `SetSessionLock` comes back to this loop to be addressed.
    let (lock_command_tx, mut lock_commands) = tokio::sync::mpsc::unbounded_channel::<shared::SetSessionLock>();
    let lock = LockController::new(lock_command_tx);
    // How a spawned lock-screen PAM conversation's answer gets back into this loop, the same
    // shape every controller signal above already uses. See the `secure_submit(lock,
    // authenticate)` arm for why that one conversation is spawned rather than `.await`ed inline
    // like polkit's. `pam_outcome_tx` is kept here for the whole run, so the channel never closes
    // and this arm never busy-loops on a `None`.
    // Each outcome carries the acquisition the conversation was admitted against, because a PAM
    // answer outlives the lock it answers for -- see `lock::accepts_outcome`.
    let (pam_outcome_tx, mut pam_outcomes) = tokio::sync::mpsc::unbounded_channel::<(u64, shared::PamOutcome)>();

    let config_dir = shared::config_dir()?;
    let mut reload_events = watcher::spawn_watcher(&config_dir, RELOAD_DEBOUNCE)?;
    let mut next_sequence: u64 = 0;

    // Generation 0 is boot-spawned by the Supervisor itself, for the first time (docs/adr/0025
    // item 7) -- there is no shell without a Generation 0, so a spawn failure here is fatal to
    // `main`.
    let renderer_path = renderer_binary_path()?;
    let renderer_path_str = renderer_path.to_string_lossy().into_owned();
    let boot_child =
        process::spawn_group_leader(&renderer_path_str, &[], &[("OBLISK_GENERATION_ID".to_string(), "0".to_string())])?;
    let mut authoritative = Authoritative { generation_id: 0, child: boot_child };
    let mut next_generation_id: u32 = 1;

    // The last `StateSnapshot` pushed for each capability to the authoritative generation, keyed
    // by capability name -- reused to hydrate a fresh Candidate's first evaluation with every
    // capability's latest known state (§ 15.2 point 1; docs/adr/0029 generalizes the old
    // single-slot `last_audio_snapshot: Option<StateSnapshot>` now that a second capability
    // (`network`) pushes `StateSnapshot`s too). A capability with no entry yet simply isn't
    // pushed to a fresh Candidate -- there's nothing to hydrate it with.
    let mut last_snapshots: HashMap<String, shared::StateSnapshot> = HashMap::new();
    // Every capability's own state-version counter (ADR-0004), keyed by name -- generalizes the
    // pre-Phase-16 single `audio_revision: u32` now that a second capability (`network`) pushes
    // `StateSnapshot`s too (docs/adr/0029).
    let mut revisions: HashMap<String, u32> = HashMap::new();

    // The most recently received polkit challenge awaiting a secure_submit(polkit, authenticate)
    // reply, if any -- same "only the most recent one matters" simplification as
    // `last_audio_snapshot`, for the same reason: this codebase has no multi-challenge queue or
    // UI to disambiguate between concurrent auth prompts, so keeping only the latest is the
    // correct minimal behavior, not a missing feature.
    let mut pending_challenge: Option<dbus::polkit::BeginAuthenticationCall> = None;

    // Every `process.run`-spawned child still tracked (docs/adr/0026), plus the channel
    // `stream_process_output`'s background tasks use to report a naturally-exited process back to
    // this loop for reaping and registry cleanup.
    // Whether a topology-changing reload was refused while the session was locked and still has
    // to run once it clears (docs/adr/0042: only one client may hold a session lock, so a
    // candidate cannot acquire the one the authoritative generation holds). A bool, not a queue:
    // a second topology change while locked is still one reload to run on unlock.
    let mut swap_owed_on_unlock = false;

    let mut processes: LiveProcesses = HashMap::new();
    let (process_done_tx, mut process_done) = tokio::sync::mpsc::unbounded_channel::<(u32, u64)>();
    // No handler at all previously meant Ctrl-C killed the Supervisor on the spot, leaving the
    // Renderer (a different process group by design, Phase 7) orphaned and running headless --
    // confirmed live. `SIGTERM` gets the same treatment: a process manager stopping this unit
    // sends `SIGTERM`, not `SIGINT`.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let mut memory_sampler = memory::sampler_from_env();
    // Set only by the departure arm below, so the shutdown reap can tell "the Renderer is still
    // running and needs reaping" from "it is already gone and `wait` has already collected it".
    let mut renderer_departed = false;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("SIGINT received, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                eprintln!("SIGTERM received, shutting down");
                break;
            }
            // docs/adr/0058 decision 1. The one process this whole Supervisor exists to feed was
            // the only thing here nobody watched, and the failure is quiet by construction: a dead
            // Renderer sends no frames, and a healthy idle one sends no frames either. Without this
            // arm the difference never reaches the `select!` at all. It surfaces minutes later as
            // every push failing with "no connection registered for generation 0", by which point
            // the log says what is broken but not that anything broke.
            status = authoritative.child.wait() => {
                let departure = match status {
                    Ok(status) => classify_departure(status),
                    Err(err) => {
                        eprintln!("failed to wait on generation {}'s renderer: {err}", authoritative.generation_id);
                        RendererDeparture::Failed { code: -1 }
                    }
                };
                eprintln!("{}", departure_report(departure, authoritative.generation_id, lock.snapshot().active));
                renderer_departed = true;
                // Shutting down is the honest interim, not the destination: docs/adr/0058
                // decision 3 replaces this `break` with a braked respawn. A Supervisor with no
                // Renderer can do nothing except fail every push it is handed, so ending here and
                // freeing the socket beats spinning on capability channels nobody will read.
                break;
            }
            Some(_) = memory::tick_sampler(&mut memory_sampler) => {
                memory::log_sample("steady state", &[(authoritative.generation_id, &authoritative.child)]);
            }
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
                pending_challenge = Some(challenge);
            }
            Some(generation_id) = connected.recv() => {
                // Only the authoritative generation needs a replay: a PBA candidate's connection
                // (a different `generation_id`) already gets its hydration explicitly from
                // `run_pba`'s own `snapshots` argument (docs/adr/0029), and replaying here too
                // would just be redundant, not wrong -- restricting to authoritative keeps this
                // arm from doing anything during a live PBA handshake it isn't part of.
                //
                // Fixes the boot-time race `send_frame_logged`'s doc comment used to describe:
                // `network`'s/`bluetooth`'s controllers can start pushing before this connection
                // exists; whatever they'd already captured in `last_snapshots` by the time it
                // registers gets delivered right now instead of staying lost until some
                // unrelated later event happens to push a fresh snapshot.
                if generation_id == authoritative.generation_id {
                    for snapshot in last_snapshots.values() {
                        send_frame_logged(&registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
                    }
                }
            }
            Some(apps) = audio_apps.recv() => {
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "audio", &apps);
            }
            Some(signal) = network_signals.recv() => {
                // The controller owns the signal's state semantics (ADR-0037) -- this arm only
                // pushes whatever state the signal produced.
                let state = network.handle_signal(signal).await;
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "network", &state);
            }
            Some(signal) = bluetooth_signals.recv() => {
                let state = bluetooth.handle_signal(signal).await;
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "bluetooth", &state);
            }
            Some(TraySignal::RegistryChanged) = tray_signals.recv() => {
                // No debounce (docs/adr/0031, matching ADR-0029/0030): the registry entry
                // driving this signal is already fully recomputed by the forwarder task that
                // sent it (see dbus::tray's module doc comment) -- build_state is a synchronous
                // snapshot of already-live data, no further D-Bus round trip needed here.
                let tray_state = tray.build_state();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "tray", &tray_state);
            }
            Some(dbus::mpris::MprisSignal::Changed) = mpris_signals.recv() => {
                // No debounce (ADR-0036, matching tray/bluetooth/network's own precedent): every
                // registry entry driving this signal is already fully recomputed by its own
                // forwarder task before the signal was sent -- build_state is a synchronous
                // snapshot of already-live data, no further D-Bus round trip needed here.
                let mpris_state = mpris.build_state();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "mpris", &mpris_state);
            }
            Some(NotificationsSignal::Changed) = notifications_signals.recv() => {
                // No debounce (ADR-0033, matching ADR-0029/0030/0031): every mutation (Notify,
                // dismiss, reply, set_dnd, expiry firing, FIFO eviction) fully re-derives
                // notifications.feed/notifications.dnd from already-live state -- build_state is a
                // synchronous snapshot, no further D-Bus round trip needed here.
                let notifications_state = notifications.build_state();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "notifications", &notifications_state);
            }
            Some(event) = idle_signals.recv() => {
                // Unlike tray/network/bluetooth (always pushed to the single authoritative
                // generation), an idle event is routed to whichever generation actually made the
                // `register_threshold` call that's now firing (`event.generation_id`) -- ADR-0006's
                // "a registration belongs to the generation that made it" framing, and the only
                // sensible target when a superseded (non-authoritative) generation still holds a
                // live threshold registration. Dispatched straight as an `IdleEvent`, not through
                // the `StateSnapshot`/`revision` signal-table path (ADR-0032: idle is
                // event-shaped, not pollable state). `hardware::idle` already builds `shared::IdleEvent`
                // directly (no intermediate signal type to relabel -- Standards review), so this
                // arm just routes it.
                send_frame_logged(&registry, event.generation_id, &SupervisorFrame::IdleEvent(event));
            }
            Some(SysinfoSignal::Changed) = sysinfo_signals.recv() => {
                // No debounce (matching tray/network/bluetooth/notifications' own precedent):
                // whichever of the three tasks (cpu; ram+swap; temp_cores+temp_gpu) just
                // ticked already wrote its own field(s) into the shared state under its own
                // lock -- this arm only needs to clone the current combined state and push it
                // (docs/adr/0035).
                let state = sysinfo.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "sysinfo", &state);
            }
            Some(KeyboardSignal::Changed) = keyboard_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo above -- the backlight
                // forwarder already wrote `backlight_pct` under its own lock before signaling.
                let state = keyboard.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "keyboard", &state);
            }
            Some(BatterySignal::Changed) = battery_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo/keyboard below.
                let state = battery.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "battery", &state);
            }
            Some(BrightnessSignal::Changed) = brightness_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo/keyboard/battery above --
                // this arm only fires at all when a backlight device was found (docs/adr/0053).
                let state = brightness.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "brightness", &state);
            }
            Some(WorkspacesSignal::Changed) = workspaces_signals.recv() => {
                // Same no-debounce, full-re-derive shape as the arms above -- the controller
                // already filters to real changes, since niri's event stream reports plenty this
                // capability's payload does not carry (urgency, per-window layout geometry).
                let state = workspaces.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "workspaces", &state);
            }
            Some(PowerSignal::Changed) = power_signals.recv() => {
                // Same no-debounce, full-re-derive shape as the arms above. UPower re-emits
                // `EnergyRate` on its own cadence (roughly once a minute on this machine) and the
                // controller filters that down to real changes before it ever reaches here.
                let state = power.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "power", &state);
            }
            Some(SystemSignal::Changed) = system_signals.recv() => {
                // Once per wall-clock second, and the only capability here that pushes on a timer
                // rather than on a real event -- docs/adr/0053 decision 2 owns why that cost is
                // taken and what bounds it (the controller emits only when the epoch second it
                // would report actually changed).
                let state = system.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "system", &state);
            }
            Some(PrivacySignal::Changed) = privacy_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo/keyboard above.
                let state = privacy.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "privacy", &state);
            }
            Some(UpdatesSignal::Changed) = updates_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo/keyboard/privacy above --
                // fires after both a periodic check and an install's progress updates.
                let state = updates.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "updates", &state);
            }
            Some(command) = lock_commands.recv() => {
                // The only place a `SetSessionLock` is addressed, because this is the only holder
                // of the authoritative generation id. The state push rides along: every command
                // this capability sends is also a state change a lock screen has to see
                // (docs/adr/0052 decision 4).
                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::SetSessionLock(command));
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
            }
            Some((acquisition, outcome)) = pam_outcomes.recv() => {
                // The other half of the `secure_submit(lock, authenticate)` arm below: the one
                // place a `Success` becomes an unlock order, which is what keeps docs/adr/0042's
                // "never call `unlock_and_destroy` except on a successful authentication" a
                // property of a single call site even though the conversation itself now runs off
                // this loop.
                //
                // `acquisition` is what makes that property mean anything. An answer takes about
                // a second to come back and may take `PAM_EXCHANGE_TIMEOUT`'s thirty, and inside
                // that window the compositor can end the lock the password was typed against
                // (`finished` after `locked`, which is what `loginctl unlock-session` produces)
                // and an idle timer can take a new one. `record_authentication` refuses an answer
                // that no longer matches the lock on the glass; without it, that stale `Success`
                // released a lock nobody had authenticated against, and a stale failure counted an
                // attempt and printed an error against a lock screen the user had not touched yet.
                let succeeded = outcome == shared::PamOutcome::Success;
                if !lock.record_authentication(acquisition, outcome) {
                    // No push: a refused answer changed no state, and `push_snapshot` bumps the
                    // revision unconditionally, so pushing here would tell every lock screen its
                    // state changed when it did not.
                    eprintln!("lock: dropping a pam outcome for acquisition {acquisition}, which is no longer the lock on the glass");
                } else if succeeded {
                    // The command's own arm pushes the snapshot that goes with it.
                    lock.unlock();
                } else {
                    push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
                }
            }
            Some(()) = reload_events.recv() => {
                begin_reload(&registry, authoritative.generation_id, &mut next_sequence);
            }
            Some((generation_id, id)) = process_done.recv() => {
                // The wait itself must never block this select! -- see wait_and_report_exit's
                // own doc comment -- so only the fast, synchronous removal happens inline here.
                if let Some(child) = take_exited_process(&mut processes, generation_id, id) {
                    tokio::spawn(wait_and_report_exit(registry.clone(), generation_id, id, child));
                }
            }
            Some(inbound) = inbound_frames.recv() => match inbound.frame {
                RendererFrame::LockReport(report) if inbound.generation_id != authoritative.generation_id => {
                    // Same posture the `Unchanged` and handshake-frame arms below take towards a
                    // stale frame, and for a sharper reason: a superseded-but-not-yet-reaped
                    // connection is a real source of frames here, and either direction of a
                    // stale report corrupts the swap gate. A stale `Unlocked`/`Finished` clears
                    // `active` while the live generation genuinely holds the lock, reopening the
                    // gate and firing `swap_owed_on_unlock` straight into a swap that reaps the
                    // holder; a stale `Locked` shuts the gate with no holder at all, and no
                    // report will ever arrive to reopen it, so the config can never reload again.
                    eprintln!(
                        "generation {}'s lock report arrived from a non-authoritative generation (authoritative is {}); dropping: {report:?}",
                        inbound.generation_id, authoritative.generation_id
                    );
                }
                RendererFrame::LockReport(report) => {
                    // docs/adr/0052 decision 4: the outcome *is* this capability's state, so it
                    // goes through the controller and straight back out as a snapshot the lock
                    // screen reads. Also docs/adr/0042's gate: a swap deferred while the gate was
                    // shut runs the moment it opens.
                    //
                    // The question asked is `defers_swap` after the report, not "was the outcome
                    // `Finished`/`Unlocked`". `Refused` opens the gate too, now that a request in
                    // flight shuts it: a topology change deferred in the window before the
                    // Renderer answered, followed by a refusal (a config that declared no `lock`
                    // node, docs/adr/0052 decision 3), would otherwise leave a swap owed that
                    // nothing ever redeems, and the config could never reload again.
                    lock.record(lock::LockEvent::Reported(report.outcome));
                    push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
                    if !lock.defers_swap() && std::mem::take(&mut swap_owed_on_unlock) {
                        // A fresh cycle through the same call the watcher and `RequestReload`
                        // already make, not the deferred evaluation replayed: its `sequence` is
                        // stale by now (`is_current_reload` would drop the report anyway) and the
                        // config may have changed again since. Reusing the whole existing reload
                        // machinery with a second trigger holds no stale candidate.
                        begin_reload(&registry, authoritative.generation_id, &mut next_sequence);
                    }
                }
                RendererFrame::Command(envelope) => match envelope.params.capability.as_str() {
                    // One arm per capability: each capability's own `dispatch` adapter owns its
                    // action match, argument parse, and write-action spawn (ADR-0037), so a new
                    // action never touches this file. Static calls, no registry, no trait.
                    "process" => process::registry::dispatch(&mut processes, &registry, &process_done_tx, &envelope).await,
                    "network" => network::dispatch(&network, &envelope),
                    "bluetooth" => bluetooth::dispatch(&bluetooth, &envelope),
                    "tray" => tray::dispatch(&tray, &envelope),
                    "idle" => idle::dispatch(&idle, &envelope),
                    "sysinfo" => sysinfo::dispatch(&sysinfo, &envelope),
                    "keyboard" => keyboard::dispatch(&keyboard, &envelope),
                    "brightness" => brightness::dispatch(&brightness, &envelope),
                    "workspaces" => workspaces::dispatch(&workspaces, &envelope),
                    "power" => power::dispatch(&power, &envelope),
                    "audio" => audio::dispatch(&audio_commands, &envelope),
                    "mpris" => dbus::mpris::dispatch(&mpris, &envelope),
                    "updates" => updates::dispatch(&updates, &envelope),
                    "notifications" => notifications::dispatch(&notifications, &envelope),
                    "lock" => lock::dispatch(&lock, &envelope),
                    _ => eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope),
                },
                RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_) => {
                    // Both only matter mid-handshake, where `SocketCandidateLink` reads them
                    // directly off `inbound_frames` itself (see `TopologyChanged` below, and
                    // docs/adr/0025). One reaching this top-level match means it arrived
                    // *outside* any in-flight handshake this Supervisor is currently driving --
                    // stale, or a wire-protocol desync -- logged, not fatal.
                    eprintln!("generation {}'s handshake frame arrived outside any in-flight PBA handshake; dropping: {:?}", inbound.generation_id, inbound.frame);
                }
                RendererFrame::RequestReload => {
                    // docs/adr/0041 decision 4's "only new thing is the trigger": a `wl_output`
                    // appeared or disappeared, so the config has to be re-evaluated in case it
                    // loops over `screens`. Deliberately the same call the watcher arm above
                    // makes, on `authoritative.generation_id` rather than `inbound.generation_id`
                    // -- a superseded generation still holding a connection open must not be able
                    // to start a cycle, and the authoritative one is the only one whose scene the
                    // reload path would apply to anyway.
                    begin_reload(&registry, authoritative.generation_id, &mut next_sequence);
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence }) => {
                    if is_current_reload(sequence, next_sequence) {
                        idle.reset_registrations(inbound.generation_id).await;
                        send_frame_logged(&registry, inbound.generation_id, &SupervisorFrame::ApplyPendingReload(ApplyPendingReload { sequence }));
                    } else {
                        eprintln!(
                            "generation {}'s Unchanged report (sequence {sequence}) is stale -- a newer Reevaluate (sequence {next_sequence}) is \
                             already in flight; not applying",
                            inbound.generation_id
                        );
                    }
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) if lock.defers_swap() => {
                    // docs/adr/0042 and build-steps.md Phase 23 item 5: candidate N+1 cannot
                    // acquire the session lock generation N is holding, so PBA's overlapping-
                    // generation handoff is impossible until the lock clears. Deferred, not
                    // failed -- the `Finished`/`Unlocked` arm above starts a fresh cycle. Guarded
                    // arm before the real one below, the same ordering the `SecureSubmit` arms
                    // further down rely on. In-place reloads (the `Unchanged` arm above) are
                    // deliberately not gated: a colour or a label still applies live to a lock
                    // screen, which is the whole point of painting it in the Lua process.
                    //
                    // `defers_swap`, not `is_active`: a lock whose order is out but whose report
                    // has not come back yet is just as unswappable, and that window is neither
                    // short nor rare (`ext_session_lock_v1` lets the compositor withhold `locked`
                    // until lock surfaces are presented on every output). A swap there would
                    // `reap_process_group` the process that owns the lock object, with no report
                    // left to redeem `swap_owed_on_unlock` -- the session stays locked for good.
                    eprintln!("generation swap for sequence {sequence} deferred: the session is locked (docs/adr/0042)");
                    swap_owed_on_unlock = true;
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) => {
                    // Phase 13's watcher becomes `run_pba`'s real caller here (build-steps.md
                    // Phase 14 item 5). Inlined synchronously inside this match arm, not
                    // `tokio::spawn`ed -- see docs/adr/0025's "why the main loop blocks" item:
                    // swaps are rare and bounded (seconds, not minutes -- PBA_TIMINGS above), and
                    // nothing else is capability-routed over this socket yet to starve.
                    let candidate_generation_id = next_generation_id;
                    next_generation_id += 1;
                    let candidate_envs = vec![
                        ("OBLISK_GENERATION_ID".to_string(), candidate_generation_id.to_string()),
                        ("OBLISK_PBA_CANDIDATE".to_string(), "1".to_string()),
                    ];
                    // Every capability's latest known snapshot hydrates the fresh Candidate's
                    // first evaluation (§ 15.2 point 1; docs/adr/0029), not just audio's.
                    let snapshots: Vec<shared::StateSnapshot> = last_snapshots.values().cloned().collect();
                    let mut link = SocketCandidateLink { registry: registry.clone(), candidate_generation_id, inbound: &mut inbound_frames };

                    match reload::run_pba(&renderer_path_str, &[], &candidate_envs, &mut link, &snapshots, sequence, PBA_TIMINGS).await {
                        Ok(outcome) => {
                            // docs/adr/0043 decision 1 item 3, taken here and nowhere else: this
                            // is the widest point of the handoff window. The Candidate has
                            // presented evidence (`run_pba` returned `Ok`) and the superseded
                            // generation still owns every buffer it drew, so both are fully
                            // resident. One statement later the reap below starts tearing one of
                            // them down.
                            memory::log_sample("pba handoff", &[(authoritative.generation_id, &authoritative.child), (candidate_generation_id, &outcome.candidate)]);
                            for surface_id in &outcome.promoted_surfaces {
                                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::DeselectInput(DeselectInput { surface_id: surface_id.clone() }));
                                send_frame_logged(&registry, candidate_generation_id, &SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id: surface_id.clone() }));
                            }
                            // § 12's group reap applies to every process the superseded
                            // generation's Lua spawned via process.run, not just its own Renderer
                            // process (docs/adr/0026).
                            reap_generations_processes(&mut processes, authoritative.generation_id).await;
                            // `run_pba` never reaps `superseded` any more (docs/adr/0025 item 3)
                            // -- that's this caller's job, done only now that the Swap messages
                            // above have actually gone out.
                            match process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await {
                                Ok(process::ReapOutcome::ExitedCleanly(status)) => {
                                    eprintln!("superseded generation {} exited cleanly: {status}", authoritative.generation_id);
                                }
                                Ok(process::ReapOutcome::Escalated(status)) => {
                                    eprintln!("superseded generation {} had to be escalated to SIGKILL: {status}", authoritative.generation_id);
                                }
                                Err(err) => {
                                    eprintln!("failed to reap superseded generation {}: {err}", authoritative.generation_id);
                                }
                            }
                            authoritative = Authoritative { generation_id: candidate_generation_id, child: outcome.candidate };
                        }
                        Err(failure) => {
                            match failure {
                                reload::PbaFailure::SpawnFailed(err) => {
                                    eprintln!("generation swap for sequence {sequence} failed: could not spawn the candidate: {err}");
                                }
                                reload::PbaFailure::Link { stage, source } => {
                                    eprintln!("generation swap for sequence {sequence} failed during {stage:?}: {source}");
                                }
                                reload::PbaFailure::Timeout { stage } => {
                                    eprintln!("generation swap for sequence {sequence} failed: {stage:?} timed out");
                                }
                                reload::PbaFailure::UnexpectedEvidence { stage, surface_id } => {
                                    eprintln!(
                                        "generation swap for sequence {sequence} failed during {stage:?}: unexpected evidence for surface_id {surface_id:?}"
                                    );
                                }
                                reload::PbaFailure::AbortReapFailed { original, reap_error } => {
                                    eprintln!(
                                        "generation swap for sequence {sequence} failed ({original:?}) and the candidate's abort-reap also failed: {reap_error}"
                                    );
                                }
                            }
                            eprintln!("{} stays authoritative", authoritative.generation_id);
                        }
                    }
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                    eprintln!("generation {}'s shell.lua re-evaluation (sequence {sequence}) failed: {error}", inbound.generation_id);
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "polkit" && submit.action == "authenticate" => {
                    // build-steps.md Phase 15 item 3 (docs/adr/0028): the polkit-routed case,
                    // now driven for real via pam_worker::drive_pam_and_respond. This guarded
                    // arm must come before the catch-all SecureSubmit arm below (match arms are
                    // tried in order) so a polkit authenticate submit is routed here instead of
                    // falling through to the generic log-and-drop.
                    match pending_challenge.take() {
                        Some(challenge) => {
                            // mem::take moves the plaintext bytes out for drive_pam_and_respond
                            // to own and zeroize on every return path (see its own doc comment)
                            // -- it leaves submit.secret as an empty Vec (Default), which holds
                            // no plaintext, so there is nothing left in `submit` to zeroize.
                            let secret = std::mem::take(&mut submit.secret);
                            pam_worker::drive_pam_and_respond(&authority, challenge, secret).await;
                        }
                        None => {
                            eprintln!(
                                "generation {}'s secure_submit(polkit, authenticate) arrived with no pending polkit challenge; dropping",
                                submit.generation_id
                            );
                            submit.secret.zeroize();
                        }
                    }
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "network" && submit.action == "connect" => {
                    // ADR-0029: the network-routed case, mirroring the polkit arm above exactly
                    // -- an empty secret means an open network, a non-empty one becomes the
                    // wpa-psk password (NetworkController::connect decides which). Must come
                    // before the catch-all SecureSubmit arm below, same ordering reason as the
                    // polkit arm.
                    match network.take_connect_intent() {
                        Some(pending) => {
                            // mem::take moves the plaintext bytes out for NetworkController::connect
                            // to own and zeroize on every return path (see its own doc comment) --
                            // it leaves submit.secret as an empty Vec (Default), which holds no
                            // plaintext, so there is nothing left in `submit` to zeroize.
                            let secret = std::mem::take(&mut submit.secret);
                            let controller = network.clone();
                            tokio::spawn(async move { controller.connect(pending, secret).await; });
                        }
                        None => {
                            eprintln!(
                                "generation {}'s secure_submit(network, connect) arrived with no pending network connect intent; dropping",
                                submit.generation_id
                            );
                            submit.secret.zeroize();
                        }
                    }
                }
                RendererFrame::SecureSubmit(mut submit)
                    if submit.capability == "lock"
                        && submit.action == "authenticate"
                        && inbound.generation_id != authoritative.generation_id =>
                {
                    // The same stale-frame guard the `LockReport` arm above takes, and the
                    // asymmetry of leaving it off here would be the bug rather than the tidiness
                    // (Correctness review pass five). A superseded-but-not-yet-reaped connection is
                    // a real source of frames, and only the authoritative generation paints the
                    // lock screen a password can have been typed into. Left ungated, a stale
                    // submission spends the single in-flight PAM slot `try_begin_authentication`
                    // hands out and counts an attempt against a lock the user is still looking at.
                    //
                    // Deliberately unlike the polkit and network arms, which take any generation's
                    // submission: those answer a challenge the Supervisor itself is holding, so the
                    // pending intent is the guard. This one answers a lock, and the lock belongs to
                    // exactly one generation (docs/adr/0042).
                    eprintln!(
                        "generation {}'s secure_submit(lock, authenticate) is stale -- {} is authoritative; dropping",
                        submit.generation_id, authoritative.generation_id
                    );
                    submit.secret.zeroize();
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "lock" && submit.action == "authenticate" => {
                    // docs/adr/0042 and docs/adr/0052: this arm and the `pam_outcomes` arm above
                    // are the *only* path to an unlock, which is what turns "never call
                    // `unlock_and_destroy` except on a successful authentication" into a property
                    // of one call site rather than a rule the Renderer has to be trusted to keep.
                    // Must come before the catch-all SecureSubmit arm below, same ordering reason
                    // as the polkit and network arms above. No pending-intent lookup (unlike
                    // those two): the lock screen's submission carries everything needed, and the
                    // user being authenticated is this process's own owner.
                    //
                    // `try_begin_authentication` is the admission check, and it refuses two
                    // things (see its own doc comment): a submit with no lock held -- nothing
                    // stops a config from putting a `("lock", "authenticate")` textfield on the
                    // bar, and build-steps.md Phase 23 item 3 scopes authentication to the lock
                    // screen -- and a second submit while one conversation is still in flight.
                    // The `Some` is the acquisition this conversation is about, carried through
                    // the worker and back so the `pam_outcomes` arm above can tell an answer about
                    // *this* lock from an answer about the one before it.
                    if let Some(acquisition) = lock.try_begin_authentication() {
                        push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
                        // mem::take moves the plaintext bytes out for run_lock_authentication (via
                        // authenticate_current_user) to own and zeroize on every exit path,
                        // including a panic or a runtime-shutdown cancellation (see both
                        // functions' own doc comments) -- it leaves submit.secret as an empty Vec
                        // (Default), which holds no plaintext, so there is nothing left in
                        // `submit` to zeroize.
                        let secret = std::mem::take(&mut submit.secret);
                        // Spawned, not `.await`ed inline like the polkit arm above -- a deliberate
                        // departure from docs/adr/0025's "the main loop blocks for real work,
                        // bounded and rare". A polkit challenge is user-initiated once and
                        // rate-limited by polkitd; a lock screen's Enter key is neither bounded
                        // nor rare, and it is reachable by whoever is sitting at the console, so
                        // the same reasoning gives the opposite answer here. Blocking this loop
                        // for `pam_unix`'s ~2 second failure delay (or `PAM_EXCHANGE_TIMEOUT`'s 30
                        // seconds on a wedged worker) stalls every `LockReport`, every reload and
                        // every process reap behind it.
                        //
                        // run_lock_authentication, not authenticate_current_user directly: it
                        // guarantees outcome_tx still gets a PamOutcome even if this task panics
                        // or is dropped before its own happy-path send would run (Correctness
                        // review finding: `authenticating` has no other release path than that
                        // send reaching the pam_outcomes arm above).
                        let outcome_tx = pam_outcome_tx.clone();
                        tokio::spawn(pam_worker::run_lock_authentication(shared::Zeroizing::new(secret), acquisition, outcome_tx));
                    } else {
                        eprintln!(
                            "generation {}'s secure_submit(lock, authenticate) arrived with no lock held, or with an attempt already in flight; dropping",
                            submit.generation_id
                        );
                        submit.secret.zeroize();
                    }
                }
                RendererFrame::SecureSubmit(mut submit) => {
                    // build-steps.md Phase 15 item 2 closes ADR-0015 item 2 (the textfield/IPC
                    // half only) -- this is deliberately still just a channel-forward-and-log
                    // placeholder, the same discipline this codebase already uses for
                    // `challenges.recv()` above. The `("polkit", "authenticate")` and
                    // `("network", "connect")` and `("lock", "authenticate")` cases are now
                    // handled by the guarded arms above (docs/adr/0028 Phase 15 item 3;
                    // docs/adr/0029; docs/adr/0042); this fallback covers every other
                    // capability/action, none of which exist yet.
                    //
                    // Never log the secret itself -- only its length -- and log before zeroizing
                    // it, not after (a post-zeroize log would just print the byte count of an
                    // already-cleared buffer, which happens to be correct here since `len()` is
                    // read first, but doing the read before the clear keeps that order honest).
                    eprintln!(
                        "generation {}'s secure_submit received: capability={:?} action={:?} secret_len={}",
                        submit.generation_id,
                        submit.capability,
                        submit.action,
                        submit.secret.len()
                    );
                    submit.secret.zeroize();
                }
            },
            else => break,
        }
    }

    // Shutdown, reached from the signal arms above or `else => break` (every channel closed):
    // reap the authoritative Renderer and every still-live `process.run` child rather than exit
    // out from under them. `reap_process_group` is SIGTERM-then-SIGKILL (process::DEFAULT_REAP_GRACE),
    // the same grace this codebase already gives every other reap.
    // Skipped when the departure arm already collected it: `reap_process_group` starts by asking
    // for the pid, which tokio clears once `wait` has returned, so reaping a Renderer that is
    // already gone logs "child has no pid; already reaped" as a failure right underneath the crash
    // report that explains it. The exit is not a failure to reap, and should not read as one.
    if !renderer_departed
        && let Err(err) = process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await
    {
        eprintln!("failed to reap authoritative generation {}'s renderer on shutdown: {err}", authoritative.generation_id);
    }
    reap_all_processes(&mut processes).await;
    // Best-effort: `socket::bind`'s own stale-file removal covers a missed unlink (a crash,
    // `SIGKILL`) on the *next* boot regardless, so a failure here isn't fatal to anything.
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// The raw `wait(2)` status for a normal exit with `code`, which is what `ExitStatus::from_raw`
    /// wants. Written out rather than inlined so the `<< 8` appears once and the tests below read
    /// as the cases they are.
    fn exited(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn killed_by(signal: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(signal)
    }

    #[test]
    fn a_renderer_that_exits_zero_is_not_a_crash() {
        assert_eq!(classify_departure(exited(0)), RendererDeparture::Clean);
    }

    #[test]
    fn a_renderer_that_exits_nonzero_carries_its_code() {
        assert_eq!(classify_departure(exited(101)), RendererDeparture::Failed { code: 101 });
    }

    #[test]
    fn a_renderer_killed_by_a_signal_reports_the_signal_not_an_exit_code() {
        // The OOM killer's SIGKILL, and the shape a `wait` status makes easiest to misread: a
        // signalled child has no exit code at all, so a classifier that reaches for `code()` first
        // reports `None` and loses the only fact that says what happened.
        assert_eq!(classify_departure(killed_by(9)), RendererDeparture::Signalled { signal: 9 });
    }

    #[test]
    fn a_departure_while_locked_says_the_session_stays_locked() {
        let report = departure_report(RendererDeparture::Signalled { signal: 9 }, 3, true);

        assert!(report.contains("signal 9"), "the signal has to survive into the message: {report}");
        assert!(
            report.contains("session stays locked"),
            "a Renderer that died holding the lock is a different emergency from one that died without it, and the \
             message is the only place that distinction reaches a human: {report}"
        );
    }

    #[test]
    fn a_departure_while_unlocked_does_not_mention_the_lock() {
        let report = departure_report(RendererDeparture::Failed { code: 101 }, 3, false);

        assert!(report.contains("code 101"), "{report}");
        assert!(!report.contains("locked"), "an unlocked crash must not cry lock: {report}");
    }

    #[test]
    fn is_current_reload_matches_only_the_most_recently_sent_sequence() {
        assert!(is_current_reload(3, 3));
        assert!(!is_current_reload(3, 4), "a report for an older sequence than the last-sent one must be stale");
    }

    #[test]
    fn begin_reload_bumps_the_supervisor_owned_sequence_and_sends_the_reevaluate_carrying_it() {
        let registry = socket::GenerationRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        registry.register(7, tx);
        let mut next_sequence = 0;

        // Both triggers -- the watcher's file change and a Renderer's `RequestReload` after a
        // `wl_output` change (docs/adr/0041 decision 4) -- reach this same call, so two of them
        // must produce two distinct, increasing sequences rather than repeating one.
        begin_reload(&registry, 7, &mut next_sequence);
        begin_reload(&registry, 7, &mut next_sequence);

        assert_eq!(next_sequence, 2);
        let sent: Vec<SupervisorFrame> =
            std::iter::from_fn(|| rx.try_recv().ok()).map(|payload| serde_json::from_slice(&payload).unwrap()).collect();
        assert_eq!(
            sent,
            vec![
                SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 }),
                SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 2 }),
            ]
        );
    }

    #[test]
    fn begin_reload_still_advances_the_sequence_when_the_generation_has_no_connection() {
        // `send_frame_logged` logs and drops a `NoConnection` failure. The sequence must advance
        // anyway, or a later report from a generation that reconnects could collide with a
        // sequence `is_current_reload` has already accepted.
        let registry = socket::GenerationRegistry::default();
        let mut next_sequence = 41;
        begin_reload(&registry, 7, &mut next_sequence);
        assert_eq!(next_sequence, 42);
    }
}
