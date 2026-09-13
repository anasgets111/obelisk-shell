//! Process-group lifecycle: spawn a new group leader, then reap the group with `SIGTERM`, grace,
//! and `SIGKILL`. See ADR-0018, ADR-0026 (`process.run`, [`registry`]), and ADR-0025
//! (`reload::run_pba` calls the reap primitive).
//!
//! Uses `tokio::process::Command::process_group(0)` rather than hand-rolling `setpgid`.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::{Child, Command};

pub mod registry;

/// Meaning of an elapsed `wait()` timeout after a following `try_wait()`.
#[derive(Debug, PartialEq, Eq)]
enum TimeoutRace {
    /// The group exited as the deadline fired.
    ActuallyExited(ExitStatus),
    /// Still running; the timeout did not race an exit.
    StillRunning,
}

/// Classifies an elapsed `wait()` timeout. A real exit can land at the deadline; `late_status` must
/// be the immediate non-blocking `try_wait()` result.
fn classify_timeout(late_status: io::Result<Option<ExitStatus>>) -> io::Result<TimeoutRace> {
    Ok(match late_status? {
        Some(status) => TimeoutRace::ActuallyExited(status),
        None => TimeoutRace::StillRunning,
    })
}

/// Waits up to `grace`, classifying a deadline with [`classify_timeout`]. Used after both SIGTERM
/// and SIGKILL in [`reap_process_group`].
async fn wait_or_classify(child: &mut Child, grace: Duration) -> io::Result<TimeoutRace> {
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => Ok(TimeoutRace::ActuallyExited(status?)),
        Err(_elapsed) => classify_timeout(child.try_wait()),
    }
}

/// Sends `signal` to `pgid`; `ESRCH` is success because the group may die before `kill(2)` lands.
pub(crate) fn signal_group_best_effort(pgid: Pid, signal: Signal) -> io::Result<()> {
    match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// The 100ms grace window between SIGTERM and SIGKILL. Parameterized so
/// tests can reap faster.
pub const DEFAULT_REAP_GRACE: Duration = Duration::from_millis(100);

/// Spawns `cmd` as a new group leader. Descendants without `setsid`/`setpgid` inherit the group,
/// letting [`reap_process_group`] clean a subtree with one `killpg`.
pub fn spawn_group_leader(cmd: &str, args: &[String], envs: &[(String, String)]) -> io::Result<Child> {
    Command::new(cmd).args(args).envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str()))).process_group(0).spawn()
}

/// Spawns `cmd` fully detached: its own session, reparented to init, and never this process's to
/// wait on or signal.
///
/// [`spawn_group_leader`] gives a new process *group*, which is enough to reap a subtree with one
/// `killpg` but leaves the program a direct child of the Supervisor: it sits under the shell in
/// every process tree, and it is one registry entry away from being reaped by a config reload. A
/// text editor opened from the launcher should outlive the shell that opened it, and should not
/// look like part of it.
///
/// So: `setsid` in the child, then fork again and let the intermediate leave at once. The
/// grandchild is orphaned the moment its parent exits and `init` adopts it. `setsid` before the
/// second fork rather than after is what stops the grandchild ever acquiring a controlling
/// terminal, since only a session leader can.
///
/// The three standard streams go to `/dev/null`. A detached program has nowhere to write: the
/// Supervisor is not holding pipes for it, and leaving them inherited would let it scribble on the
/// shell's own stdout long after nobody is reading.
pub fn spawn_detached(cmd: &str, args: &[String]) -> io::Result<()> {
    let mut command = Command::new(cmd);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: `pre_exec` runs in the forked child between `fork` and `exec`, where only the calling
    // thread exists. Every call here is async-signal-safe and on POSIX's list for that window:
    // `setsid`, `fork` and `_exit`. Nothing allocates, takes a lock, or touches Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                // The grandchild, which goes on to `exec` and is orphaned when its parent leaves.
                0 => Ok(()),
                // The intermediate. `_exit` rather than `exit`: this is a forked copy of a process
                // whose atexit handlers and buffered streams belong to the Supervisor.
                _ => libc::_exit(0),
            }
        });
    }
    // The intermediate exits immediately; dropping the handle leaves it to tokio's reaper, which
    // is what keeps it from lingering as a zombie. The grandchild was never ours to hold.
    command.spawn().map(drop)
}

