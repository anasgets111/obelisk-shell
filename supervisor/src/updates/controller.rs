//! [`UpdatesController`]: the `oblisk.updates` write-action dispatcher and state owner
//! (ADR-0034). Same interval-suspend-at-zero *shape* as `hardware::sysinfo::controller`'s
//! watch-channel scheduler, but deliberately zero shared code -- the ADR's own instruction
//! ("the user asked for these to have nothing to do with each other"), so this is a fresh,
//! independent implementation, not a shared helper. Split from `updates` -- see `updates/mod.rs`
//! for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::check::{UpdateCandidate, check_for_updates};
use super::install::{needs_reboot, parse_install_step};
use super::pacman_conf::resolve_repo_servers;
use crate::process;

/// `oblisk.updates`'s combined payload. `check_error`/`install_error` are `None` when nothing's
/// gone wrong -- not a fabricated empty string. `install_total_steps == 0` while `installing` is
/// true means the transaction size isn't known yet (pacman hasn't printed it), matching the
/// dotfiles' `UpdateService.qml`'s `progressDeterminate` concept without a separate bool field.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct UpdatesState {
    pub count: u32,
    pub packages: Vec<UpdateCandidate>,
    pub last_successful_check: Option<i64>,
    pub check_error: Option<String>,
    pub installing: bool,
    pub install_current_step: u32,
    pub install_total_steps: u32,
    pub install_current_package: String,
    pub install_error: Option<String>,
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatesSignal {
    Changed,
}

/// `updates:configure({interval})`'s `arguments: [{interval}]` -- a table argument (mirrors
/// `sysinfo:configure`'s shape, ADR-0034's own choice of braces in `updates:configure({interval})`
/// over a bare positional arg), even though there's only one field today.
pub fn parse_configure_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_object()?.get("interval")?.as_u64()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollMode {
    Dormant,
    Ticking(Duration),
}

fn poll_mode(interval: Duration) -> PollMode {
    if interval.is_zero() { PollMode::Dormant } else { PollMode::Ticking(interval) }
}

/// `Clone` (mirrors `IdleController`/`KeyboardController`) so `main.rs` can hand a cheap
/// `Arc`-backed copy to the `tokio::spawn`ed task `updates:install()`'s dispatch arm needs.
#[derive(Clone)]
pub struct UpdatesController {
    state: Arc<Mutex<UpdatesState>>,
    interval_tx: watch::Sender<Duration>,
    events: UnboundedSender<UpdatesSignal>,
}

impl UpdatesController {
    /// `pacman_conf_path`/`pacman_db_root` (real defaults `/etc/pacman.conf`/`/var/lib/pacman`)
    /// are injected, not hardcoded (`docs/oblisk-tdd-test-harness.md`'s convention). Starts
    /// dormant (`Duration::ZERO`) -- nothing checks for updates until Lua calls
    /// `updates:configure` at least once, matching every other opt-in interval mechanism already
    /// in this codebase.
    pub fn new(pacman_conf_path: PathBuf, pacman_db_root: PathBuf, events: UnboundedSender<UpdatesSignal>) -> Self {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (interval_tx, interval_rx) = watch::channel(Duration::ZERO);
        tokio::spawn(run_check_task(pacman_conf_path, pacman_db_root, interval_rx, Arc::clone(&state), events.clone()));
        Self { state, interval_tx, events }
    }

    pub fn configure(&self, interval_secs: u64) {
        if self.interval_tx.send(Duration::from_secs(interval_secs)).is_err() {
            eprintln!("updates: configure called but the check task is gone; ignored");
        }
    }

    /// `updates:install()`. A no-op (logged) if an install is already running -- pacman itself
    /// doesn't support two concurrent transactions against the same db lock, so a second
    /// `install()` call while one is in flight would just fail loudly against that lock; better
    /// to short-circuit here with a clear log line. The check-and-set is one atomic critical
    /// section under a single lock acquisition (Correctness review): two `updates:install()`
    /// commands dispatched close together each land in their own `tokio::spawn`ed task on
    /// `main.rs`'s multi-thread runtime, so a check and a *separate* later set (two lock
    /// acquisitions, as this used to be) leaves a real window for both to observe `installing
    /// == false` and both launch a real `pkexec pacman -Syu` concurrently against the same
    /// system pacman db.
    pub async fn install(&self) {
        {
            let mut guard = self.state.lock().unwrap();
            if guard.installing {
                drop(guard);
                eprintln!("updates: install() called while an install is already running; ignored");
                return;
            }
            guard.installing = true;
            guard.install_current_step = 0;
            guard.install_total_steps = 0;
            guard.install_current_package = String::new();
            guard.install_error = None;
        }
        let _ = self.events.send(UpdatesSignal::Changed);
        run_install(Arc::clone(&self.state), self.events.clone()).await;
    }

