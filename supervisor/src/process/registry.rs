//! `process.run` bookkeeping (ADR-0026): child registration, stdout/stderr `ProcessOutput` frames,
//! kill/exit reporting, and per-generation/shutdown reap sweeps.

use std::collections::HashMap;
use std::io;

use shared::{ProcessExited, ProcessOutputLine, ProcessStream, SupervisorFrame};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};

use crate::send_frame_logged;
use crate::socket;

/// Tracked `process.run` children, keyed by spawning generation and Renderer-assigned
/// `CommandEnvelope.id` (ADR-0026). Mutated only inside `main()`'s `select!`, not behind a mutex.
pub(crate) type LiveProcesses = HashMap<(u32, u64), Child>;

/// Parses `process.run` arguments as `[cmd, args]`. `None` means protocol shape mismatch, not
/// spawn failure; the caller logs it.
pub(crate) fn process_run_args(arguments: &[serde_json::Value]) -> Option<(String, Vec<String>)> {
    let cmd = arguments.first()?.as_str()?.to_string();
    let args =
        arguments.get(1)?.as_array()?.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    Some((cmd, args))
}

/// Dispatches `process.run`/`process.kill` (ADR-0037), including parse and spawn. `async` because
/// kill must reap before sending `ProcessExited`.
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
                    send_frame_logged(
                        registry,
                        generation_id,
                        &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }),
                    );
                }
            },
            None => {
                eprintln!(
                    "malformed process.run command from generation {generation_id}: {:?}",
                    envelope.params.arguments
                );
                // Lua's ProcessHandle already awaits `id`'s exit_cb; this prevents a callback pair
                // leak when no process spawned.
                send_frame_logged(
                    registry,
                    generation_id,
                    &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }),
                );
            }
        },
        "kill" => match kill_registered_process(processes, generation_id, id).await {
            KillOutcome::Reaped(code) => {
                send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
            }
            KillOutcome::ReapFailed => {
                // Entry is removed and failure logged; send `None` to stop `id`'s exit_cb leaking.
                // `None` is honest, not synthesized.
                send_frame_logged(
                    registry,
                    generation_id,
                    &SupervisorFrame::ProcessExited(ProcessExited { id, code: None }),
                );
            }
            KillOutcome::NotRegistered => {}
        },
        _ => crate::log_unknown_action(&envelope.params),
    }
}

/// Spawns `("process", "run")` with piped stdout/stderr, removes handles before registering the
/// `Child`, leaving `processes` owning it while a task reads output. Spawn failure is `None`.
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

/// Reads stdout/stderr concurrently by line, forwarding `SupervisorFrame::ProcessOutput` through
/// `registry`. After both EOFs, reports `(generation_id, id)` on `process_done_tx`; it does not own
/// the `Child`, so it cannot `wait()` for the exit code.
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