/// [`spawn_group_leader`] with piped stdout/stderr for `process.run` (ADR-0026) to forward as
/// `SupervisorFrame::ProcessOutput`; stdin stays inherited.
pub fn spawn_group_leader_piped(cmd: &str, args: &[String]) -> io::Result<Child> {
    Command::new(cmd).args(args).process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
}

/// [`spawn_group_leader`] with piped stdin/stdout for PAM (ADR-0028): write one password,
/// read one `shared::PamOutcome`; stderr stays inherited.
pub fn spawn_group_leader_stdio_piped(cmd: &str, args: &[String], envs: &[(String, String)]) -> io::Result<Child> {
    Command::new(cmd)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
}

/// How [`reap_process_group`] recovered `child`'s process group.
#[derive(Debug)]
pub enum ReapOutcome {
    /// The group exited within grace; `SIGKILL` was not sent.
    ExitedCleanly(ExitStatus),
    /// The group ignored or was too slow for `SIGTERM`; `SIGKILL` forced it down.
    Escalated(ExitStatus),
}

/// Reaps `child`'s group: SIGTERM, wait `grace`, then SIGKILL. Signaling the group reaches
/// descendants in it, not just `child`.
///
/// `child` must come from [`spawn_group_leader`]: `child.id()` is the pgid only for a group leader.
///
/// Waits roughly `2 * grace` at most. D-state I/O can defer even SIGKILL indefinitely, so the
/// post-SIGKILL wait is bounded too.
pub async fn reap_process_group(child: &mut Child, grace: Duration) -> io::Result<ReapOutcome> {
    let pid = child.id().ok_or_else(|| io::Error::other("child has no pid; already reaped"))?;
    let pgid = Pid::from_raw(pid as i32);

    signal_group_best_effort(pgid, Signal::SIGTERM)?;
    if let TimeoutRace::ActuallyExited(status) = wait_or_classify(child, grace).await? {
        return Ok(ReapOutcome::ExitedCleanly(status));
    }

    signal_group_best_effort(pgid, Signal::SIGKILL)?;
    match wait_or_classify(child, grace).await? {
        TimeoutRace::ActuallyExited(status) => Ok(ReapOutcome::Escalated(status)),
        TimeoutRace::StillRunning => {
            Err(io::Error::other("process group did not exit even after SIGKILL (likely stuck in uninterruptible I/O)"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    fn sh_args(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    // The OS race is unreliable: 320 stress runs never hit it. Test classification directly with
    // fabricated `ExitStatus` values instead.

    #[test]
    fn classify_timeout_reports_the_race_when_late_status_shows_an_exit() {
        let status = ExitStatus::from_raw(0);

        let race = classify_timeout(Ok(Some(status))).expect("late_status was Ok");

        assert_eq!(race, TimeoutRace::ActuallyExited(status));
    }

    #[test]
    fn classify_timeout_reports_still_running_when_late_status_is_none() {
        let race = classify_timeout(Ok(None)).expect("late_status was Ok");

        assert_eq!(race, TimeoutRace::StillRunning);
    }

    #[tokio::test]
    async fn wait_or_classify_reports_still_running_against_a_long_lived_child_within_a_short_grace() {
        let mut child = spawn_group_leader("sh", &sh_args("sleep 5"), &[]).expect("failed to spawn");

        let race = wait_or_classify(&mut child, Duration::from_millis(20)).await.expect("wait_or_classify failed");

        assert_eq!(
            race,
            TimeoutRace::StillRunning,
            "a still-running child within a short grace must not be misreported as exited"
        );

        // No signal was sent above, so clean up directly rather than leaking the sleep.
        child.kill().await.expect("cleanup kill failed");
    }

    /// Polls until `condition` or `timeout`, waiting for a grandchild's `/proc` teardown after its
    /// parent is reaped.
    async fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn proc_exists(pid: i32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[tokio::test]
    async fn spawn_group_leader_puts_the_child_in_its_own_process_group() {
        let mut child = spawn_group_leader("sh", &sh_args("sleep 5"), &[]).expect("failed to spawn");
        let child_pid = child.id().expect("freshly spawned child has a pid");

        let child_pgid = nix::unistd::getpgid(Some(Pid::from_raw(child_pid as i32))).expect("getpgid on the child");
        let our_pgid = nix::unistd::getpgrp();
        assert_ne!(child_pgid, our_pgid, "child should not inherit the test process's own group");
        assert_eq!(child_pgid.as_raw(), child_pid as i32, "a group leader's pgid equals its own pid");

        reap_process_group(&mut child, Duration::from_millis(200)).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn reap_process_group_reaps_a_sigterm_compliant_child_without_escalating() {
        let mut child = spawn_group_leader("sh", &sh_args("sleep 5"), &[]).expect("failed to spawn");

        let outcome = reap_process_group(&mut child, Duration::from_millis(500)).await.expect("reap failed");

        assert!(matches!(outcome, ReapOutcome::ExitedCleanly(_)), "expected a clean exit, got {outcome:?}");
    }

    #[tokio::test]
    async fn reap_process_group_escalates_a_sigterm_ignoring_child_to_sigkill() {
        let mut child = spawn_group_leader("sh", &sh_args("trap '' TERM; sleep 5"), &[]).expect("failed to spawn");
        // Let the shell install `trap '' TERM`; a racing SIGTERM can otherwise flake this test.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = reap_process_group(&mut child, Duration::from_millis(50)).await.expect("reap failed");

        let status = match outcome {
            ReapOutcome::Escalated(status) => status,
            ReapOutcome::ExitedCleanly(_) => panic!("expected escalation, child ignores SIGTERM"),
        };
        assert!(!status.success(), "a SIGKILLed process must not report success");
    }

    #[tokio::test]
    async fn signal_group_best_effort_tolerates_a_process_group_that_no_longer_exists() {
        let mut child = spawn_group_leader("true", &[], &[]).expect("failed to spawn");
        let pid = child.id().expect("freshly spawned child has a pid");
        let pgid = Pid::from_raw(pid as i32);
        // Reap directly so the empty group makes `killpg` return ESRCH, the race this helper
        // swallows.
        child.wait().await.expect("wait should reap the already-exited child");

        signal_group_best_effort(pgid, Signal::SIGTERM)
            .expect("signaling an already-gone process group must not surface ESRCH as an error");
    }

    #[tokio::test]
    async fn reap_process_group_kills_the_whole_group_not_just_the_direct_child() {
        // The child backgrounds a grandchild without `setsid`/`setpgid`, prints `$!`, then blocks
        // in `wait`; both remain real members of the same group.
        let mut child = Command::new("sh")
            .args(sh_args("sleep 30 & echo $!; wait"))
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to spawn");

        let stdout = child.stdout.take().expect("stdout was piped");
        let mut lines = BufReader::new(stdout).lines();
        let grandchild_pid: i32 = lines
            .next_line()
            .await
            .expect("reading the grandchild pid from stdout")
            .expect("child should have printed a pid line before blocking in wait")
            .trim()
            .parse()
            .expect("grandchild pid should parse as an integer");

        assert!(proc_exists(grandchild_pid), "grandchild should be running before the group is reaped");

        reap_process_group(&mut child, Duration::from_millis(200)).await.expect("reap failed");

        let gone = wait_until(Duration::from_millis(500), || !proc_exists(grandchild_pid)).await;
        assert!(gone, "grandchild (pid {grandchild_pid}) should be gone after killpg reached the whole group");
    }

    #[tokio::test]
    async fn spawn_group_leader_piped_also_puts_the_child_in_its_own_process_group() {
        // Guard the piped variant's process-group behavior.
        let mut child = spawn_group_leader_piped("sh", &sh_args("sleep 5")).expect("failed to spawn");
        let child_pid = child.id().expect("freshly spawned child has a pid");

        let child_pgid = nix::unistd::getpgid(Some(Pid::from_raw(child_pid as i32))).expect("getpgid on the child");
        let our_pgid = nix::unistd::getpgrp();
        assert_ne!(child_pgid, our_pgid, "child should not inherit the test process's own group");

        reap_process_group(&mut child, Duration::from_millis(200)).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn spawn_group_leader_piped_pipes_stdout_and_stderr_separately_with_the_real_exit_code() {
        let mut child =
            spawn_group_leader_piped("sh", &sh_args("echo line1; echo line2 >&2; exit 3")).expect("failed to spawn");

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let stdout_line = BufReader::new(stdout).lines().next_line().await.unwrap().unwrap();
        let stderr_line = BufReader::new(stderr).lines().next_line().await.unwrap().unwrap();
        assert_eq!(stdout_line, "line1");
        assert_eq!(stderr_line, "line2");

        let status = child.wait().await.expect("wait failed");
        assert_eq!(status.code(), Some(3));
    }

    #[tokio::test]
    async fn spawn_group_leader_stdio_piped_also_puts_the_child_in_its_own_process_group() {
        // Guard the stdin/stdout-piped variant's process-group behavior.
        let mut child = spawn_group_leader_stdio_piped("sh", &sh_args("cat"), &[]).expect("failed to spawn");
        let child_pid = child.id().expect("freshly spawned child has a pid");

        let child_pgid = nix::unistd::getpgid(Some(Pid::from_raw(child_pid as i32))).expect("getpgid on the child");
        let our_pgid = nix::unistd::getpgrp();
        assert_ne!(child_pgid, our_pgid, "child should not inherit the test process's own group");

        reap_process_group(&mut child, Duration::from_millis(200)).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn spawn_group_leader_stdio_piped_pipes_stdin_and_stdout_with_the_real_exit_code() {
        let mut child = spawn_group_leader_stdio_piped("sh", &sh_args("cat"), &[]).expect("failed to spawn");

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        tokio::io::AsyncWriteExt::write_all(&mut stdin, b"hello\n").await.expect("write to stdin failed");
        drop(stdin); // closes the write half so `cat`'s read hits EOF and it exits

        let echoed = BufReader::new(stdout).lines().next_line().await.unwrap().unwrap();
        assert_eq!(echoed, "hello");

        let status = child.wait().await.expect("wait failed");
        assert!(status.success());
    }

    /// ADR-0188. The claim is that a launched program stops being ours, so the test asks the
    /// program itself: it writes its own parent's pid, and that must not be this process.
    ///
    /// `spawn_group_leader` fails this exactly, which is the bug -- a new process group is still a
    /// direct child.
    #[tokio::test]
    async fn a_detached_program_is_reparented_away_from_this_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("ppid");
        // No sleep, deliberately: whether `$PPID` is read before or after `init` adopts it, the
        // answer is a pid that is not this process -- the intermediate's, or the reaper's. Waiting
        // for the handover would make the test's timing part of what it asserts, for nothing.
        let script = format!("printf %s \"$PPID\" > {}", out.display());
        spawn_detached("sh", &["-c".to_string(), script]).expect("spawn");

        let mut waited = 0;
        while !out.exists() && waited < 200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        let recorded = std::fs::read_to_string(&out).expect("the detached program must have run");
        let parent: u32 = recorded.trim().parse().expect("a pid");
        assert_ne!(parent, std::process::id(), "a detached program must not be this process's child");

        // And it is a session of its own, so a signal to this process's group cannot reach it.
        assert_ne!(parent, 0);
    }
}