    pub fn snapshot(&self) -> UpdatesState {
        self.state.lock().unwrap().clone()
    }
}

/// Copies `pacman_db_root`'s `local/` subdirectory (the installed-package metadata `alpm` reads
/// -- `sync/` doesn't need pre-copying, `check_for_updates`'s own `update(true)` overwrites it
/// with a fresh download regardless of what's there) into a fresh `tempfile::tempdir()`, then
/// runs [`check_for_updates`] against that throwaway copy -- never the real
/// `pacman_db_root` (`checkupdates`'s own real-world approach, ADR-0034).
///
/// Known limitation (Correctness review, not fixed): no coordination against a concurrently
/// running real install (`run_install`) mutating this same `local/` directory. A scheduled
/// check that happens to overlap an in-flight install can surface a spurious, transient
/// `check_error` (a file disappearing mid-copy) or copy a momentarily inconsistent snapshot --
/// self-heals on the next scheduled check either way, never corrupts real state (this function
/// only ever reads the real db, the throwaway copy is discarded after each check). Real
/// cross-task coordination to close this window is more machinery than a self-healing,
/// read-only race justifies right now.
fn check_against_a_throwaway_copy(pacman_conf_path: &Path, pacman_db_root: &Path) -> Result<Vec<UpdateCandidate>, String> {
    let throwaway = tempfile::tempdir().map_err(|err| format!("failed to create a throwaway temp dir: {err}"))?;
    let local_src = pacman_db_root.join("local");
    let local_dst = throwaway.path().join("local");
    copy_dir_recursive(&local_src, &local_dst).map_err(|err| format!("failed to copy {} to a throwaway dir: {err}", local_src.display()))?;

    let repos = resolve_repo_servers(pacman_conf_path);
    if repos.is_empty() {
        return Err(format!("no repos resolved from {}", pacman_conf_path.display()));
    }

    check_for_updates(Path::new("/"), throwaway.path(), &repos).map_err(|err| err.to_string())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Runs until every `UpdatesController` (and its `Clone`s) drops. `alpm`'s types aren't `Send`
/// (raw C pointers under the hood), and the sync itself is genuinely blocking network I/O, so
/// each check runs inside `tokio::task::spawn_blocking` -- never awaited inline, matching this
/// codebase's async-hygiene rule (build-steps.md Phase 9) for the same reason
/// `hardware::idle::notify::connect_wayland_idle`'s real-Wayland setup does.
async fn run_check_task(pacman_conf_path: PathBuf, pacman_db_root: PathBuf, mut interval_rx: watch::Receiver<Duration>, state: Arc<Mutex<UpdatesState>>, events: UnboundedSender<UpdatesSignal>) {
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                if interval_rx.changed().await.is_err() {
                    return;
                }
            }
            PollMode::Ticking(duration) => {
                let mut ticker = tokio::time::interval(duration);
                // Correctness review: a check can genuinely run longer than a short configured
                // interval (real network I/O, unlike sysinfo's fast local sysfs/procfs reads).
                // `tokio::time::interval`'s default `MissedTickBehavior::Burst` would then fire
                // every missed tick back-to-back the moment the slow check finally returns to
                // this `select!`, hammering the mirrors instead of settling back into the
                // configured cadence -- `Delay` instead just resumes ticking `duration` after
                // the check actually finished.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ticker.tick().await; // tokio::time::interval's first tick fires immediately; consume it unused
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            let conf_path = pacman_conf_path.clone();
                            let db_root = pacman_db_root.clone();
                            let result = tokio::task::spawn_blocking(move || check_against_a_throwaway_copy(&conf_path, &db_root)).await;
                            let mut guard = state.lock().unwrap();
                            match result {
                                Ok(Ok(candidates)) => {
                                    guard.count = candidates.len() as u32;
                                    guard.packages = candidates;
                                    guard.last_successful_check = Some(now_unix());
                                    guard.check_error = None;
                                }
                                Ok(Err(err)) => guard.check_error = Some(err),
                                Err(join_err) => guard.check_error = Some(format!("check task panicked: {join_err}")),
                            }
                            drop(guard);
                            let _ = events.send(UpdatesSignal::Changed);
                        }
                        changed = interval_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break; // interval reconfigured -- rebuild dormant/ticking in the outer loop
                        }
                    }
                }
            }
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The real, system-modifying install: `pkexec pacman -Syu --noconfirm` against the real
/// `/etc/pacman.conf`/`/var/lib/pacman` as root, no throwaway anything (unlike `check`'s
/// read-only sync). Streams stdout line by line, parsing progress via
/// `install::parse_install_step` and writing it into `state` as it goes -- the capability owns
/// install + progress, Lua never sees raw subprocess output (ADR-0034). Assumes `state.installing`
/// is already `true` and its progress fields already reset -- `UpdatesController::install`'s own
/// atomic check-and-set does that, as one critical section with the "already running?" check
/// (Correctness review); this function only ever runs once that's already been established.
async fn run_install(state: Arc<Mutex<UpdatesState>>, events: UnboundedSender<UpdatesSignal>) {
    let child = match process::spawn_group_leader_piped("pkexec", &["pacman".to_string(), "-Syu".to_string(), "--noconfirm".to_string()], &[]) {
        Ok(child) => child,
        Err(err) => {
            let mut guard = state.lock().unwrap();
            guard.installing = false;
            guard.install_error = Some(format!("failed to spawn pkexec: {err}"));
            drop(guard);
            let _ = events.send(UpdatesSignal::Changed);
            return;
        }
    };
    run_install_with_child(state, events, child).await;
}