/// Handles one stream poll: sends a frame for a line, logs read errors, and reports EOF/error done.
fn report_process_output_line(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    stream: ProcessStream,
    line: io::Result<Option<String>>,
) -> bool {
    match line {
        Ok(Some(line)) => {
            send_frame_logged(
                registry,
                generation_id,
                &SupervisorFrame::ProcessOutput(ProcessOutputLine { id, stream, line }),
            );
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
    /// No entry: already reaped via completion, or Lua never received a handle.
    NotRegistered,
    /// Group reaped; report its exit code to Lua.
    Reaped(Option<i32>),
    /// `reap_process_group` failed; already logged.
    ReapFailed,
}

/// Removes `(generation_id, id)` and reaps its group via `super::reap_process_group` (ADR-0018).
/// Reuse the returned code, usually `None` for SIGTERM/SIGKILL deaths, so `exit_cb` is honest.
pub(crate) async fn kill_registered_process(processes: &mut LiveProcesses, generation_id: u32, id: u64) -> KillOutcome {
    let Some(mut child) = processes.remove(&(generation_id, id)) else {
        return KillOutcome::NotRegistered;
    };
    match super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await {
        Ok(super::ReapOutcome::ExitedCleanly(status) | super::ReapOutcome::Escalated(status)) => {
            KillOutcome::Reaped(status.code())
        }
        Err(err) => {
            eprintln!("failed to reap process {id} (generation {generation_id}) on kill: {err}");
            KillOutcome::ReapFailed
        }
    }
}

/// Fast `process_done` half: after both streams close, remove the entry without awaiting. Safe in
/// `main()`'s `select!`; [`wait_and_report_exit`] performs the real `wait()`.
pub(crate) fn take_exited_process(processes: &mut LiveProcesses, generation_id: u32, id: u64) -> Option<Child> {
    processes.remove(&(generation_id, id))
}

/// Slow `process_done` half: waits for the real exit and reports it to Lua. Detach it, never await
/// inline in `main()`'s `select!`: streams can close while a daemonizing child keeps running after
/// redirecting them to `/dev/null`.
pub(crate) async fn wait_and_report_exit(
    registry: socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    mut child: Child,
) {
    match child.wait().await {
        Ok(status) => send_frame_logged(
            &registry,
            generation_id,
            &SupervisorFrame::ProcessExited(ProcessExited { id, code: status.code() }),
        ),
        Err(err) => eprintln!("failed to wait on exited process {id} (generation {generation_id}): {err}"),
    }
}

/// Applies Supervisor § 10's SIGTERM/SIGKILL group reap to every process spawned by the superseded
/// generation's Lua, not only its Renderer (`CONTEXT.md` Generation swap). Send no `ProcessExited`;
/// that generation's connection is torn down in the same swap.
pub(crate) async fn reap_generations_processes(processes: &mut LiveProcesses, generation_id: u32) {
    let stale_ids: Vec<(u32, u64)> =
        processes.keys().filter(|(entry_generation_id, _)| *entry_generation_id == generation_id).copied().collect();
    for key in stale_ids {
        if let Some(mut child) = processes.remove(&key)
            && let Err(err) = super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await
        {
            eprintln!("failed to reap process {key:?} belonging to superseded generation {generation_id}: {err}");
        }
    }
}

/// Reaps every tracked child at shutdown, the counterpart to the per-generation sweep. Without
/// it, SIGINT/SIGTERM orphaned live children; a boot Renderer was confirmed to keep running
/// headless in its own group.
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
        assert_eq!(
            process_run_args(&arguments),
            Some(("echo".to_string(), vec!["hello".to_string(), "world".to_string()]))
        );
    }

    #[test]
    fn process_run_args_rejects_a_malformed_shape() {
        assert_eq!(process_run_args(&[]), None, "missing both elements");
        assert_eq!(process_run_args(&[serde_json::json!(1), serde_json::json!([])]), None, "cmd is not a string");
        assert_eq!(
            process_run_args(&[serde_json::json!("echo"), serde_json::json!("not-an-array")]),
            None,
            "args is not an array"
        );
        assert_eq!(
            process_run_args(&[serde_json::json!("echo"), serde_json::json!([1, 2])]),
            None,
            "args contains non-strings"
        );
    }

    fn registry_with_connection(
        generation_id: u32,
    ) -> (socket::GenerationRegistry, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
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
            spawn_and_register_process(&mut processes, 1, 9, "sh", &sh_args("echo out1; echo err1 >&2; exit 3"))
                .unwrap();
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
        // that's take_exited_process + wait_and_report_exit's job, via a detached task.
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
        assert!(
            gone.is_ok(),
            "process {pid} should be gone after the supersede-time reap, not just removed from the registry"
        );
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
        let pids: Vec<u32> =
            processes.values().map(|child| child.id().expect("freshly spawned child has a pid")).collect();

        reap_all_processes(&mut processes).await;

        let gone = tokio::time::timeout(Duration::from_millis(500), async {
            while pids.iter().any(|pid| Path::new(&format!("/proc/{pid}")).exists()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "every reaped process should actually be gone from /proc, not just removed from the registry"
        );
    }
}
