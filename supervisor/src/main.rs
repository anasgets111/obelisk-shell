mod audio;
mod dbus;
mod hardware;
mod pam_worker;
mod privacy;
mod process;
mod reload;
mod reload_link;
mod snapshot;
mod socket;
mod updates;
mod watcher;

use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use dbus::bluetooth::{self, BluetoothController, BluetoothSignal};
use dbus::network::{self, NetworkController, NetworkSignal, PendingNetworkConnect};
use dbus::notifications::{self, NotificationsController, NotificationsSignal};
use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use dbus::tray::{self, TrayController, TraySignal};
use hardware::idle::{self, IdleController};
use hardware::keyboard::{self, KeyboardController, KeyboardSignal};
use hardware::sysinfo::{self, SysinfoController, SysinfoSignal};
use privacy::{PrivacyController, PrivacySignal};
use updates::{UpdatesController, UpdatesSignal};
use process::registry::{
    KillOutcome, LiveProcesses, kill_registered_process, process_run_args, reap_all_processes, reap_generations_processes,
    spawn_and_register_process, stream_process_output, take_exited_process, wait_and_report_exit,
};
use reload_link::SocketCandidateLink;
use shared::{
    ApplyPendingReload, DeselectInput, ProcessExited, PromoteGeneration, RendererFrame, ReevaluateReport, ReevaluateRequest, SupervisorFrame,
    Zeroize,
};
use snapshot::{
    bump_revision, push_bluetooth_snapshot, push_keyboard_snapshot, push_network_snapshot, push_notifications_snapshot, push_privacy_snapshot, push_sysinfo_snapshot,
    push_tray_snapshot, push_updates_snapshot,
};

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
    let network = NetworkController::new(connection.clone()).await?;
    // The Wi-Fi signal forwarder task (see its own doc comment for why this is a separate task
    // rather than raw signal streams merged directly into this function's own `select!`) needs
    // its own clone of the sender; `network_signal_tx` is kept alive here even when there's no
    // Wi-Fi device so the channel never closes (a closed channel would make `.recv()` resolve to
    // `None` on every poll below, busy-looping that arm instead of idling).
    let (network_signal_tx, mut network_signals) = tokio::sync::mpsc::unbounded_channel::<NetworkSignal>();
    match network.wifi_signal_source() {
        Some(wireless) => network::spawn_wifi_signal_forwarder(wireless, network_signal_tx),
        None => {
            eprintln!("network: no Wi-Fi device found; scan/access-point events are disabled for this session");
        }
    }
    let mut network_state = network::NetworkState::default();

    // BlueZ runs on the same system bus too (docs/adr/0030). Unlike NetworkController, the
    // channel is threaded into the constructor itself rather than exposed via a
    // `*_signal_source()` getter spawned separately afterward -- see BluetoothController::new's
    // own doc comment for why (startup hydration itself needs it, not just the forwarders spawned
    // after construction).
    let (bluetooth_signal_tx, mut bluetooth_signals) = tokio::sync::mpsc::unbounded_channel::<BluetoothSignal>();
    let bluetooth = BluetoothController::new(connection.clone(), bluetooth_signal_tx).await;
    let mut bluetooth_state = bluetooth::BluetoothState::default();

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
    std::thread::spawn(move || audio::mixer::run(audio_tx, video_tx));

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
    // The most recently received network:connect(ssid, hidden) intent awaiting a matching
    // secure_submit(network, connect) reply, if any -- mirrors `pending_challenge` above
    // (ADR-0028's pattern, extended to network by ADR-0029).
    let mut pending_network_connect: Option<PendingNetworkConnect> = None;

    // Every `process.run`-spawned child still tracked (docs/adr/0026), plus the channel
    // `stream_process_output`'s background tasks use to report a naturally-exited process back to
    // this loop for reaping and registry cleanup.
    let mut processes: LiveProcesses = HashMap::new();
    let (process_done_tx, mut process_done) = tokio::sync::mpsc::unbounded_channel::<(u32, u64)>();
    // No handler at all previously meant Ctrl-C killed the Supervisor on the spot, leaving the
    // Renderer (a different process group by design, Phase 7) orphaned and running headless --
    // confirmed live. `SIGTERM` gets the same treatment: a process manager stopping this unit
    // sends `SIGTERM`, not `SIGINT`.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
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
                let revision = bump_revision(&mut revisions, "audio");
                match serde_json::to_value(&apps) {
                    Ok(payload) => {
                        let snapshot = shared::StateSnapshot { capability: "audio".to_string(), revision, payload };
                        send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
                        last_snapshots.insert("audio".to_string(), snapshot);
                    }
                    Err(err) => eprintln!("failed to serialize audio StateSnapshot: {err}"),
                }
            }
            Some(signal) = network_signals.recv() => {
                // No debounce (docs/adr/0029): every relevant event fully re-derives the AP list
                // from scratch and pushes a fresh StateSnapshot, even a burst of several in a
                // row from one completed scan -- `revision` already makes an intermediate push
                // harmless.
                if signal == NetworkSignal::ScanCompleted {
                    network_state.scanning = false;
                }
                network_state.available_networks = network.build_available_networks().await;
                push_network_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &network_state);
            }
            Some(signal) = bluetooth_signals.recv() => {
                // No debounce (docs/adr/0030, matching ADR-0029): every relevant event fully
                // re-derives the affected part of the state from scratch.
                match signal {
                    BluetoothSignal::AdapterChanged => {
                        bluetooth_state.enabled = bluetooth.read_enabled().await;
                        bluetooth_state.discovering = bluetooth.read_discovering().await;
                    }
                    BluetoothSignal::DeviceRegistryChanged => {
                        let (connected_devices, discovered_devices) = bluetooth.build_device_lists().await;
                        bluetooth_state.connected_devices = connected_devices;
                        bluetooth_state.discovered_devices = discovered_devices;
                    }
                }
                push_bluetooth_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &bluetooth_state);
            }
            Some(TraySignal::RegistryChanged) = tray_signals.recv() => {
                // No debounce (docs/adr/0031, matching ADR-0029/0030): the registry entry
                // driving this signal is already fully recomputed by the forwarder task that
                // sent it (see dbus::tray's module doc comment) -- build_state is a synchronous
                // snapshot of already-live data, no further D-Bus round trip needed here.
                let tray_state = tray.build_state();
                push_tray_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &tray_state);
            }
            Some(NotificationsSignal::Changed) = notifications_signals.recv() => {
                // No debounce (ADR-0033, matching ADR-0029/0030/0031): every mutation (Notify,
                // dismiss, reply, set_dnd, expiry firing, FIFO eviction) fully re-derives
                // notifications.feed/notifications.dnd from already-live state -- build_state is a
                // synchronous snapshot, no further D-Bus round trip needed here.
                let notifications_state = notifications.build_state();
                push_notifications_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &notifications_state);
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
                push_sysinfo_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &state);
            }
            Some(KeyboardSignal::Changed) = keyboard_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo above -- the backlight
                // forwarder already wrote `backlight_pct` under its own lock before signaling.
                let state = keyboard.snapshot();
                push_keyboard_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &state);
            }
            Some(PrivacySignal::Changed) = privacy_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo/keyboard above.
                let state = privacy.snapshot();
                push_privacy_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &state);
            }
            Some(UpdatesSignal::Changed) = updates_signals.recv() => {
                // Same no-debounce, full-re-derive shape as sysinfo/keyboard/privacy above --
                // fires after both a periodic check and an install's progress updates.
                let state = updates.snapshot();
                push_updates_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &state);
            }
            Some(()) = reload_events.recv() => {
                next_sequence += 1;
                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: next_sequence }));
            }
            Some((generation_id, id)) = process_done.recv() => {
                // The wait itself must never block this select! -- see wait_and_report_exit's
                // own doc comment -- so only the fast, synchronous removal happens inline here.
                if let Some(child) = take_exited_process(&mut processes, generation_id, id) {
                    tokio::spawn(wait_and_report_exit(registry.clone(), generation_id, id, child));
                }
            }
            Some(inbound) = inbound_frames.recv() => match inbound.frame {
                RendererFrame::Command(envelope) if envelope.params.capability == "process" && envelope.params.action == "run" => {
                    let generation_id = envelope.params.generation_id;
                    let id = envelope.id;
                    match process_run_args(&envelope.params.arguments) {
                        Some((cmd, args)) => match spawn_and_register_process(&mut processes, generation_id, id, &cmd, &args) {
                            Some((stdout, stderr)) => {
                                let task_registry = registry.clone();
                                let task_done_tx = process_done_tx.clone();
                                tokio::spawn(async move {
                                    stream_process_output(&task_registry, generation_id, id, stdout, stderr).await;
                                    let _ = task_done_tx.send((generation_id, id));
                                });
                            }
                            None => {
                                send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }));
                            }
                        },
                        None => {
                            eprintln!("malformed process.run command from generation {generation_id}: {:?}", envelope.params.arguments);
                            // Lua's ProcessHandle is already waiting on `id`'s exit_cb -- with no
                            // process ever spawned, nothing else will ever report this id done, so
                            // this is what stops it leaking `pending`'s callback pair forever on
                            // the Renderer side (Correctness review, docs/adr/0026 addendum).
                            send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }));
                        }
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "process" && envelope.params.action == "kill" => {
                    let generation_id = envelope.params.generation_id;
                    let id = envelope.id;
                    match kill_registered_process(&mut processes, generation_id, id).await {
                        KillOutcome::Reaped(code) => {
                            send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
                        }
                        KillOutcome::ReapFailed => {
                            // The registry entry is already removed by this point (see
                            // kill_registered_process) and the OS-level reap failure is already
                            // logged -- no future event will ever report this id done, so this is
                            // what stops it leaking `pending`'s callback pair forever on the
                            // Renderer side (Correctness review, docs/adr/0026 addendum). The real
                            // exit code is unknowable here; `None` is honest, not synthesized.
                            send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }));
                        }
                        KillOutcome::NotRegistered => {}
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "network" && envelope.params.action == "set_networking_enabled" => {
                    match network::parse_bool_arg(&envelope.params.arguments) {
                        Some(enabled) => {
                            let controller = network.clone();
                            tokio::spawn(async move { controller.set_networking_enabled(enabled).await; });
                        }
                        None => eprintln!(
                            "malformed network.set_networking_enabled command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "network" && envelope.params.action == "set_wifi_enabled" => {
                    match network::parse_bool_arg(&envelope.params.arguments) {
                        Some(enabled) => {
                            let controller = network.clone();
                            tokio::spawn(async move { controller.set_wifi_enabled(enabled).await; });
                        }
                        None => eprintln!(
                            "malformed network.set_wifi_enabled command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "network" && envelope.params.action == "set_ethernet_enabled" => {
                    match network::parse_bool_arg(&envelope.params.arguments) {
                        Some(enabled) => {
                            let controller = network.clone();
                            tokio::spawn(async move { controller.set_ethernet_enabled(enabled).await; });
                        }
                        None => eprintln!(
                            "malformed network.set_ethernet_enabled command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "network" && envelope.params.action == "scan" => {
                    // "network.scanning" flips to true on initiation, not once RequestScan's
                    // D-Bus round trip completes (docs/oblisk-supervisor-services-dbus.md §4.2)
                    // -- the actual call is what ADR-0029 says must be tokio::spawn'ed, not this
                    // local state flip. Only when a Wi-Fi device actually exists, though
                    // (Correctness review): with none, `NetworkController::scan` silently no-ops
                    // and no `ScanCompleted` signal will ever arrive (no wifi_signal_forwarder was
                    // spawned for this session either) to flip `scanning` back to `false` --
                    // leaving it stuck `true` forever.
                    if network.has_wifi_device() {
                        network_state.scanning = true;
                        push_network_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &network_state);
                    }
                    let controller = network.clone();
                    tokio::spawn(async move { controller.scan().await; });
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "network" && envelope.params.action == "connect" => {
                    match network::parse_connect_args(&envelope.params.arguments) {
                        Some((ssid, hidden)) => pending_network_connect = Some(PendingNetworkConnect { ssid, hidden }),
                        None => eprintln!(
                            "malformed network.connect command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "network" && envelope.params.action == "forget" => {
                    match network::parse_ssid_arg(&envelope.params.arguments) {
                        Some(ssid) => {
                            let controller = network.clone();
                            tokio::spawn(async move { controller.forget(&ssid).await; });
                        }
                        None => eprintln!(
                            "malformed network.forget command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "set_enabled" => {
                    match bluetooth::parse_bool_arg(&envelope.params.arguments) {
                        Some(enabled) => {
                            let controller = bluetooth.clone();
                            tokio::spawn(async move { controller.set_enabled(enabled).await; });
                        }
                        None => eprintln!(
                            "malformed bluetooth.set_enabled command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "start_discovery" => {
                    // discovered_devices clears immediately, before the D-Bus call's own
                    // completion (docs/adr/0030, matching NM's scanning = true immediate-flip
                    // pattern) -- the actual StartDiscovery call is what must be tokio::spawn'ed,
                    // not this local state flip.
                    bluetooth_state.discovered_devices = Vec::new();
                    push_bluetooth_snapshot(&registry, authoritative.generation_id, &mut revisions, &mut last_snapshots, &bluetooth_state);
                    let controller = bluetooth.clone();
                    tokio::spawn(async move { controller.start_discovery().await; });
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "stop_discovery" => {
                    // The last discovered_devices snapshot stays visible (docs/adr/0030) -- no
                    // local state mutation here, only the D-Bus call.
                    let controller = bluetooth.clone();
                    tokio::spawn(async move { controller.stop_discovery().await; });
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "pair" => {
                    match bluetooth::parse_mac_arg(&envelope.params.arguments) {
                        Some(mac) => {
                            let controller = bluetooth.clone();
                            tokio::spawn(async move { controller.pair(&mac).await; });
                        }
                        None => eprintln!(
                            "malformed bluetooth.pair command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "connect" => {
                    match bluetooth::parse_mac_arg(&envelope.params.arguments) {
                        Some(mac) => {
                            let controller = bluetooth.clone();
                            tokio::spawn(async move { controller.connect(&mac).await; });
                        }
                        None => eprintln!(
                            "malformed bluetooth.connect command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "disconnect" => {
                    match bluetooth::parse_mac_arg(&envelope.params.arguments) {
                        Some(mac) => {
                            let controller = bluetooth.clone();
                            tokio::spawn(async move { controller.disconnect(&mac).await; });
                        }
                        None => eprintln!(
                            "malformed bluetooth.disconnect command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "bluetooth" && envelope.params.action == "forget" => {
                    match bluetooth::parse_mac_arg(&envelope.params.arguments) {
                        Some(mac) => {
                            let controller = bluetooth.clone();
                            tokio::spawn(async move { controller.forget(&mac).await; });
                        }
                        None => eprintln!(
                            "malformed bluetooth.forget command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "tray" && envelope.params.action == "activate" => {
                    match tray::parse_activate_args(&envelope.params.arguments) {
                        Some((id, x, y)) => {
                            let controller = tray.clone();
                            tokio::spawn(async move { controller.activate(&id, x, y).await; });
                        }
                        None => eprintln!(
                            "malformed tray.activate command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "tray" && envelope.params.action == "activate_menu_item" => {
                    match tray::parse_activate_menu_item_args(&envelope.params.arguments) {
                        Some((id, menu_item_id)) => {
                            let controller = tray.clone();
                            tokio::spawn(async move { controller.activate_menu_item(&id, menu_item_id).await; });
                        }
                        None => eprintln!(
                            "malformed tray.activate_menu_item command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "tray" && envelope.params.action == "menu_will_show" => {
                    match tray::parse_menu_will_show_args(&envelope.params.arguments) {
                        Some((id, submenu_id)) => {
                            let controller = tray.clone();
                            tokio::spawn(async move { controller.menu_will_show(&id, submenu_id).await; });
                        }
                        None => eprintln!(
                            "malformed tray.menu_will_show command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "idle" && envelope.params.action == "register" => {
                    match idle::parse_register_args(&envelope.params.arguments) {
                        Some(sec) => {
                            let controller = idle.clone();
                            let generation_id = envelope.params.generation_id;
                            tokio::spawn(async move { controller.register_threshold(generation_id, sec).await; });
                        }
                        None => eprintln!(
                            "malformed idle.register command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "idle" && envelope.params.action == "inhibit" => {
                    match idle::parse_inhibit_args(&envelope.params.arguments) {
                        Some(reason) => {
                            let controller = idle.clone();
                            let generation_id = envelope.params.generation_id;
                            tokio::spawn(async move { controller.inhibit(generation_id, &reason).await; });
                        }
                        None => eprintln!(
                            "malformed idle.inhibit command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "idle" && envelope.params.action == "release_inhibit" => {
                    let controller = idle.clone();
                    let generation_id = envelope.params.generation_id;
                    tokio::spawn(async move { controller.release_inhibit(generation_id).await; });
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "sysinfo" && envelope.params.action == "configure" => {
                    match sysinfo::parse_configure_args(&envelope.params.arguments) {
                        Some(cfg) => sysinfo.configure(cfg),
                        None => eprintln!(
                            "malformed sysinfo.configure command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "keyboard" && envelope.params.action == "set_backlight" => {
                    match keyboard::parse_set_backlight_args(&envelope.params.arguments) {
                        Some(pct) => {
                            let controller = keyboard.clone();
                            tokio::spawn(async move { controller.set_backlight(pct).await; });
                        }
                        None => eprintln!(
                            "malformed keyboard.set_backlight command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "updates" && envelope.params.action == "configure" => {
                    match updates::parse_configure_args(&envelope.params.arguments) {
                        Some(interval_secs) => updates.configure(interval_secs),
                        None => eprintln!(
                            "malformed updates.configure command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "updates" && envelope.params.action == "install" => {
                    let controller = updates.clone();
                    tokio::spawn(async move { controller.install().await; });
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "notifications" && envelope.params.action == "dismiss" => {
                    match notifications::parse_dismiss_args(&envelope.params.arguments) {
                        Some(id) => {
                            let controller = notifications.clone();
                            tokio::spawn(async move { controller.dismiss(id).await; });
                        }
                        None => eprintln!(
                            "malformed notifications.dismiss command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "notifications" && envelope.params.action == "reply" => {
                    match notifications::parse_reply_args(&envelope.params.arguments) {
                        Some((id, text)) => {
                            let controller = notifications.clone();
                            tokio::spawn(async move { controller.reply(id, text).await; });
                        }
                        None => eprintln!(
                            "malformed notifications.reply command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "notifications" && envelope.params.action == "set_sound" => {
                    match notifications::parse_set_sound_args(&envelope.params.arguments) {
                        Some((urgency, path)) => notifications.set_sound(urgency, &path),
                        None => eprintln!(
                            "malformed notifications.set_sound command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "notifications" && envelope.params.action == "set_dnd" => {
                    match dbus::parse_bool_arg(&envelope.params.arguments) {
                        Some(enabled) => notifications.set_dnd(enabled),
                        None => eprintln!(
                            "malformed notifications.set_dnd command from generation {}: {:?}",
                            envelope.params.generation_id, envelope.params.arguments
                        ),
                    }
                }
                RendererFrame::Command(envelope) => {
                    eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope);
                }
                RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_) => {
                    // Both only matter mid-handshake, where `SocketCandidateLink` reads them
                    // directly off `inbound_frames` itself (see `TopologyChanged` below, and
                    // docs/adr/0025). One reaching this top-level match means it arrived
                    // *outside* any in-flight handshake this Supervisor is currently driving --
                    // stale, or a wire-protocol desync -- logged, not fatal.
                    eprintln!("generation {}'s handshake frame arrived outside any in-flight PBA handshake; dropping: {:?}", inbound.generation_id, inbound.frame);
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
                    match pending_network_connect.take() {
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
                RendererFrame::SecureSubmit(mut submit) => {
                    // build-steps.md Phase 15 item 2 closes ADR-0015 item 2 (the textfield/IPC
                    // half only) -- this is deliberately still just a channel-forward-and-log
                    // placeholder, the same discipline this codebase already uses for
                    // `challenges.recv()` above. The `("polkit", "authenticate")` and
                    // `("network", "connect")` cases are now handled by the guarded arms above
                    // (docs/adr/0028 Phase 15 item 3; docs/adr/0029); this fallback covers every
                    // other capability/action, none of which exist yet.
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
    if let Err(err) = process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await {
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
    use super::*;

    #[test]
    fn is_current_reload_matches_only_the_most_recently_sent_sequence() {
        assert!(is_current_reload(3, 3));
        assert!(!is_current_reload(3, 4), "a report for an older sequence than the last-sent one must be stale");
    }

}