/// Split from [`run_install`] so the stdout-driven progress loop can be exercised against a
/// stub child process in a test, without needing a real `pkexec`/`pacman` on the test machine.
/// Sends `UpdatesSignal::Changed` on every parsed progress line, not just at the end -- ADR-0034:
/// install's progress fields "ride the updates signal" the same way a check's results do, so a
/// live install shows incremental per-package progress to Lua, not just a start/end jump.
async fn run_install_with_child(state: Arc<Mutex<UpdatesState>>, events: UnboundedSender<UpdatesSignal>, mut child: tokio::process::Child) {
    // Drained concurrently on its own task, not left unread (Correctness review): a real
    // `pacman -Syu` upgrade routinely writes one "installed as .pacnew"/conflict warning per
    // touched config file to stderr -- easily enough to fill the pipe's ~64KiB kernel buffer on
    // a moderate upgrade. Once full, pacman's own `write()` to stderr blocks, which blocks the
    // whole (single-threaded) pacman process, which means it never produces more stdout and
    // never exits -- `child.wait().await` below would then never return, wedging `installing`
    // at `true` permanently. Logged (not silently discarded) so real warnings stay visible.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("updates: pkexec pacman stderr: {line}");
            }
        });
    }

    // Plain local `Vec`, not `Arc<Mutex<_>>` (Standards review): every read and write happens
    // sequentially within this one function's own loop, never shared with another task -- unlike
    // `state`, which genuinely is shared with `UpdatesController::snapshot()`.
    let mut installed_packages: Vec<String> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(step) = parse_install_step(&line) {
                let mut guard = state.lock().unwrap();
                guard.install_current_step = step.current;
                guard.install_total_steps = step.total;
                guard.install_current_package = step.package.clone();
                drop(guard);
                installed_packages.push(step.package);
                let _ = events.send(UpdatesSignal::Changed);
            }
        }
    }

    let status = child.wait().await;
    let mut guard = state.lock().unwrap();
    guard.installing = false;
    match status {
        Ok(status) if status.success() => {
            // Accumulates (OR), never overwrites: a reboot owed from an earlier install this
            // process lifetime (e.g. a kernel package updated, then the user ran a second,
            // unrelated install without rebooting in between) must not be silently cleared just
            // because *this* install didn't itself touch the kernel. Only a real reboot resets
            // it, via a fresh `UpdatesState::default()` on the next Supervisor startup -- self-
            // caught while addressing the Spec review's staleness question, not a filed finding.
            guard.reboot_required |= needs_reboot(&installed_packages);
        }
        Ok(status) => guard.install_error = Some(format!("pkexec pacman exited with {status}")),
        Err(err) => guard.install_error = Some(format!("failed to wait on pkexec pacman: {err}")),
    }
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_configure_args_reads_the_interval_from_a_table() {
        let args = vec![serde_json::json!({"interval": 3600})];
        assert_eq!(parse_configure_args(&args), Some(3600));
    }

    #[test]
    fn parse_configure_args_is_none_for_a_missing_or_wrong_typed_argument() {
        assert_eq!(parse_configure_args(&[]), None);
        assert_eq!(parse_configure_args(&[serde_json::json!(3600)]), None);
        assert_eq!(parse_configure_args(&[serde_json::json!({"wrong_key": 3600})]), None);
    }

    #[test]
    fn poll_mode_is_dormant_at_zero_and_ticking_otherwise() {
        assert_eq!(poll_mode(Duration::ZERO), PollMode::Dormant);
        assert_eq!(poll_mode(Duration::from_secs(1)), PollMode::Ticking(Duration::from_secs(1)));
    }

    #[test]
    fn updates_state_default_has_no_updates_and_no_errors() {
        let state = UpdatesState::default();
        assert_eq!(state.count, 0);
        assert!(state.packages.is_empty());
        assert_eq!(state.last_successful_check, None);
        assert_eq!(state.check_error, None);
        assert!(!state.installing);
    }

    #[tokio::test]
    async fn run_install_with_child_parses_progress_and_detects_a_successful_completion() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo ':: Synchronizing package databases...'; echo '(1/2) installing nss (3.127-1 -> 3.128-1)'; echo '(2/2) upgrading gnome-autoar'; exit 0".to_string()],
            &[],
        )
        .expect("spawn a stub install script");

        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!(snapshot.install_current_step, 2);
        assert_eq!(snapshot.install_total_steps, 2);
        assert_eq!(snapshot.install_current_package, "gnome-autoar");
        assert_eq!(snapshot.install_error, None);

        // Correctness: progress must ride the updates signal per-line, not just at the end
        // (ADR-0034) -- two progress lines plus the final completion signal.
        let mut signal_count = 0;
        while events_rx.try_recv().is_ok() {
            signal_count += 1;
        }
        assert_eq!(signal_count, 3, "two progress-line signals plus one completion signal");
    }

    #[tokio::test]
    async fn run_install_with_child_reports_a_nonzero_exit_as_an_install_error() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "exit 1".to_string()], &[]).expect("spawn a failing stub");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert!(snapshot.install_error.is_some());
    }

    #[tokio::test]
    async fn run_install_with_child_flags_reboot_required_when_a_kernel_package_was_installed() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "echo '(1/1) upgrading linux'; exit 0".to_string()], &[]).expect("spawn a stub install script");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, child).await;

        assert!(state.lock().unwrap().reboot_required);
    }

    #[tokio::test]
    async fn reboot_required_stays_true_across_a_later_install_that_did_not_touch_the_kernel() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));

        let kernel_child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "echo '(1/1) upgrading linux'; exit 0".to_string()], &[]).expect("spawn a stub install script");
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, kernel_child).await;
        assert!(state.lock().unwrap().reboot_required, "first install touched the kernel");

        let unrelated_child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "echo '(1/1) upgrading nss'; exit 0".to_string()], &[]).expect("spawn a stub install script");
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, unrelated_child).await;

        assert!(state.lock().unwrap().reboot_required, "a later install with no kernel package must not clear a still-pending reboot");
    }

    // ---- copy_dir_recursive ----

    #[test]
    fn copy_dir_recursive_copies_nested_files_and_directories() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("top.txt"), "top").unwrap();
        std::fs::create_dir(src.path().join("nested")).unwrap();
        std::fs::write(src.path().join("nested").join("inner.txt"), "inner").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dest_path = dst.path().join("copy");
        copy_dir_recursive(src.path(), &dest_path).unwrap();

        assert_eq!(std::fs::read_to_string(dest_path.join("top.txt")).unwrap(), "top");
        assert_eq!(std::fs::read_to_string(dest_path.join("nested").join("inner.txt")).unwrap(), "inner");
    }

    #[test]
    fn copy_dir_recursive_errors_against_a_nonexistent_source() {
        let dst = tempfile::tempdir().unwrap();
        assert!(copy_dir_recursive(Path::new("/does/not/exist"), &dst.path().join("copy")).is_err());
    }
}
