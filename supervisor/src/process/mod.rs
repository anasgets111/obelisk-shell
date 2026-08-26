//! Subprocess process-group lifecycle primitives (build-steps.md Phase 7,
//! `docs/oblisk-supervisor-services-dbus.md` § 12's process-group gating half).
//!
//! Two primitives only: spawning a child as the leader of its own new process group, and
//! safely reaping that whole group (`SIGTERM`, grace period, escalate to `SIGKILL`). Neither
//! `process.run`'s Lua binding nor a process registry exist yet -- see
//! docs/adr/0018-process-group-primitives-without-process-run-or-a-registry.md for why this
//! phase stopped there. The Phase 8 Presentation-Before-Authority reload orchestrator
//! (`reload::run_pba`) got both primitives' first real callers in Phase 14 --
//! docs/adr/0025-pba-orchestrator-wired-with-atomic-per-candidate-promotion.md.
//!
//! build-steps.md's own Phase 7 snippet spawns via `unsafe { .pre_exec(|| nix::unistd::
//! setpgid(...)) }`. `tokio::process::Command::process_group(0)` (stable since tokio 1.21,
//! confirmed present in this crate's vendored 1.53.1) does exactly that -- "a process group
//! ID of 0 will use the process ID as the PGID" -- as a safe method, so [`spawn_group_leader`]
//! uses that instead of hand-rolling the same `setpgid` call through `unsafe` `pre_exec`.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::{Child, Command};

/// What a `wait()` `timeout()`'s elapsed deadline turns out to mean once a following,
/// non-blocking `try_wait()` is consulted.
#[derive(Debug, PartialEq, Eq)]
enum TimeoutRace {
    /// The group had, in fact, already exited in the same instant the deadline fired.
    ActuallyExited(ExitStatus),
    /// Genuinely still running -- the timeout wasn't racing a real exit.
    StillRunning,
}

/// Classifies a `wait()` `timeout()`'s elapsed deadline: the deadline firing only means our
/// poll hadn't observed an exit yet, not that the group is still alive -- SIGTERM/SIGKILL
/// delivery, scheduling, and the zombie-reap transition all take non-zero, non-deterministic
/// time, so a real exit can land in the same instant the timer fires. `late_status` should
/// come from a non-blocking `try_wait()` call made immediately after the timeout, so
/// classifying this never itself blocks. Shared by both the post-SIGTERM and post-SIGKILL
/// branches of [`reap_process_group`] so they can't drift out of sync the way they used to --
/// only the post-SIGTERM branch used to make this check, which is exactly what let a process
/// group actually killed by SIGKILL get misreported as stuck in uninterruptible I/O.
fn classify_timeout(late_status: io::Result<Option<ExitStatus>>) -> io::Result<TimeoutRace> {
    Ok(match late_status? {
        Some(status) => TimeoutRace::ActuallyExited(status),
        None => TimeoutRace::StillRunning,
    })
}

/// Waits up to `grace` for `child` to exit, applying [`classify_timeout`] if the deadline
/// fires first. The one place [`reap_process_group`]'s post-SIGTERM and post-SIGKILL phases
/// both go through -- sharing this instead of each phase inlining its own
/// `timeout(...).await` + fallback keeps them from drifting out of sync again, which is
/// exactly how the post-SIGKILL phase ended up missing the race check in the first place.
async fn wait_or_classify(child: &mut Child, grace: Duration) -> io::Result<TimeoutRace> {
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => Ok(TimeoutRace::ActuallyExited(status?)),
        Err(_elapsed) => classify_timeout(child.try_wait()),
    }
}

/// Sends `signal` to `pgid`, treating "the group is already gone" (`ESRCH` -- every member
/// already exited) as success rather than an error: a group dying in the narrow window
/// between us deciding to signal it and the `kill(2)` actually landing is a race to reap
/// around, not a real failure to report up.
fn signal_group_best_effort(pgid: Pid, signal: Signal) -> io::Result<()> {
    match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// The 100ms grace window `docs/oblisk-supervisor-services-dbus.md` § 12 specifies between
/// `SIGTERM` and `SIGKILL` escalation. [`reap_process_group`] takes its grace period as a
/// parameter rather than hardcoding this, so callers needing a faster reap (tests, an
/// impatient hot-reload) aren't stuck with it -- this is just the spec's own default.
/// `main.rs`'s real reap of a superseded generation after a PBA swap (Phase 14) is this
/// constant's first production caller.
pub const DEFAULT_REAP_GRACE: Duration = Duration::from_millis(100);

/// Spawns `cmd` as the leader of a new, independent Unix process group, rather than
/// inheriting this process's own group. Any process this child forks without calling
/// `setsid`/`setpgid` itself (e.g. a shell backgrounding a job) inherits that same group --
/// which is what lets [`reap_process_group`] clean up a whole subtree, not just the direct
/// child, via a single `killpg`. `envs` is set on top of this process's own inherited
/// environment (e.g. `OBLISK_GENERATION_ID`/`OBLISK_PBA_CANDIDATE` -- `reload::run_pba`'s real
/// caller, `main.rs`, is this parameter's first production use).
pub fn spawn_group_leader(cmd: &str, args: &[String], envs: &[(String, String)]) -> io::Result<Child> {
    Command::new(cmd).args(args).envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str()))).process_group(0).spawn()
}

