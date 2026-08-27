//! `LiveProcesses` bookkeeping for the Lua `process.run` binding (ADR-0026): registering a
//! freshly-spawned child, streaming its stdout/stderr back as `ProcessOutput` frames,
//! kill/exit reporting, and per-generation/shutdown-time reap sweeps. Extracted out of
//! `main.rs` -- conceptually part of `process/`'s process-lifecycle primitives, not
//! `main.rs`'s composition-root job.

use std::collections::HashMap;
use std::io;

use shared::{ProcessExited, ProcessOutputLine, ProcessStream, SupervisorFrame};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};

use crate::send_frame_logged;
use crate::socket;

/// Every `process.run`-spawned child still tracked, keyed by the generation that spawned it and
/// the `CommandEnvelope.id` the Renderer assigned it (docs/adr/0026). A plain local, mutated only
/// from inside `main()`'s own `select!` arms -- not behind a mutex itself, matching every other
/// piece of cross-task *spawn-tracking* state in this file (ADR-0018's promised upgrade path).
/// (`socket::GenerationRegistry`'s own `connections` map is a pre-existing `Arc<Mutex<...>>`,
/// unrelated to this registry -- that one's shared across the listener's accept loop and every
/// connection task, a different problem than this one solves.)
pub(crate) type LiveProcesses = HashMap<(u32, u64), Child>;

/// Parses `process.run`'s `CommandEnvelope.params.arguments` -- `[cmd, args]`, `cmd` a string and
/// `args` an array of strings, the shape `renderer/src/lua/process.rs`'s `ProcessRegistry::run`
/// sends. `None` on any shape mismatch (a protocol desync, not a spawn failure -- logged by the
/// caller, no `ProcessExited` sent back since there's no `id` this parse can even attribute one
/// to reliably beyond what the envelope itself already carries).
pub(crate) fn process_run_args(arguments: &[serde_json::Value]) -> Option<(String, Vec<String>)> {
    let cmd = arguments.first()?.as_str()?.to_string();
    let args = arguments.get(1)?.as_array()?.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    Some((cmd, args))
}

/// The `process` capability's action dispatch (ADR-0037): owns the action match, argument
/// parse, and spawn for `process.run`/`process.kill` -- `main.rs` routes the whole capability
/// here with one arm. `async` (unlike the other capabilities' dispatchers) because `kill`'s reap
/// must complete before its `ProcessExited` report goes out.
pub(crate) async fn dispatch(
    processes: &mut LiveProcesses,
    registry: &socket::GenerationRegistry,
    process_done_tx: &tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    envelope: &shared::CommandEnvelope,
) {
    let generation_id = envelope.params.generation_id;
    let id = envelope.id;
    match envelope.params.action.as_str() {
        "run" => match process_run_args(&envelope.params.arguments) {
            Some((cmd, args)) => match spawn_and_register_process(processes, generation_id, id, &cmd, &args) {
                Some((stdout, stderr)) => {
                    let task_registry = registry.clone();
                    let task_done_tx = process_done_tx.clone();
                    tokio::spawn(async move {
                        stream_process_output(&task_registry, generation_id, id, stdout, stderr).await;
                        let _ = task_done_tx.send((generation_id, id));
                    });
                }
                None => {
                    send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }));
                }
            },
            None => {
                eprintln!("malformed process.run command from generation {generation_id}: {:?}", envelope.params.arguments);
                // Lua's ProcessHandle is already waiting on `id`'s exit_cb -- with no
                // process ever spawned, nothing else will ever report this id done, so
                // this is what stops it leaking `pending`'s callback pair forever on
                // the Renderer side (Correctness review, docs/adr/0026 addendum).
                send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }));
            }
        },
        "kill" => match kill_registered_process(processes, generation_id, id).await {
            KillOutcome::Reaped(code) => {
                send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
            }
            KillOutcome::ReapFailed => {
                // The registry entry is already removed by this point (see
                // kill_registered_process) and the OS-level reap failure is already
                // logged -- no future event will ever report this id done, so this is
                // what stops it leaking `pending`'s callback pair forever on the
                // Renderer side (Correctness review, docs/adr/0026 addendum). The real
                // exit code is unknowable here; `None` is honest, not synthesized.
                send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }));
            }
            KillOutcome::NotRegistered => {}
        },
        _ => crate::log_unknown_action(&envelope.params),
    }
}

