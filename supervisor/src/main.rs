mod audio;
mod dbus;
mod pam_worker;
mod process;
mod reload;
mod reload_link;
mod socket;
mod watcher;

use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use dbus::bluetooth::{self, BluetoothController, BluetoothSignal};
use dbus::network::{self, NetworkController, NetworkSignal, PendingNetworkConnect};
use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use reload_link::SocketCandidateLink;
use shared::{
    ApplyPendingReload, DeselectInput, ProcessExited, ProcessOutputLine, ProcessStream, PromoteGeneration, RendererFrame, ReevaluateReport,
    ReevaluateRequest, SupervisorFrame, Zeroize,
};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};

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

/// ADR-0006: an in-place reload must drop any Supervisor-held registrations tied to
/// `generation_id` before the fresh evaluation is applied, so a re-issued `idle:register_threshold`
/// (etc.) reads as a replacement, not a duplicate leak. No capability registers anything against
/// a `generation_id` yet -- the D-Bus/hardware controllers that would populate this (Phase 16)
/// don't exist -- so this is a real, called, currently-empty seam, matching this codebase's
/// established real-but-unwired precedent (`socket::GenerationRegistry` itself sat exactly like
/// this through Phase 9-10, see docs/adr/0020). See docs/adr/0024 item 2.
fn reset_registrations(_generation_id: u32) {}

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

/// Bumps and returns `capability`'s own state-version counter (ADR-0004; docs/adr/0029
/// generalizes the old single `audio_revision: u32` into this map, keyed by capability name).
/// Starts at `1` for a capability's first-ever push, matching the old `audio_revision`'s own
/// `0`-initialized-then-pre-incremented behavior.
fn bump_revision(revisions: &mut HashMap<String, u32>, capability: &str) -> u32 {
    let revision = revisions.entry(capability.to_string()).or_insert(0);
    *revision += 1;
    *revision
}

/// Bumps `"network"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- the one place every network-capability push in `run_supervisor`'s
/// `select!` goes through, so the bump-then-serialize-then-send sequence only lives once. Also
/// records the pushed snapshot in `last_snapshots` (docs/adr/0029), the same per-capability
/// hydration map a freshly-promoted PBA candidate is seeded from.
fn push_network_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &network::NetworkState,
) {
    let revision = bump_revision(revisions, "network");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "network".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("network".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize network StateSnapshot: {err}"),
    }
}

/// Bumps `"bluetooth"`'s revision and pushes `state` as a fresh `StateSnapshot` to the
/// authoritative generation -- mirrors [`push_network_snapshot`] exactly (docs/adr/0030 needs
/// zero new plumbing beyond a fresh capability name flowing through ADR-0029's already-generic
/// `revisions`/`last_snapshots` maps).
fn push_bluetooth_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    revisions: &mut HashMap<String, u32>,
    last_snapshots: &mut HashMap<String, shared::StateSnapshot>,
    state: &bluetooth::BluetoothState,
) {
    let revision = bump_revision(revisions, "bluetooth");
    match serde_json::to_value(state) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot { capability: "bluetooth".to_string(), revision, payload };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
            last_snapshots.insert("bluetooth".to_string(), snapshot);
        }
        Err(err) => eprintln!("failed to serialize bluetooth StateSnapshot: {err}"),
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

/// Every `process.run`-spawned child still tracked, keyed by the generation that spawned it and
/// the `CommandEnvelope.id` the Renderer assigned it (docs/adr/0026). A plain local, mutated only
/// from inside `main()`'s own `select!` arms -- not behind a mutex itself, matching every other
/// piece of cross-task *spawn-tracking* state in this file (ADR-0018's promised upgrade path).
/// (`socket::GenerationRegistry`'s own `connections` map is a pre-existing `Arc<Mutex<...>>`,
/// unrelated to this registry -- that one's shared across the listener's accept loop and every
/// connection task, a different problem than this one solves.)
type LiveProcesses = HashMap<(u32, u64), Child>;