/// Identical to [`spawn_group_leader`] except stdout/stderr are piped instead of inherited --
/// `process.run`'s real requirement (build-steps.md Phase 15 item 1, docs/adr/0026): the
/// Supervisor reads the child's output itself to forward it as `SupervisorFrame::ProcessOutput`
/// lines, rather than let it reach the Supervisor's own terminal like every `spawn_group_leader`
/// caller (the boot Renderer spawn, every PBA candidate spawn) still needs to. Stdin stays
/// inherited, matching `spawn_group_leader`; nothing in this phase's spec asks for piped stdin.
pub fn spawn_group_leader_piped(cmd: &str, args: &[String], envs: &[(String, String)]) -> io::Result<Child> {
    Command::new(cmd)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

/// Safely reaps `child`'s process group: `SIGTERM` to the whole group, wait up to `grace`
/// for `child` (the group leader) to exit, escalate to `SIGKILL` on the whole group if it
/// hasn't. Signaling the *group* rather than just `child`'s own pid is what reaches any
/// descendant the child forked into the same group without giving it its own -- see
/// [`spawn_group_leader`].
///
/// `child` must have been spawned via [`spawn_group_leader`] (or otherwise be its own group
/// leader) -- this reads `child.id()` as the pgid, which only holds for a group leader.
///
/// Never blocks longer than roughly `2 * grace`: `SIGKILL` can't be caught or ignored, but a
/// process wedged in uninterruptible I/O (D state) can still defer it indefinitely, so the
/// post-`SIGKILL` wait is bounded by `grace` too rather than left open-ended -- an
/// unreapable group surfaces as an error instead of hanging this call (and its caller)
/// forever.
///
/// `reload::run_pba` (Phase 8) and `main.rs`'s PBA swap wiring (Phase 14) are this function's
/// real callers: `run_pba` reaps an aborted Candidate on every failure path, and `main.rs`
/// reaps the superseded generation once a swap's Swap messages have been sent.
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
        TimeoutRace::StillRunning => Err(io::Error::other(
            "process group did not exit even after SIGKILL (likely stuck in uninterruptible I/O)",
        )),
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

    // Reproducing the actual OS-level race (timing a real SIGKILL delivery to land exactly on
    // a `tokio::time::timeout` deadline) isn't reliably possible without flaking -- 320 stress
    // runs of `reap_process_group_escalates_a_sigterm_ignoring_child_to_sigkill` never hit it.
    // These two tests instead cover the pure classification decision directly, with a
    // fabricated `ExitStatus` (`ExitStatusExt::from_raw`, no real process involved) standing in
    // for whatever `try_wait` would have returned. Before the SIGKILL-branch fix, only the
    // post-SIGTERM race had a check to test this way at all -- that asymmetry was the bug.

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

        assert_eq!(race, TimeoutRace::StillRunning, "a still-running child within a short grace must not be misreported as exited");

        // reap_process_group isn't used here (a signal was never sent above), so clean up
        // directly rather than leaking the sleep for the rest of the test run.
        child.kill().await.expect("cleanup kill failed");
    }

    /// Polls `condition` until it's true or `timeout` elapses, sleeping briefly between
    /// checks -- used below to wait out a grandchild's `/proc` teardown without a flaky
    /// single-shot check immediately after its parent is reaped.
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

        // Clean up rather than leaking the sleep for the rest of the test run.
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
        // Give the shell a moment to actually execute `trap '' TERM` before signaling it --
        // without this, a SIGTERM racing the just-forked shell's own startup can land before
        // the trap is installed, hitting the default (terminate) disposition instead and
        // flaking this test into the clean-exit branch.
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
        // Reap it directly (bypassing reap_process_group) so the group has zero members left
        // by the time we signal it -- killpg against a pgid nothing belongs to anymore
        // returns ESRCH, the exact race this helper exists to swallow instead of erroring out
        // of a reap that should otherwise proceed.
        child.wait().await.expect("wait should reap the already-exited child");

        signal_group_best_effort(pgid, Signal::SIGTERM)
            .expect("signaling an already-gone process group must not surface ESRCH as an error");
    }

    #[tokio::test]
    async fn reap_process_group_kills_the_whole_group_not_just_the_direct_child() {
        // The direct child backgrounds a grandchild without calling setsid/setpgid itself,
        // so the grandchild inherits the child's new group. It prints the grandchild's pid
        // via `$!` and then blocks in a bare `wait` (waits for every background job), which
        // keeps the shell itself alive as a real second process rather than being tail-call
        // exec-replaced -- so there's a real parent/grandchild pair sharing one group.
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
        // The piped variant must not have silently dropped the process-group behavior the whole
        // primitive exists for -- same assertion as spawn_group_leader's own coverage.
        let mut child = spawn_group_leader_piped("sh", &sh_args("sleep 5"), &[]).expect("failed to spawn");
        let child_pid = child.id().expect("freshly spawned child has a pid");

        let child_pgid = nix::unistd::getpgid(Some(Pid::from_raw(child_pid as i32))).expect("getpgid on the child");
        let our_pgid = nix::unistd::getpgrp();
        assert_ne!(child_pgid, our_pgid, "child should not inherit the test process's own group");

        reap_process_group(&mut child, Duration::from_millis(200)).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn spawn_group_leader_piped_pipes_stdout_and_stderr_separately_with_the_real_exit_code() {
        let mut child =
            spawn_group_leader_piped("sh", &sh_args("echo line1; echo line2 >&2; exit 3"), &[]).expect("failed to spawn");

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let stdout_line = BufReader::new(stdout).lines().next_line().await.unwrap().unwrap();
        let stderr_line = BufReader::new(stderr).lines().next_line().await.unwrap().unwrap();
        assert_eq!(stdout_line, "line1");
        assert_eq!(stderr_line, "line2");

        let status = child.wait().await.expect("wait failed");
        assert_eq!(status.code(), Some(3));
    }
}
