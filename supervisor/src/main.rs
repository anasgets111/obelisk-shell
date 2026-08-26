mod audio;
mod dbus;
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

use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use reload_link::SocketCandidateLink;
use shared::{
    ApplyPendingReload, DeselectInput, ProcessExited, ProcessOutputLine, ProcessStream, PromoteGeneration, RendererFrame, ReevaluateReport,
    ReevaluateRequest, SupervisorFrame,
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

/// Every `process.run`-spawned child still tracked, keyed by the generation that spawned it and
/// the `CommandEnvelope.id` the Renderer assigned it (docs/adr/0026). A plain local, not an
/// `Arc<Mutex<...>>` -- mutated only from inside `main()`'s own `select!` arms, matching every
/// other piece of cross-task state in this file (ADR-0018's promised upgrade path).
type ProcessRegistry = HashMap<(u32, u64), Child>;

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
    processes: &mut ProcessRegistry,
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
async fn kill_registered_process(processes: &mut ProcessRegistry, generation_id: u32, id: u64) -> KillOutcome {
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

/// `process_done`'s handler: `stream_process_output` already reported that `(generation_id, id)`'s
/// streams closed, meaning the process has exited or is exiting right now -- `wait()` is safe to
/// await inline here rather than a real block. Returns the real exit code, or `None` if nothing
/// was registered (a `kill` that raced the same exit already removed it) or the wait itself
/// failed (logged).
async fn reap_exited_process(processes: &mut ProcessRegistry, generation_id: u32, id: u64) -> Option<Option<i32>> {
    let mut child = processes.remove(&(generation_id, id))?;
    match child.wait().await {
        Ok(status) => Some(status.code()),
        Err(err) => {
            eprintln!("failed to wait on exited process {id} (generation {generation_id}): {err}");
            None
        }
    }
}

/// § 12's SIGTERM-then-SIGKILL group reap, applied to every process the superseded generation's
/// Lua spawned -- not just its own Renderer process (`CONTEXT.md`'s Generation swap). No
/// `ProcessExited` is sent for these: the superseded generation's own connection is being torn
/// down in the same swap, so there's no live Lua VM left to receive it.
async fn reap_generations_processes(processes: &mut ProcessRegistry, generation_id: u32) {
    let stale_ids: Vec<(u32, u64)> = processes.keys().filter(|(entry_generation_id, _)| *entry_generation_id == generation_id).copied().collect();
    for key in stale_ids {
        if let Some(mut child) = processes.remove(&key)
            && let Err(err) = process::reap_process_group(&mut child, process::DEFAULT_REAP_GRACE).await
        {
            eprintln!("failed to reap process {key:?} belonging to superseded generation {generation_id}: {err}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let connection = zbus::Connection::system().await?;
    let subject = current_session_subject()?;

    let (tx, mut challenges) = tokio::sync::mpsc::unbounded_channel();
    let agent = AuthenticationAgent::new(tx);
    register_agent(&connection, agent, &subject, "en_US.UTF-8", AGENT_OBJECT_PATH).await?;

    let (audio_tx, mut audio_apps) = tokio::sync::mpsc::unbounded_channel();
    // pipewire-rs's event loop is Rc-based and single-threaded (not Send) -- it needs its
    // own OS thread, not a tokio task.
    std::thread::spawn(move || audio::mixer::run(audio_tx));

    let socket_path = shared::control_socket_path()?;
    let (registry, mut inbound_frames) = socket::spawn_listener(&socket_path)?;

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

    // The last audio `StateSnapshot` pushed to the authoritative generation, reused to hydrate a
    // fresh Candidate's first evaluation (§ 15.2 point 1) -- or a fresh revision-0 empty one if
    // none has ever been pushed yet.
    let mut last_audio_snapshot: Option<shared::StateSnapshot> = None;
    let mut audio_revision: u32 = 0;

    // Every `process.run`-spawned child still tracked (docs/adr/0026), plus the channel
    // `stream_process_output`'s background tasks use to report a naturally-exited process back to
    // this loop for reaping and registry cleanup.
    let mut processes: ProcessRegistry = HashMap::new();
    let (process_done_tx, mut process_done) = tokio::sync::mpsc::unbounded_channel::<(u32, u64)>();
    loop {
        tokio::select! {
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
            }
            Some(apps) = audio_apps.recv() => {
                audio_revision += 1;
                match serde_json::to_value(&apps) {
                    Ok(payload) => {
                        let snapshot = shared::StateSnapshot { revision: audio_revision, payload };
                        send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
                        last_audio_snapshot = Some(snapshot);
                    }
                    Err(err) => eprintln!("failed to serialize audio StateSnapshot: {err}"),
                }
            }
            Some(()) = reload_events.recv() => {
                next_sequence += 1;
                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: next_sequence }));
            }
            Some((generation_id, id)) = process_done.recv() => {
                if let Some(code) = reap_exited_process(&mut processes, generation_id, id).await {
                    send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
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
                        None => eprintln!("malformed process.run command from generation {generation_id}: {:?}", envelope.params.arguments),
                    }
                }
                RendererFrame::Command(envelope) if envelope.params.capability == "process" && envelope.params.action == "kill" => {
                    let generation_id = envelope.params.generation_id;
                    let id = envelope.id;
                    match kill_registered_process(&mut processes, generation_id, id).await {
                        KillOutcome::Reaped(code) => {
                            send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
                        }
                        KillOutcome::NotRegistered | KillOutcome::ReapFailed => {}
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
                    let snapshot = last_audio_snapshot.clone().unwrap_or(shared::StateSnapshot { revision: 0, payload: serde_json::json!({}) });
                    let mut link = SocketCandidateLink { registry: registry.clone(), candidate_generation_id, inbound: &mut inbound_frames };

                    match reload::run_pba(&renderer_path_str, &[], &candidate_envs, &mut link, &snapshot, sequence, PBA_TIMINGS).await {
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
            },
            else => break,
        }
    }
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
        let mut processes: ProcessRegistry = HashMap::new();
        let handles = spawn_and_register_process(&mut processes, 1, 7, "true", &[]);
        assert!(handles.is_some());
        assert!(processes.contains_key(&(1, 7)));
    }

    #[tokio::test]
    async fn spawn_and_register_process_registers_nothing_on_a_spawn_failure() {
        let mut processes: ProcessRegistry = HashMap::new();
        let handles = spawn_and_register_process(&mut processes, 1, 7, "/no/such/binary", &[]);
        assert!(handles.is_none());
        assert!(!processes.contains_key(&(1, 7)));
    }

    #[tokio::test]
    async fn stream_process_output_forwards_both_streams_then_the_real_exit_code_arrives_via_wait() {
        let mut processes: ProcessRegistry = HashMap::new();
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
        // its real caller, main()'s process_done arm, gets it via reap_exited_process.
        assert_eq!(reap_exited_process(&mut processes, 1, 9).await, Some(Some(3)));
    }

    #[tokio::test]
    async fn kill_registered_process_reaps_and_reports_a_signal_death() {
        let mut processes: ProcessRegistry = HashMap::new();
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
        let mut processes: ProcessRegistry = HashMap::new();
        assert!(matches!(kill_registered_process(&mut processes, 1, 99).await, KillOutcome::NotRegistered));
    }

    #[tokio::test]
    async fn reap_exited_process_on_an_unregistered_id_returns_none() {
        let mut processes: ProcessRegistry = HashMap::new();
        assert_eq!(reap_exited_process(&mut processes, 1, 1).await, None);
    }

    #[tokio::test]
    async fn reap_generations_processes_reaps_only_the_matching_generations_entries() {
        let mut processes: ProcessRegistry = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 5"));
        spawn_and_register_process(&mut processes, 2, 1, "sh", &sh_args("sleep 5"));

        reap_generations_processes(&mut processes, 1).await;

        assert!(!processes.contains_key(&(1, 1)), "generation 1's process must be reaped and removed");
        assert!(processes.contains_key(&(2, 1)), "generation 2's process must be untouched");

        kill_registered_process(&mut processes, 2, 1).await;
    }

    #[tokio::test]
    async fn reap_generations_processes_actually_kills_the_process_not_just_the_registry_entry() {
        let mut processes: ProcessRegistry = HashMap::new();
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
}