/// Parses `process.run`'s `CommandEnvelope.params.arguments` -- `[cmd, args]`, `cmd` a string and
/// `args` an array of strings, the shape `renderer/src/lua/process.rs`'s `ProcessRegistry::run`
/// sends. `None` on any shape mismatch (a protocol desync, not a spawn failure -- logged by the
/// caller, no `ProcessExited` sent back since there's no `id` this parse can even attribute one
/// to reliably beyond what the envelope itself already carries).
fn process_run_args(arguments: &[serde_json::Value]) -> Option<(String, Vec<String>)> {
    let cmd = arguments.first()?.as_str()?.to_string();
    let args = arguments.get(1)?.as_array()?.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    Some((cmd, args))
}

/// `("process", "run")`'s spawn step: pipes stdout/stderr (`process::spawn_group_leader_piped`),
/// takes the piped handles off the `Child` before registering it, so `processes` can keep owning
/// the `Child` (for `kill`/supersede-reap) while a separate task reads its output. Logs and
/// returns `None` on spawn failure -- the caller still owes Lua a `ProcessExited` with an absent
/// code (§ 12).
fn spawn_and_register_process(
    processes: &mut LiveProcesses,
    generation_id: u32,
    id: u64,
    cmd: &str,
    args: &[String],
) -> Option<(ChildStdout, ChildStderr)> {
    match process::spawn_group_leader_piped(cmd, args, &[]) {
        Ok(mut child) => {
            let stdout = child.stdout.take().expect("spawn_group_leader_piped always pipes stdout");
            let stderr = child.stderr.take().expect("spawn_group_leader_piped always pipes stderr");
            processes.insert((generation_id, id), child);
            Some((stdout, stderr))
        }
        Err(err) => {
            eprintln!("process.run({cmd:?}, {args:?}) failed to spawn: {err}");
            None
        }
    }
}

/// Reads `stdout`/`stderr` concurrently, line-by-line, forwarding each as
/// `SupervisorFrame::ProcessOutput` through `registry` directly -- the same
/// `registry.clone()`-into-a-task pattern `SocketCandidateLink` already uses, so this doesn't
/// need to round-trip through `main()`'s `select!` for output. Runs until both streams hit EOF
/// (the process has exited or is exiting), then reports `(generation_id, id)` on
/// `process_done_tx` so `main()` can collect the real exit code and drop the registry entry --
/// this task never owns the `Child` itself, so it can't call `wait()` for that code directly.
async fn stream_process_output(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    stdout: ChildStdout,
    stderr: ChildStderr,
) {
    let mut stdout_lines = tokio::io::BufReader::new(stdout).lines();
    let mut stderr_lines = tokio::io::BufReader::new(stderr).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;

    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout_lines.next_line(), if !stdout_done => {
                stdout_done = report_process_output_line(registry, generation_id, id, ProcessStream::Stdout, line);
            }
            line = stderr_lines.next_line(), if !stderr_done => {
                stderr_done = report_process_output_line(registry, generation_id, id, ProcessStream::Stderr, line);
            }
        }
    }
}

/// One `stream_process_output` poll's outcome, shared by its stdout/stderr arms: sends a
/// `ProcessOutput` frame for a real line, logs a read error, and reports back whether this stream
/// is now done (EOF or error) so the caller can stop polling it.
fn report_process_output_line(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    stream: ProcessStream,
    line: io::Result<Option<String>>,
) -> bool {
    match line {
        Ok(Some(line)) => {
            send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessOutput(ProcessOutputLine { id, stream, line }));
            false
        }
        Ok(None) => true,
        Err(err) => {
            eprintln!("process {id} (generation {generation_id}): failed to read {stream:?}: {err}");
            true
        }
    }
}

/// What `("process", "kill")` found for `(generation_id, id)`.
#[derive(Debug)]
enum KillOutcome {
    /// Nothing was registered under this id -- already exited and reaped via the completion
    /// channel, or an id Lua never actually got a handle for.
    NotRegistered,
    /// The process group was reaped; report this exit code back to Lua.
    Reaped(Option<i32>),
    /// `reap_process_group` itself failed (already logged).
    ReapFailed,
}

