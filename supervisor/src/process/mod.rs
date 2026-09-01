//! Subprocess process-group lifecycle primitives: spawn a child as leader of its own new
//! process group, then reap the whole group (`SIGTERM`, grace period, escalate to `SIGKILL`).
//! See ADR-0018, ADR-0026 (`process.run`, on top in [`registry`]), ADR-0025
//! (`reload::run_pba` is the reap primitive's caller).
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

/// What a `wait()` `timeout()`'s elapsed deadline means once a following `try_wait()` is consulted.
#[derive(Debug, PartialEq, Eq)]
enum TimeoutRace {
    /// The group had, in fact, already exited in the same instant the deadline fired.
    ActuallyExited(ExitStatus),
    /// Genuinely still running -- the timeout wasn't racing a real exit.
    StillRunning,
}

/// Classifies a `wait()` `timeout()`'s elapsed deadline: a real exit can land in the same
/// instant the timer fires. `late_status` must come from a non-blocking `try_wait()` right after.
fn classify_timeout(late_status: io::Result<Option<ExitStatus>>) -> io::Result<TimeoutRace> {
    Ok(match late_status? {
        Some(status) => TimeoutRace::ActuallyExited(status),
        None => TimeoutRace::StillRunning,
    })
}

/// Waits up to `grace` for `child` to exit, applying [`classify_timeout`] if the deadline
/// fires first. Shared by both post-SIGTERM and post-SIGKILL phases in [`reap_process_group`].
async fn wait_or_classify(child: &mut Child, grace: Duration) -> io::Result<TimeoutRace> {
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => Ok(TimeoutRace::ActuallyExited(status?)),
        Err(_elapsed) => classify_timeout(child.try_wait()),
    }
}

/// Sends `signal` to `pgid`, treating "already gone" (`ESRCH`) as success -- a group dying
/// between deciding to signal and `kill(2)` landing is a race to reap around, not a failure.
fn signal_group_best_effort(pgid: Pid, signal: Signal) -> io::Result<()> {
    match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// The 100ms grace window `docs/oblisk-supervisor-services-dbus.md` § 12 specifies between
/// `SIGTERM` and `SIGKILL`. A parameter, not baked in, so tests can use a faster reap.
pub const DEFAULT_REAP_GRACE: Duration = Duration::from_millis(100);

/// Spawns `cmd` as the leader of a new, independent Unix process group. Any process this
/// child forks without calling `setsid`/`setpgid` itself inherits that same group, which is
/// what lets [`reap_process_group`] clean up a whole subtree via a single `killpg`.
pub fn spawn_group_leader(cmd: &str, args: &[String], envs: &[(String, String)]) -> io::Result<Child> {
    Command::new(cmd).args(args).envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str()))).process_group(0).spawn()
}

/// Identical to [`spawn_group_leader`] except stdout/stderr are piped instead of inherited --
/// `process.run` (ADR-0026) needs to forward the child's output as
/// `SupervisorFrame::ProcessOutput`. Stdin stays inherited.
pub fn spawn_group_leader_piped(cmd: &str, args: &[String], envs: &[(String, String)]) -> io::Result<Child> {
    Command::new(cmd)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Identical to [`spawn_group_leader`] except stdin and stdout are piped instead of
/// inherited. `dbus::polkit`'s PAM worker (ADR-0028) writes the password to stdin once, then
/// reads one `shared::PamOutcome` frame back over stdout. Stderr stays inherited.
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
    /// The group exited on its own within the grace period; `SIGKILL` was never sent.
    ExitedCleanly(ExitStatus),
    /// The group ignored (or was too slow to react to) `SIGTERM`; `SIGKILL` was sent to
    /// force it down.
    Escalated(ExitStatus),
}

/// Safely reaps `child`'s process group: `SIGTERM` to the whole group, wait up to `grace`,
/// escalate to `SIGKILL` if it hasn't exited. Signals the *group*, not just `child`'s pid, so
/// it also reaches any descendant the child forked into the same group.
///
/// `child` must have been spawned via [`spawn_group_leader`] -- this reads `child.id()` as
/// the pgid, which only holds for a group leader.
///
/// Never blocks longer than roughly `2 * grace`: a process wedged in uninterruptible I/O (D
/// state) can defer even `SIGKILL` indefinitely, so the post-`SIGKILL` wait is bounded too.
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

    // Reproducing the actual OS-level race (a real SIGKILL landing exactly on a timeout
    // deadline) isn't reliable to trigger -- 320 stress runs never hit it. These two tests
    // instead cover the classification decision directly with a fabricated `ExitStatus`.

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

    /// Polls `condition` until it's true or `timeout` elapses, sleeping briefly between checks --
    /// used below to wait out a grandchild's `/proc` teardown after its parent is reaped.
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
        // Give the shell a moment to install `trap '' TERM` -- a SIGTERM racing the shell's
        // own startup can otherwise land before the trap installs, flaking this test.
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
        // Reap it directly (bypassing reap_process_group) so the group has zero members left --
        // killpg against a pgid nothing belongs to anymore returns ESRCH, the exact race this
        // helper exists to swallow.
        child.wait().await.expect("wait should reap the already-exited child");

        signal_group_best_effort(pgid, Signal::SIGTERM)
            .expect("signaling an already-gone process group must not surface ESRCH as an error");
    }

    #[tokio::test]
    async fn reap_process_group_kills_the_whole_group_not_just_the_direct_child() {
        // The direct child backgrounds a grandchild without calling setsid/setpgid, so it
        // inherits the child's group. It prints the grandchild's pid via `$!` then blocks in a
        // bare `wait`, keeping the shell alive as a real second process sharing that group.
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
        // Guards against the piped variant silently dropping the process-group behavior.
        let mut child = spawn_group_leader_piped("sh", &sh_args("sleep 5"), &[]).expect("failed to spawn");
        let child_pid = child.id().expect("freshly spawned child has a pid");

        let child_pgid = nix::unistd::getpgid(Some(Pid::from_raw(child_pid as i32))).expect("getpgid on the child");
        let our_pgid = nix::unistd::getpgrp();
        assert_ne!(child_pgid, our_pgid, "child should not inherit the test process's own group");

        reap_process_group(&mut child, Duration::from_millis(200)).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn spawn_group_leader_piped_pipes_stdout_and_stderr_separately_with_the_real_exit_code() {
        let mut child = spawn_group_leader_piped("sh", &sh_args("echo line1; echo line2 >&2; exit 3"), &[])
            .expect("failed to spawn");

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
        // Guards against the stdin/stdout-piped variant silently dropping process-group behavior.
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
}
