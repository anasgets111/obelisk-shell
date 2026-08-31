mod applications;
mod audio;
mod dbus;
mod generation;
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
use std::path::{Path, PathBuf};
use std::time::Duration;

use applications::{ApplicationsController, ApplicationsSignal};
use dbus::bluetooth::{self, BluetoothController, BluetoothSignal};
use dbus::network::{self, NetworkController, NetworkSignal};
use dbus::notifications::{self, NotificationsController, NotificationsSignal};
use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use dbus::power::{self, PowerController, PowerSignal};
use dbus::tray::{self, TrayController, TraySignal};
use generation::{Authoritative, RendererDeparture, RestartBrake, RESTART_LIMIT, RESTART_WINDOW, classify_departure, departure_report, renderer_binary_path};
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
use process::registry::{LiveProcesses, reap_all_processes, take_exited_process, wait_and_report_exit};
use reload_link::SocketCandidateLink;
use socket::send_frame_logged;
use shared::{
    ApplyPendingReload, RendererFrame, ReevaluateReport, ReevaluateRequest, SupervisorFrame, Zeroize,
};
use snapshot::push_snapshot;

/// How long the Watcher waits after the last relevant `shell.lua` change before dispatching a
/// reload -- coalesces a multi-event save into one round trip. Fixed (docs/adr/0024 item 6).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// § 15.2/15.3's ready-signal and evidence-verification deadlines (`reload::PbaTimings`), scaled
/// up from `reload.rs`'s own test constants for a real Candidate that has to bind Wayland/EGL.
/// Generous enough a healthy Candidate never trips them, tight enough a wedged one doesn't hang a
/// config edit.
const PBA_TIMINGS: reload::PbaTimings =
    reload::PbaTimings { ready_timeout: Duration::from_secs(2), evidence_timeout: Duration::from_secs(3), reap_grace: process::DEFAULT_REAP_GRACE };

/// Whether an `Unchanged` report's `sequence` still names the most recently sent `Reevaluate`.
/// A mismatch means a newer `Reevaluate` already went out for this generation, so the go-ahead
/// must not fire for a superseded evaluation (docs/adr/0024 item 2).
fn is_current_reload(report_sequence: u64, next_sequence: u64) -> bool {
    report_sequence == next_sequence
}

/// Starts one reload cycle: bumps the sequence and sends the `Reevaluate` carrying it (docs/adr/
/// 0024, docs/adr/0041 decision 4). Reached by the watcher's debounced file change and by a
/// Renderer's `RequestReload` after a `wl_output` change -- both funnel through the same counter
/// so `is_current_reload` rejects a stale report from either trigger.
fn begin_reload(registry: &socket::GenerationRegistry, generation_id: u32, next_sequence: &mut u64) {
    *next_sequence += 1;
    send_frame_logged(registry, generation_id, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: *next_sequence }));
}


/// The malformed-arguments log line every capability's `dispatch` adapter shares (ADR-0037).
pub(crate) fn log_malformed_command(params: &shared::CommandParams) {
    eprintln!(
        "malformed {}.{} command from generation {}: {:?}",
        params.capability, params.action, params.generation_id, params.arguments
    );
}

/// [`log_malformed_command`]'s sibling for a `dispatch` adapter's unmatched-action fallback.
pub(crate) fn log_unknown_action(params: &shared::CommandParams) {
    eprintln!("{}: unknown action {:?} from generation {}", params.capability, params.action, params.generation_id);
}

/// Why `run_supervisor` returned, and the process exit code it becomes (docs/adr/0059 decision 3).
/// `packaging/oblisk-shell.service` restarts this process on every exit but one, so that exit
/// needs a code of its own to be named in `RestartPreventExitStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shutdown {
    /// `SIGINT`, `SIGTERM`, or every channel closing. Rerunning the shell is recovery.
    Requested,
    /// [`RestartBrake`] refused another respawn. Rerunning would hand the same config to a fresh
    /// Renderer, which dies the same way, forever.
    RestartBrakeTripped,
}

impl Shutdown {
    /// `3` is arbitrary except for what it isn't: not `0` (a clean exit), not `1` (`main`'s own
    /// `?` failure), and not a code shell conventions reserve for signals (128+) or "not found" (127).
    fn exit_code(self) -> i32 {
        match self {
            Shutdown::Requested => 0,
            Shutdown::RestartBrakeTripped => 3,
        }
    }
}

/// Why a generation is being asked to take a lock it did not request -- the log line differs by
/// cause even though both arrive at the same state: a lock the compositor already holds with
/// nothing of ours on it. Naming it beats a `bool` whose two sides read identically at the call
/// site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelockReason {
    /// docs/adr/0058 decision 4: the Renderer holding the lock died and a replacement was spawned.
    RendererReplaced,
    /// docs/adr/0060: this Supervisor started with `$XDG_RUNTIME_DIR`'s marker set, so a previous
    /// one died while the session was locked.
    SupervisorRestarted,
}