/// `("process", "kill")`'s handler: removes `(generation_id, id)` and reaps its process group via
/// the already-built `process::reap_process_group` -- ADR-0018's promised real caller for it.
/// `reap_process_group`'s returned `ExitStatus` already carries the real code (`None` here in the
/// ordinary case, since `SIGTERM`/`SIGKILL` are signal deaths), reused directly so a killed
/// process's `exit_cb` still fires with an honest code instead of a synthesized one.
async fn kill_registered_process(processes: &mut LiveProcesses, generation_id: u32, id: u64) -> KillOutcome {
    let Some(mut child) = processes.remove(&(generation_id, id)) else {
        return KillOutcome::NotRegistered;
    };
    match process::reap_process_group(&mut child, process::DEFAULT_REAP_GRACE).await {
        Ok(process::ReapOutcome::ExitedCleanly(status) | process::ReapOutcome::Escalated(status)) => KillOutcome::Reaped(status.code()),
        Err(err) => {
            eprintln!("failed to reap process {id} (generation {generation_id}) on kill: {err}");
            KillOutcome::ReapFailed
        }
    }
}

/// `process_done`'s handler, fast half: `stream_process_output` reported that `(generation_id,
/// id)`'s streams closed. Only removes the registry entry -- never awaits -- so it's safe to call
/// directly inside `main()`'s `select!` (see [`wait_and_report_exit`] for why the actual `wait()`
/// must not happen here).
fn take_exited_process(processes: &mut LiveProcesses, generation_id: u32, id: u64) -> Option<Child> {
    processes.remove(&(generation_id, id))
}

/// `process_done`'s handler, slow half: waits for `child`'s real exit and reports it to Lua via
/// `registry`. Always run as a detached `tokio::spawn`ed task, never awaited inline inside
/// `main()`'s `select!` -- both piped streams closing only means the process *stopped writing to
/// them*, not that it has exited: a process can close or redirect its own stdout/stderr (a
/// daemonizing child, `exec 1>&- 2>&-`, dup2 onto `/dev/null`) while continuing to run
/// indefinitely. `child.wait()` in that case never returns, and awaiting it inline in `main()`'s
/// single top-level `select!` would starve every other arm -- every inbound command, every
/// reload, every generation swap -- for as long as that process keeps running (Correctness
/// review, docs/adr/0026 addendum).
async fn wait_and_report_exit(registry: socket::GenerationRegistry, generation_id: u32, id: u64, mut child: Child) {
    match child.wait().await {
        Ok(status) => send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: status.code() })),
        Err(err) => eprintln!("failed to wait on exited process {id} (generation {generation_id}): {err}"),
    }
}

/// § 12's SIGTERM-then-SIGKILL group reap, applied to every process the superseded generation's
/// Lua spawned -- not just its own Renderer process (`CONTEXT.md`'s Generation swap). No
/// `ProcessExited` is sent for these: the superseded generation's own connection is being torn
/// down in the same swap, so there's no live Lua VM left to receive it.
async fn reap_generations_processes(processes: &mut LiveProcesses, generation_id: u32) {
    let stale_ids: Vec<(u32, u64)> = processes.keys().filter(|(entry_generation_id, _)| *entry_generation_id == generation_id).copied().collect();
    for key in stale_ids {
        if let Some(mut child) = processes.remove(&key)
            && let Err(err) = process::reap_process_group(&mut child, process::DEFAULT_REAP_GRACE).await
        {
            eprintln!("failed to reap process {key:?} belonging to superseded generation {generation_id}: {err}");
        }
    }
}