/// `("process", "run")`'s spawn step: pipes stdout/stderr (`super::spawn_group_leader_piped`),
/// takes the piped handles off the `Child` before registering it, so `processes` can keep owning
/// the `Child` (for `kill`/supersede-reap) while a separate task reads its output. Logs and
/// returns `None` on spawn failure -- the caller still owes Lua a `ProcessExited` with an absent
/// code (§ 12).
pub(crate) fn spawn_and_register_process(
    processes: &mut LiveProcesses,
    generation_id: u32,
    id: u64,
    cmd: &str,
    args: &[String],
) -> Option<(ChildStdout, ChildStderr)> {
    match super::spawn_group_leader_piped(cmd, args, &[]) {
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
pub(crate) async fn stream_process_output(
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
pub(crate) enum KillOutcome {
    /// Nothing was registered under this id -- already exited and reaped via the completion
    /// channel, or an id Lua never actually got a handle for.
    NotRegistered,
    /// The process group was reaped; report this exit code back to Lua.
    Reaped(Option<i32>),
    /// `reap_process_group` itself failed (already logged).
    ReapFailed,
}

/// `("process", "kill")`'s handler: removes `(generation_id, id)` and reaps its process group via
/// the already-built `super::reap_process_group` -- ADR-0018's promised real caller for it.
/// `reap_process_group`'s returned `ExitStatus` already carries the real code (`None` here in the
/// ordinary case, since `SIGTERM`/`SIGKILL` are signal deaths), reused directly so a killed
/// process's `exit_cb` still fires with an honest code instead of a synthesized one.
pub(crate) async fn kill_registered_process(processes: &mut LiveProcesses, generation_id: u32, id: u64) -> KillOutcome {
    let Some(mut child) = processes.remove(&(generation_id, id)) else {
        return KillOutcome::NotRegistered;
    };
    match super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await {
        Ok(super::ReapOutcome::ExitedCleanly(status) | super::ReapOutcome::Escalated(status)) => KillOutcome::Reaped(status.code()),
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
pub(crate) fn take_exited_process(processes: &mut LiveProcesses, generation_id: u32, id: u64) -> Option<Child> {
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
pub(crate) async fn wait_and_report_exit(registry: socket::GenerationRegistry, generation_id: u32, id: u64, mut child: Child) {
    match child.wait().await {
        Ok(status) => send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code: status.code() })),
        Err(err) => eprintln!("failed to wait on exited process {id} (generation {generation_id}): {err}"),
    }
}

/// § 12's SIGTERM-then-SIGKILL group reap, applied to every process the superseded generation's
/// Lua spawned -- not just its own Renderer process (`CONTEXT.md`'s Generation swap). No
/// `ProcessExited` is sent for these: the superseded generation's own connection is being torn
/// down in the same swap, so there's no live Lua VM left to receive it.
pub(crate) async fn reap_generations_processes(processes: &mut LiveProcesses, generation_id: u32) {
    let stale_ids: Vec<(u32, u64)> = processes.keys().filter(|(entry_generation_id, _)| *entry_generation_id == generation_id).copied().collect();
    for key in stale_ids {
        if let Some(mut child) = processes.remove(&key)
            && let Err(err) = super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await
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
pub(crate) async fn reap_all_processes(processes: &mut LiveProcesses) {
    let ids: Vec<(u32, u64)> = processes.keys().copied().collect();
    for key in ids {
        if let Some(mut child) = processes.remove(&key)
            && let Err(err) = super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await
        {
            eprintln!("failed to reap process {key:?} on shutdown: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;

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