impl RelockReason {
    /// The subject noun for the outcome messages below.
    fn subject(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the replacement",
            RelockReason::SupervisorRestarted => "the restarted shell",
        }
    }

    /// Why the lock is going out unasked, for the request-in-flight log line.
    fn because(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the Renderer that held it died",
            RelockReason::SupervisorRestarted => "the Supervisor that held it died",
        }
    }
}

/// Branches into the PAM worker's own tokio-free path (ADR-0028) before the normal Supervisor.
/// Must run before any D-Bus/tokio-runtime/audio-thread setup -- the worker path must not
/// construct a tokio runtime at all.
fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("OBLISK_PAM_WORKER").is_some() {
        return pam_worker::run_worker();
    }
    // std::process::exit, not return: the exit code is the point of `Shutdown`, and `main`'s
    // `Result` can only produce 0 or 1 (docs/adr/0059 decision 3). Every teardown
    // `run_supervisor` owns has already run by the time it returns.
    let shutdown = tokio::runtime::Runtime::new()?.block_on(run_supervisor())?;
    std::process::exit(shutdown.exit_code());
}

async fn run_supervisor() -> Result<Shutdown, Box<dyn Error>> {
    let connection = zbus::Connection::system().await?;
    let subject = current_session_subject()?;

    let (tx, mut challenges) = tokio::sync::mpsc::unbounded_channel();
    let agent = AuthenticationAgent::new(tx);
    register_agent(&connection, agent, &subject, "en_US.UTF-8", AGENT_OBJECT_PATH).await?;
    // Long-lived proxy for the polkit reply once a challenge's PAM conversation finishes
    // (docs/adr/0028) -- built once here, not per-challenge.
    let authority = zbus_polkit::policykit1::AuthorityProxy::new(&connection).await?;

    // NetworkManager shares the system bus with polkit (docs/adr/0029). Anything pushed before
    // the boot Renderer's connection exists is replayed by connected.recv() below.
    let (network_signal_tx, mut network_signals) = tokio::sync::mpsc::unbounded_channel::<NetworkSignal>();
    let network = NetworkController::new(connection.clone(), network_signal_tx).await?;

    // BlueZ shares the system bus too (docs/adr/0030).
    let (bluetooth_signal_tx, mut bluetooth_signals) = tokio::sync::mpsc::unbounded_channel::<BluetoothSignal>();
    let bluetooth = BluetoothController::new(connection.clone(), bluetooth_signal_tx).await;

    // The tray host is a session-bus protocol, unlike NetworkManager/BlueZ/polkit above, so it
    // needs its own connection. A missing session bus degrades to TrayController::inert.
    let (tray_signal_tx, mut tray_signals) = tokio::sync::mpsc::unbounded_channel::<TraySignal>();
    let tray = match zbus::Connection::session().await {
        Ok(tray_connection) => TrayController::new(tray_connection, tray_signal_tx).await,
        Err(err) => {
            eprintln!("tray: failed to connect to the session bus; tray host disabled for this run: {err}");
            TrayController::inert(tray_signal_tx)
        }
    };

    let (audio_tx, mut audio_apps) = tokio::sync::mpsc::unbounded_channel();
    // video_tx feeds oblisk.privacy's PipeWire name-enrichment (docs/adr/0034), same registry
    // thread as audio.
    let (video_tx, video_sources) = tokio::sync::mpsc::unbounded_channel();
    // pipewire-rs's event loop is Rc-based and !Send -- needs its own OS thread, not a tokio
    // task. audio_commands is a pipewire::channel, not a controller handle, for the same reason.
    let (audio_commands, audio_command_rx) = audio::mixer::command_channel();
    std::thread::spawn(move || audio::mixer::run(audio_tx, video_tx, audio_command_rx));

    // Notifications get their own session-bus connection (ADR-0033): a real desktop may already
    // own org.freedesktop.Notifications, degrading to inert via RequestName's DoNotQueue.
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

    // MPRIS gets its own session-bus connection too (ADR-0036). MprisController::new is not
    // async: it spawns discovery and returns immediately.
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

    // Idle capability (ADR-0032): notify rides its own Wayland connection (idle authority must
    // survive a Renderer crash or reload, ADR-0010); inhibit rides the shared connection.
    // Constructed after spawn_listener so notify setup's own spawn_blocking task (see
    // hardware::idle's module doc) can't block the control socket.
    let (idle_signal_tx, mut idle_signals) = tokio::sync::mpsc::unbounded_channel::<shared::IdleEvent>();
    let idle = IdleController::new(connection.clone(), idle_signal_tx).await;

    // sysinfo capability (docs/adr/0035): three independently-configurable poll tasks, dormant
    // until Lua calls sysinfo:configure.
    let (sysinfo_signal_tx, mut sysinfo_signals) = tokio::sync::mpsc::unbounded_channel::<SysinfoSignal>();
    let sysinfo = SysinfoController::new(PathBuf::from("/proc"), PathBuf::from("/sys/class/hwmon"), sysinfo_signal_tx);

    // keyboard capability (docs/adr/0034): a missing KbdBacklight degrades in place to
    // backlight_pct: -1, a missing lock source to false.
    let (keyboard_signal_tx, mut keyboard_signals) = tokio::sync::mpsc::unbounded_channel::<KeyboardSignal>();
    let keyboard = KeyboardController::new(connection.clone(), &PathBuf::from("/sys/class/leds"), keyboard_signal_tx).await;

    // privacy capability (docs/adr/0034): kernel-level /dev/videoN open/close via inotify plus a
    // /proc fd-scan, enriched by video_sources from the mixer thread above.
    let (privacy_signal_tx, mut privacy_signals) = tokio::sync::mpsc::unbounded_channel::<PrivacySignal>();
    let privacy = PrivacyController::new(PathBuf::from("/proc"), &PathBuf::from("/sys/class/video4linux"), video_sources, privacy_signal_tx);

    // updates capability (docs/adr/0034): alpm-based Arch update checking, separate from
    // sysinfo's own scheduler.
    let (updates_signal_tx, mut updates_signals) = tokio::sync::mpsc::unbounded_channel::<UpdatesSignal>();
    let updates = UpdatesController::new(PathBuf::from("/etc/pacman.conf"), PathBuf::from("/var/lib/pacman"), updates_signal_tx);

    // battery capability (docs/adr/0053, § 2.2). The root is a parameter, not a constant, so
    // device selection is testable against a fixture directory.
    let (battery_signal_tx, mut battery_signals) = tokio::sync::mpsc::unbounded_channel::<BatterySignal>();
    let battery = BatteryController::new(PathBuf::from("/sys/class/power_supply"), battery_signal_tx);

    // brightness capability (docs/adr/0053, § 2.3): ranked firmware over platform over raw. No
    // device found means it never pushes -- see hardware::brightness's module doc.
    let (brightness_signal_tx, mut brightness_signals) = tokio::sync::mpsc::unbounded_channel::<BrightnessSignal>();
    let brightness = BrightnessController::new(PathBuf::from("/sys/class/backlight"), connection.clone(), brightness_signal_tx);

    // workspaces capability (docs/adr/0056, § 2.9): niri's IPC stream via $NIRI_SOCKET. A
    // non-niri session never pushes.
    let (workspaces_signal_tx, mut workspaces_signals) = tokio::sync::mpsc::unbounded_channel::<WorkspacesSignal>();
    let workspaces = WorkspacesController::new(workspaces_signal_tx);

    // power capability (§ 2.13, docs/adr/0053): UPower for on_battery/energy_rate,
    // power-profiles-daemon for active_profile/profiles. Either can be missing.
    let (power_signal_tx, mut power_signals) = tokio::sync::mpsc::unbounded_channel::<PowerSignal>();
    let power = PowerController::new(connection.clone(), power_signal_tx);

    // system capability (docs/adr/0053, § 2.11): the 1 Hz clock plus persisted state.json.
    let (system_signal_tx, mut system_signals) = tokio::sync::mpsc::unbounded_channel::<SystemSignal>();
    let system = SystemController::new(
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/"))),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        system_signal_tx,
    );

    // applications capability (docs/adr/0061): the installed `.desktop` entries. Scans in the
    // background from construction, so this returns before the first surface is up.
    let (applications_signal_tx, mut applications_signals) = tokio::sync::mpsc::unbounded_channel::<ApplicationsSignal>();
    let applications = ApplicationsController::new(
        applications::application_dirs(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            std::env::var("XDG_DATA_DIRS").ok(),
            Path::new(&std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/"))),
        ),
        applications_signal_tx,
    );

    // lock capability (docs/adr/0042, docs/adr/0052): the Renderer holds ext_session_lock_v1 and
    // paints it; this side owns the decision to take it. The channel exists because the
    // controller must not cache the authoritative generation id -- a swap reassigns it.
    let (lock_command_tx, mut lock_commands) = tokio::sync::mpsc::unbounded_channel::<shared::SetSessionLock>();
    let lock = LockController::new(lock_command_tx);
    // Kept alive for the whole run so the channel never closes. Each outcome carries the
    // acquisition it answers for (see lock::accepts_outcome).
    let (pam_outcome_tx, mut pam_outcomes) = tokio::sync::mpsc::unbounded_channel::<(u64, shared::PamOutcome)>();

    let config_dir = shared::config_dir()?;
    let mut reload_events = watcher::spawn_watcher(&config_dir, RELOAD_DEBOUNCE)?;
    let mut next_sequence: u64 = 0;

    // Generation 0 is boot-spawned by the Supervisor itself (docs/adr/0025 item 7) -- there is no
    // shell without it, so a spawn failure here is fatal to main.
    let renderer_path = renderer_binary_path()?;
    let renderer_path_str = renderer_path.to_string_lossy().into_owned();
    let boot_child =
        process::spawn_group_leader(&renderer_path_str, &[], &[("OBLISK_GENERATION_ID".to_string(), "0".to_string())])?;
    let mut authoritative = Authoritative { generation_id: 0, child: boot_child };
    let mut next_generation_id: u32 = 1;
    let mut restart_brake = RestartBrake::new(RESTART_LIMIT, RESTART_WINDOW);
    // docs/adr/0058 decision 4, docs/adr/0060: set when a Renderer dies holding the lock, or
    // this process started with the session already locked. relock_when_connected is the
    // intent, relock_in_flight lets LockReport tell a re-acquisition's answer from an ordinary
    // one's. Read once, before anything can connect -- reading later would race the boot
    // Renderer's registration.
    let locked_flag = lock::SessionLockedFlag::at(shared::session_locked_flag_path()?);
    let mut relock_when_connected = locked_flag.is_set().then_some(RelockReason::SupervisorRestarted);
    let mut relock_in_flight: Option<RelockReason> = None;
    if relock_when_connected.is_some() {
        eprintln!(
            "the session was locked when the last Supervisor stopped and the compositor has not unlocked it, so the boot Renderer will be asked \
             to take that lock over (docs/adr/0060)"
        );
    }

    // The last StateSnapshot pushed per capability, keyed by name -- hydrates a fresh
    // Candidate's first evaluation (§ 15.2 point 1; docs/adr/0029).
    let mut last_snapshots: HashMap<String, shared::StateSnapshot> = HashMap::new();
    // Every capability's state-version counter, keyed by name (ADR-0004).
    let mut revisions: HashMap<String, u32> = HashMap::new();

    // No multi-challenge queue, so keeping only the latest is correct.
    let mut pending_challenge: Option<dbus::polkit::BeginAuthenticationCall> = None;

    // Every process.run-spawned child still tracked (docs/adr/0026).
    // Whether a topology-changing reload was refused while locked (docs/adr/0042). A bool, not
    // a queue: a second change while locked is still one reload to run.
    let mut swap_owed_on_unlock = false;

    let mut processes: LiveProcesses = HashMap::new();
    let (process_done_tx, mut process_done) = tokio::sync::mpsc::unbounded_channel::<(u32, u64)>();
    // With no handler, Ctrl-C killed the Supervisor on the spot, leaving the Renderer orphaned
    // and running headless. SIGTERM gets the same treatment.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let mut memory_sampler = memory::sampler_from_env();
    // Set only by the departure arm below, so shutdown can tell "still running, needs reaping"
    // from "already gone".
    let mut renderer_departed = false;
    // Every break below leaves this alone except the brake's, the one exit a service manager must
    // not restart into (docs/adr/0059 decision 3).
    let mut shutdown = Shutdown::Requested;

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
            // docs/adr/0058 decision 1: a dead Renderer sends no frames, and a healthy idle one
            // sends none either -- without this arm the difference never reaches select! at all.
            status = authoritative.child.wait() => {
                let departure = match status {
                    Ok(status) => classify_departure(status),
                    Err(err) => {
                        eprintln!("failed to wait on generation {}'s renderer: {err}", authoritative.generation_id);
                        RendererDeparture::Failed { code: -1 }
                    }
                };
                let was_locked = lock.snapshot().active;
                eprintln!("{}", departure_report(departure, authoritative.generation_id, was_locked));
                renderer_departed = true;

                // Checked before the spawn, not after a failure (docs/adr/0058 decision 3): the
                // loop this defends against is one where every spawn succeeds and every Renderer
                // then dies on the same config.
                if !restart_brake.allow(std::time::Instant::now()) {
                    eprintln!(
                        "giving up: {RESTART_LIMIT} renderers have died within {}s, which is a config that kills whatever it is \
                         handed rather than a transient (docs/adr/0058 decision 3)",
                        RESTART_WINDOW.as_secs()
                    );
                    shutdown = Shutdown::RestartBrakeTripped;
                    break;
                }
                let replacement_generation_id = next_generation_id;
                next_generation_id += 1;
                match process::spawn_group_leader(
                    &renderer_path_str,
                    &[],
                    &[("OBLISK_GENERATION_ID".to_string(), replacement_generation_id.to_string())],
                ) {
                    Ok(child) => {
                        authoritative = Authoritative { generation_id: replacement_generation_id, child };
                        renderer_departed = false;
                        eprintln!("spawned generation {replacement_generation_id} to replace it");
                        // Hydration needs no code here: the replacement's connected registration
                        // replays every last_snapshots entry via the arm below.
                        if was_locked {
                            // docs/adr/0058 decision 4: the lock object died with the process, so
                            // active no longer describes anything this shell holds. RendererLost
                            // is what lets lock() through despite that. The request waits for the
                            // replacement to register -- send_frame_logged needs a connection.
                            lock.record(lock::LockEvent::RendererLost);
                            relock_when_connected = Some(RelockReason::RendererReplaced);
                            eprintln!(
                                "the session is still locked, so generation {replacement_generation_id} will be asked to retake the lock once it connects"
                            );
                        }
                    }
                    Err(err) => {
                        eprintln!("could not spawn a replacement renderer: {err}");
                        break;
                    }
                }
            }
            Some(_) = memory::tick_sampler(&mut memory_sampler) => {
                memory::log_sample("steady state", &[(authoritative.generation_id, &authoritative.child)]);
            }
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
                pending_challenge = Some(challenge);
            }
            Some(generation_id) = connected.recv() => {
                // Authoritative only: a PBA candidate gets its own hydration from run_pba's
                // snapshots argument (docs/adr/0029); replaying here too would be redundant.
                // Fixes the boot-time race noted above -- whatever network/bluetooth captured
                // before this connection existed is delivered now.
                if generation_id == authoritative.generation_id {
                    for snapshot in last_snapshots.values() {
                        send_frame_logged(&registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
                    }
                    // docs/adr/0058 decision 4, and it must come after the replay above: a lock
                    // acquired before hydration paints one frame of defaults on the surface where
                    // that's indistinguishable from a broken shell. No auth-capability check here
                    // -- the Renderer refuses and reports Refused when its tree has no way to
                    // reach PAM (docs/adr/0052 decision 3).
                    if let Some(reason) = relock_when_connected.take() {
                        relock_in_flight = Some(reason);
                        eprintln!(
                            "asking generation {generation_id} to take the session lock over, because {} (docs/adr/0058 decision 4, docs/adr/0060)",
                            reason.because()
                        );
                        lock.lock();
                    }
                }
            }
            Some(apps) = audio_apps.recv() => {
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "audio", &apps);
            }
            Some(signal) = network_signals.recv() => {
                // The controller owns the signal's state semantics (ADR-0037).
                let state = network.handle_signal(signal).await;
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "network", &state);
            }
            Some(signal) = bluetooth_signals.recv() => {
                let state = bluetooth.handle_signal(signal).await;
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "bluetooth", &state);
            }
            Some(TraySignal::RegistryChanged) = tray_signals.recv() => {
                // No debounce (docs/adr/0031): build_state is a synchronous snapshot of already-
                // live data the forwarder task recomputed before sending.
                let tray_state = tray.build_state();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "tray", &tray_state);
            }
            Some(dbus::mpris::MprisSignal::Changed) = mpris_signals.recv() => {
                // No debounce (ADR-0036).
                let mpris_state = mpris.build_state();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "mpris", &mpris_state);
            }
            Some(NotificationsSignal::Changed) = notifications_signals.recv() => {
                // No debounce (ADR-0033): every mutation fully re-derives notifications state
                // before signaling.
                let notifications_state = notifications.build_state();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "notifications", &notifications_state);
            }
            Some(event) = idle_signals.recv() => {
                // Routed to whichever generation made the register_threshold call now firing
                // (event.generation_id), not the authoritative one -- ADR-0006. Sent as a raw
                // IdleEvent, not through the StateSnapshot/revision path: idle is event-shaped,
                // not pollable state (ADR-0032).
                send_frame_logged(&registry, event.generation_id, &SupervisorFrame::IdleEvent(event));
            }
            Some(SysinfoSignal::Changed) = sysinfo_signals.recv() => {
                // No debounce: whichever of the three poll tasks fired already wrote its field(s)
                // under its own lock (docs/adr/0035); this arm just clones and pushes.
                let state = sysinfo.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "sysinfo", &state);
            }
            Some(KeyboardSignal::Changed) = keyboard_signals.recv() => {
                let state = keyboard.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "keyboard", &state);
            }
            Some(BatterySignal::Changed) = battery_signals.recv() => {
                let state = battery.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "battery", &state);
            }
            Some(BrightnessSignal::Changed) = brightness_signals.recv() => {
                // Fires only when a backlight device was found (docs/adr/0053).
                let state = brightness.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "brightness", &state);
            }
            Some(WorkspacesSignal::Changed) = workspaces_signals.recv() => {
                // The controller filters niri's stream down to real changes already.
                let state = workspaces.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "workspaces", &state);
            }
            Some(PowerSignal::Changed) = power_signals.recv() => {
                // UPower re-emits EnergyRate roughly once a minute; the controller filters that
                // to real changes first.
                let state = power.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "power", &state);
            }
            Some(ApplicationsSignal::Changed) = applications_signals.recv() => {
                let state = applications.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "applications", &state);
            }
            Some(SystemSignal::Changed) = system_signals.recv() => {
                // The only capability pushing on a timer, once per wall-clock second (docs/adr/
                // 0053 decision 2) -- emitted only when the epoch second actually changed.
                let state = system.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "system", &state);
            }
            Some(PrivacySignal::Changed) = privacy_signals.recv() => {
                let state = privacy.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "privacy", &state);
            }
            Some(UpdatesSignal::Changed) = updates_signals.recv() => {
                // Fires after a periodic check and install progress updates.
                let state = updates.snapshot();
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "updates", &state);
            }
            Some(command) = lock_commands.recv() => {
                // The only place a SetSessionLock is addressed. The state push rides along: every
                // command this capability sends is also a state change a lock screen must see
                // (docs/adr/0052 decision 4).
                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::SetSessionLock(command));
                push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
            }
            Some((acquisition, outcome)) = pam_outcomes.recv() => {
                // The other half of the secure_submit(lock, authenticate) arm below -- the one
                // place a Success becomes an unlock order (docs/adr/0042).
                //
                // `acquisition` matters because a PAM answer outlives the lock it answers for: it
                // takes about a second (pam_unix), up to PAM_EXCHANGE_TIMEOUT's thirty on a wedged
                // worker, and inside that window the compositor can end the lock
                // (loginctl unlock-session) or an idle timer can take a new one.
                // record_authentication refuses an answer that no longer matches the lock on the
                // glass.
                let succeeded = outcome == shared::PamOutcome::Success;
                if !lock.record_authentication(acquisition, outcome) {
                    // No push: a refused answer changed no state, and push_snapshot bumps the
                    // revision unconditionally.
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
                // wait must never block select! -- see wait_and_report_exit's own doc comment --
                // so only the fast, synchronous removal happens inline.
                if let Some(child) = take_exited_process(&mut processes, generation_id, id) {
                    tokio::spawn(wait_and_report_exit(registry.clone(), generation_id, id, child));
                }
            }
            Some(inbound) = inbound_frames.recv() => match inbound.frame {
                RendererFrame::LockReport(report) if inbound.generation_id != authoritative.generation_id => {
                    // A superseded-but-not-yet-reaped connection is a real frame source, and
                    // either direction of a stale report corrupts the swap gate: a stale
                    // Unlocked/Finished reopens the gate into a swap that reaps the live lock
                    // holder; a stale Locked shuts the gate with no holder, and no report ever
                    // reopens it.
                    eprintln!(
                        "generation {}'s lock report arrived from a non-authoritative generation (authoritative is {}); dropping: {report:?}",
                        inbound.generation_id, authoritative.generation_id
                    );
                }
                RendererFrame::LockReport(report) => {
                    // docs/adr/0052 decision 4: the outcome is this capability's state, straight
                    // back out as a snapshot. Also docs/adr/0042's gate: a swap deferred while shut
                    // runs the moment it opens.
                    //
                    // Checked as defers_swap after the report, not "was the outcome Finished/
                    // Unlocked": Refused opens the gate too, since a request in flight shuts it --
                    // otherwise a refusal after a deferred topology change would leave a swap owed
                    // that nothing ever redeems.
                    if let Some(who) = relock_in_flight.take() {
                        let who = who.subject();
                        match &report.outcome {
                            shared::LockOutcome::Locked => eprintln!("{who} took the session lock over; the lock screen is back on the glass"),
                            // Deliberately not lock_stays_authenticatable's wording: nothing is on
                            // screen here but the compositor's own fallback.
                            shared::LockOutcome::Refused(reason) => eprintln!(
                                "{who} could not take the session lock over: {reason}. The session stays locked with no lock screen on it, \
                                 so the way back in is a VT switch (docs/adr/0058 decision 4, docs/adr/0060)"
                            ),
                            other => eprintln!("{who}'s lock re-acquisition ended as {other:?} rather than a lock"),
                        }
                    }
                    // Before record, and off the outcome rather than LockState: the marker must
                    // keep saying "locked" through a RendererLost that clears active
                    // (docs/adr/0060).
                    locked_flag.apply(lock::compositor_lock_change(&report.outcome));
                    lock.record(lock::LockEvent::Reported(report.outcome));
                    push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
                    if !lock.defers_swap() && std::mem::take(&mut swap_owed_on_unlock) {
                        // A fresh reload call, not the deferred evaluation replayed: its sequence
                        // is stale by now and the config may have changed again since.
                        begin_reload(&registry, authoritative.generation_id, &mut next_sequence);
                    }
                }
                RendererFrame::Command(envelope) => match envelope.params.capability.as_str() {
                    // One arm per capability: each dispatch adapter owns its own action match,
                    // argument parse, and write-action spawn (ADR-0037).
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
                    "applications" => applications::dispatch(&applications, &envelope),
                    _ => eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope),
                },
                RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_) => {
                    // Both only matter mid-handshake, where SocketCandidateLink reads them
                    // directly off inbound_frames (see TopologyChanged below, docs/adr/0025).
                    // Reaching here means it arrived outside any in-flight handshake -- stale, or
                    // a wire-protocol desync.
                    eprintln!("generation {}'s handshake frame arrived outside any in-flight PBA handshake; dropping: {:?}", inbound.generation_id, inbound.frame);
                }
                RendererFrame::RequestReload => {
                    // docs/adr/0041 decision 4: a wl_output appeared or disappeared. Deliberately
                    // uses authoritative.generation_id, not inbound.generation_id -- a superseded
                    // generation must not be able to start a cycle.
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
                    // docs/adr/0042: candidate N+1 cannot acquire the lock generation N holds, so
                    // PBA's handoff waits until the lock clears. In-place reloads (Unchanged
                    // above) are not gated. Checked as defers_swap, not is_active: a lock order
                    // that's out but not yet reported is just as unswappable, and that window can
                    // be long -- a swap inside it would reap the process owning the lock object.
                    eprintln!("generation swap for sequence {sequence} deferred: the session is locked (docs/adr/0042)");
                    swap_owed_on_unlock = true;
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) => {
                    // Inlined synchronously, not tokio::spawn'd (docs/adr/0025): swaps are rare
                    // and bounded (seconds, PBA_TIMINGS above), and nothing else capability-routed
                    // over this socket yet to starve.
                    let candidate_generation_id = next_generation_id;
                    next_generation_id += 1;
                    let candidate_envs = vec![
                        ("OBLISK_GENERATION_ID".to_string(), candidate_generation_id.to_string()),
                        ("OBLISK_PBA_CANDIDATE".to_string(), "1".to_string()),
                    ];
                    // Every capability's latest snapshot hydrates the fresh Candidate's first
                    // evaluation (§ 15.2 point 1; docs/adr/0029), not just audio's.
                    let snapshots: Vec<shared::StateSnapshot> = last_snapshots.values().cloned().collect();
                    let mut link = SocketCandidateLink { registry: registry.clone(), candidate_generation_id, inbound: &mut inbound_frames };

                    match reload::run_pba(&renderer_path_str, &[], &candidate_envs, &mut link, &snapshots, sequence, PBA_TIMINGS).await {
                        Ok(outcome) => {
                            // docs/adr/0043 decision 1 item 3: the widest point of the handoff --
                            // the Candidate has presented (run_pba returned Ok) and the superseded
                            // generation still owns every buffer, so both are fully resident.
                            // Sampled here rather than inside the swap, which reaps one of the two
                            // processes it would be measuring.
                            memory::log_sample("pba handoff", &[(authoritative.generation_id, &authoritative.child), (candidate_generation_id, &outcome.candidate)]);
                            reload::swap_and_reap(&registry, &mut processes, &mut authoritative, candidate_generation_id, outcome).await;
                        }
                        Err(failure) => {
                            eprintln!("generation swap for sequence {sequence} failed: {failure}");
                            eprintln!("{} stays authoritative", authoritative.generation_id);
                        }
                    }
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                    eprintln!("generation {}'s shell.lua re-evaluation (sequence {sequence}) failed: {error}", inbound.generation_id);
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "polkit" && submit.action == "authenticate" => {
                    // docs/adr/0028: the polkit-routed case. Must come before the catch-all
                    // SecureSubmit arm below -- match arms are tried in order.
                    match pending_challenge.take() {
                        Some(challenge) => {
                            // mem::take moves the plaintext out for drive_pam_and_respond to own
                            // and zeroize on every return path -- nothing left in submit to zeroize.
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
                    // ADR-0029, mirroring the polkit arm above: an empty secret means an open
                    // network, a non-empty one becomes the wpa-psk password. Must come before the
                    // catch-all arm below, same ordering reason.
                    match network.take_connect_intent() {
                        Some(pending) => {
                            // mem::take moves the plaintext out for NetworkController::connect to
                            // own and zeroize on every return path.
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
                    // The same stale-frame guard the LockReport arm above takes: only the
                    // authoritative generation paints the lock screen a password can have been
                    // typed into. Unlike polkit/network, which take any generation's submission
                    // (they answer a challenge the Supervisor itself holds), a lock belongs to
                    // exactly one generation (docs/adr/0042).
                    eprintln!(
                        "generation {}'s secure_submit(lock, authenticate) is stale -- {} is authoritative; dropping",
                        submit.generation_id, authoritative.generation_id
                    );
                    submit.secret.zeroize();
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "lock" && submit.action == "authenticate" => {
                    // docs/adr/0042, docs/adr/0052: this arm and the pam_outcomes arm above are
                    // the only path to an unlock, making "never unlock_and_destroy except on a
                    // successful authentication" a property of one call site. Must come before the
                    // catch-all arm below. No pending-intent lookup, unlike polkit/network: the
                    // lock screen's submission carries everything needed.
                    //
                    // try_begin_authentication refuses a submit with no lock held (build-steps.md
                    // Phase 23 item 3) and a second submit while one is in flight. The acquisition
                    // it returns is carried through the worker so pam_outcomes above can tell
                    // this lock's answer from the one before it.
                    if let Some(acquisition) = lock.try_begin_authentication() {
                        push_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, "lock", &lock.snapshot());
                        // mem::take moves the plaintext out for run_lock_authentication to own and
                        // zeroize on every exit path, including a panic or shutdown cancellation.
                        let secret = std::mem::take(&mut submit.secret);
                        // Spawned, not awaited inline like the polkit arm: a lock screen's Enter
                        // key is neither bounded nor rare. Blocking this loop for pam_unix's ~2s
                        // failure delay (or PAM_EXCHANGE_TIMEOUT's 30s on a wedged worker) would
                        // stall every LockReport, reload, and process reap behind it.
                        // run_lock_authentication, not authenticate_current_user directly, so
                        // outcome_tx gets a PamOutcome even if this task panics or is dropped.
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
                    // build-steps.md Phase 15 item 2 (ADR-0015 item 2's textfield/IPC half): a
                    // channel-forward-and-log placeholder for every capability/action other than
                    // the guarded arms above, none of which exist yet.
                    //
                    // Never log the secret -- only its length -- and log before zeroizing, so the
                    // length read happens before the clear.
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

    // Reap the authoritative Renderer and every still-live process.run child rather than exit out
    // from under them (SIGTERM-then-SIGKILL, process::DEFAULT_REAP_GRACE).
    // Skipped when the departure arm already collected it: reap_process_group asking for a pid
    // that's already cleared logs "already reaped" right under the crash report that explains it.
    if !renderer_departed
        && let Err(err) = process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await
    {
        eprintln!("failed to reap authoritative generation {}'s renderer on shutdown: {err}", authoritative.generation_id);
    }
    reap_all_processes(&mut processes).await;
    // Best-effort: socket::bind's own stale-file removal covers a missed unlink on the next boot.
    let _ = std::fs::remove_file(&socket_path);
    Ok(shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tripped_restart_brake_exits_with_a_code_a_service_manager_will_not_restart() {
        // docs/adr/0059 decision 3: systemd's default start limit (5 in 10s) is too fast to catch
        // this brake's give-up (three deaths over 60s). The code is what carries the difference.
        assert_ne!(Shutdown::RestartBrakeTripped.exit_code(), Shutdown::Requested.exit_code());
        assert_ne!(Shutdown::RestartBrakeTripped.exit_code(), 0, "a give-up is not a clean exit");
    }

    #[test]
    fn a_requested_shutdown_exits_cleanly_so_a_restart_policy_treats_it_as_one() {
        // SIGTERM at session end must not look like a failure -- `RestartPreventExitStatus` names
        // one code, so every other exit has to mean "restarting me is recovery".
        assert_eq!(Shutdown::Requested.exit_code(), 0);
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

        // Both triggers (the watcher's file change and RequestReload, docs/adr/0041 decision 4)
        // reach this same call, so two of them must produce two distinct, increasing sequences.
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
        // send_frame_logged logs and drops a NoConnection failure. The sequence must advance
        // anyway, or a reconnecting generation's later report could collide with an already-
        // accepted sequence.
        let registry = socket::GenerationRegistry::default();
        let mut next_sequence = 41;
        begin_reload(&registry, 7, &mut next_sequence);
        assert_eq!(next_sequence, 42);
    }
}