/// Every still-tracked `process.run` child, regardless of which generation spawned it -- the
/// shutdown-time counterpart to `reap_generations_processes`' narrower per-generation sweep.
/// Without this, a `SIGINT`/`SIGTERM`'d Supervisor previously left every live `process.run` child
/// (and the authoritative Renderer itself, reaped separately by `main`'s own shutdown sequence)
/// orphaned -- confirmed live: Ctrl-C during `cargo run -p supervisor` killed the Supervisor
/// instantly (no signal handler existed at all) while its boot-spawned Renderer, in its own
/// process group since Phase 7 specifically so it survives ambient signals, kept running headless
/// forever.
async fn reap_all_processes(processes: &mut LiveProcesses) {
    let ids: Vec<(u32, u64)> = processes.keys().copied().collect();
    for key in ids {
        if let Some(mut child) = processes.remove(&key)
            && let Err(err) = process::reap_process_group(&mut child, process::DEFAULT_REAP_GRACE).await
        {
            eprintln!("failed to reap process {key:?} on shutdown: {err}");
        }
    }
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

    let (audio_tx, mut audio_apps) = tokio::sync::mpsc::unbounded_channel();
    // pipewire-rs's event loop is Rc-based and single-threaded (not Send) -- it needs its
    // own OS thread, not a tokio task.
    std::thread::spawn(move || audio::mixer::run(audio_tx));

    let socket_path = shared::control_socket_path()?;
    let (registry, mut inbound_frames, mut connected) = socket::spawn_listener(&socket_path)?;

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
                        reset_registrations(inbound.generation_id);
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
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    #[test]
    fn is_current_reload_matches_only_the_most_recently_sent_sequence() {
        assert!(is_current_reload(3, 3));
        assert!(!is_current_reload(3, 4), "a report for an older sequence than the last-sent one must be stale");
    }

    #[test]
    fn process_run_args_parses_cmd_and_args_from_arguments() {
        let arguments = vec![serde_json::json!("echo"), serde_json::json!(["hello", "world"])];
        assert_eq!(process_run_args(&arguments), Some(("echo".to_string(), vec!["hello".to_string(), "world".to_string()])));
    }

    #[test]
    fn process_run_args_rejects_a_malformed_shape() {
        assert_eq!(process_run_args(&[]), None, "missing both elements");
        assert_eq!(process_run_args(&[serde_json::json!(1), serde_json::json!([])]), None, "cmd is not a string");
        assert_eq!(process_run_args(&[serde_json::json!("echo"), serde_json::json!("not-an-array")]), None, "args is not an array");
        assert_eq!(process_run_args(&[serde_json::json!("echo"), serde_json::json!([1, 2])]), None, "args contains non-strings");
    }

    fn registry_with_connection(generation_id: u32) -> (socket::GenerationRegistry, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let registry = socket::GenerationRegistry::default();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        registry.register(generation_id, tx);
        (registry, rx)
    }

    fn sh_args(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    #[tokio::test]
    async fn spawn_and_register_process_registers_a_successful_spawn_and_returns_piped_handles() {
        let mut processes: LiveProcesses = HashMap::new();
        let handles = spawn_and_register_process(&mut processes, 1, 7, "true", &[]);
        assert!(handles.is_some());
        assert!(processes.contains_key(&(1, 7)));
    }

    #[tokio::test]
    async fn spawn_and_register_process_registers_nothing_on_a_spawn_failure() {
        let mut processes: LiveProcesses = HashMap::new();
        let handles = spawn_and_register_process(&mut processes, 1, 7, "/no/such/binary", &[]);
        assert!(handles.is_none());
        assert!(!processes.contains_key(&(1, 7)));
    }

    #[tokio::test]
    async fn stream_process_output_forwards_both_streams_then_the_real_exit_code_arrives_via_wait() {
        let mut processes: LiveProcesses = HashMap::new();
        let (stdout, stderr) =
            spawn_and_register_process(&mut processes, 1, 9, "sh", &sh_args("echo out1; echo err1 >&2; exit 3")).unwrap();
        let (registry, mut rx) = registry_with_connection(1);

        stream_process_output(&registry, 1, 9, stdout, stderr).await;

        let mut lines = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            match serde_json::from_slice::<SupervisorFrame>(&payload).unwrap() {
                SupervisorFrame::ProcessOutput(line) => lines.push(line),
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|l| l.id == 9 && l.stream == ProcessStream::Stdout && l.line == "out1"));
        assert!(lines.iter().any(|l| l.id == 9 && l.stream == ProcessStream::Stderr && l.line == "err1"));

        // stream_process_output never learns the exit code itself (it doesn't own the Child) --
        // main()'s process_done arm takes the Child (take_exited_process) and hands it to a
        // detached task (wait_and_report_exit) for the real wait, never inline.
        let mut child = take_exited_process(&mut processes, 1, 9).expect("process must still be registered");
        assert_eq!(child.wait().await.unwrap().code(), Some(3));
    }

    #[tokio::test]
    async fn take_exited_process_on_an_unregistered_id_returns_none() {
        let mut processes: LiveProcesses = HashMap::new();
        assert!(take_exited_process(&mut processes, 1, 1).is_none());
    }

    #[tokio::test]
    async fn wait_and_report_exit_sends_the_real_exit_code_back_to_the_generation_that_spawned_it() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 9, "sh", &sh_args("exit 7"));
        let child = take_exited_process(&mut processes, 1, 9).unwrap();
        let (registry, mut rx) = registry_with_connection(1);

        wait_and_report_exit(registry, 1, 9, child).await;

        let payload = rx.try_recv().expect("a ProcessExited frame must have been sent");
        match serde_json::from_slice::<SupervisorFrame>(&payload).unwrap() {
            SupervisorFrame::ProcessExited(ProcessExited { id, code }) => {
                assert_eq!(id, 9);
                assert_eq!(code, Some(7));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_registered_process_reaps_and_reports_a_signal_death() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 3, "sh", &sh_args("sleep 5"));
        assert!(processes.contains_key(&(1, 3)));

        match kill_registered_process(&mut processes, 1, 3).await {
            KillOutcome::Reaped(code) => assert_eq!(code, None, "a SIGTERM/SIGKILL death has no exit code"),
            other => panic!("expected Reaped, got {other:?}"),
        }
        assert!(!processes.contains_key(&(1, 3)));
    }

    #[tokio::test]
    async fn kill_registered_process_on_an_unregistered_id_is_a_silent_no_op() {
        let mut processes: LiveProcesses = HashMap::new();
        assert!(matches!(kill_registered_process(&mut processes, 1, 99).await, KillOutcome::NotRegistered));
    }

    #[tokio::test]
    async fn reap_generations_processes_reaps_only_the_matching_generations_entries() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 5"));
        spawn_and_register_process(&mut processes, 2, 1, "sh", &sh_args("sleep 5"));

        reap_generations_processes(&mut processes, 1).await;

        assert!(!processes.contains_key(&(1, 1)), "generation 1's process must be reaped and removed");
        assert!(processes.contains_key(&(2, 1)), "generation 2's process must be untouched");

        kill_registered_process(&mut processes, 2, 1).await;
    }

    #[tokio::test]
    async fn reap_generations_processes_actually_kills_the_process_not_just_the_registry_entry() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 5"));
        let pid = processes.get(&(1, 1)).unwrap().id().expect("freshly spawned child has a pid");

        reap_generations_processes(&mut processes, 1).await;

        let gone = tokio::time::timeout(Duration::from_millis(500), async {
            while Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "process {pid} should be gone after the supersede-time reap, not just removed from the registry");
    }

    #[tokio::test]
    async fn reap_all_processes_reaps_every_generations_entries() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 5"));
        spawn_and_register_process(&mut processes, 2, 1, "sh", &sh_args("sleep 5"));

        reap_all_processes(&mut processes).await;

        assert!(processes.is_empty(), "shutdown must reap every tracked process, not just one generation's");
    }

    #[tokio::test]
    async fn reap_all_processes_actually_kills_the_processes_not_just_the_registry_entries() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 5"));
        spawn_and_register_process(&mut processes, 2, 1, "sh", &sh_args("sleep 5"));
        let pids: Vec<u32> = processes.values().map(|child| child.id().expect("freshly spawned child has a pid")).collect();

        reap_all_processes(&mut processes).await;

        let gone = tokio::time::timeout(Duration::from_millis(500), async {
            while pids.iter().any(|pid| Path::new(&format!("/proc/{pid}")).exists()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "every reaped process should actually be gone from /proc, not just removed from the registry");
    }
}
