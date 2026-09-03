//! [`UpdatesController`]: the `oblisk.updates` write-action dispatcher and state owner
//! (ADR-0034). Split from `updates` -- see `updates/mod.rs` for the module-level doc.

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

/// `oblisk.updates`'s combined payload. `check_error`/`install_error` are `None` when
/// nothing's gone wrong, not a fabricated empty string. `install_total_steps == 0` while
/// `installing` is true means the transaction size isn't known yet (pacman hasn't printed it).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct UpdatesState {
    /// How many packages have a newer version in the synced repos. Always equal to
    /// `#packages`, and carried separately so a badge does not have to walk the list.
    pub count: u32,
    /// What would be upgraded, one entry each. A failed check leaves this and
    /// [`UpdatesState::count`] on the last good answer rather than clearing them, so a config
    /// keeps showing the count it knows while [`UpdatesState::check_error`] explains the gap.
    pub packages: Vec<UpdateCandidate>,
    /// Unix seconds at the end of the last check that completed without error, or `nil` if none has
    /// since this session started. A failed check leaves it on the older, still-true value.
    pub last_successful_check: Option<i64>,
    /// Why the last check failed, or `nil` when the last one worked. A check runs against a
    /// throwaway copy of the pacman database, so this is a network or parse failure, never a
    /// half-applied change to the system.
    pub check_error: Option<String>,
    /// An install is running. The four `install_*` fields below only mean anything while this is
    /// true; `updates:install` refuses a second one.
    pub installing: bool,
    /// Which package of the transaction pacman is on, its own 1-based `(2/5)` counter.
    /// `0` before the first line is parsed.
    pub install_current_step: u32,
    /// How many packages the transaction has. `0` while [`UpdatesState::installing`] is true means
    /// pacman has not printed a step line yet, so a progress bar has no denominator: show it
    /// as indeterminate rather than dividing.
    pub install_total_steps: u32,
    /// The package name from the step line pacman is on. Empty string before the first one, not
    /// `nil`, because a name is always a string once the transaction is under way.
    pub install_current_package: String,
    /// Why the last install failed, or `nil`. Unlike a check, this one ran as root against the
    /// real database, so a failure here can leave packages partly upgraded.
    pub install_error: Option<String>,
    /// A `linux` or `linux-*` package was installed at some point this session. Sticky on purpose:
    /// once set it stays set through later installs that do not touch the kernel, because the
    /// running kernel is still the old one until the machine restarts.
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatesSignal {
    Changed,
}

/// `updates:configure({interval})`'s `arguments: [{interval}]` -- a table argument (ADR-0034),
/// even though there's only one field today.
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

/// `Clone` so `main.rs` can hand a cheap `Arc`-backed copy to the `tokio::spawn`ed task
/// `updates:install()`'s dispatch arm needs.
#[derive(Clone)]
pub struct UpdatesController {
    state: Arc<Mutex<UpdatesState>>,
    interval_tx: watch::Sender<Duration>,
    events: UnboundedSender<UpdatesSignal>,
}

impl UpdatesController {
    /// `pacman_conf_path`/`pacman_db_root` (real defaults `/etc/pacman.conf`/`/var/lib/pacman`)
    /// are injected, not hardcoded. Starts dormant (`Duration::ZERO`) -- nothing checks for
    /// updates until Lua calls `updates:configure` at least once.
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

    /// `updates:install()`. A no-op (logged) if an install is already running -- pacman doesn't
    /// support two concurrent transactions against the same db lock. The check-and-set is one
    /// atomic critical section under a single lock acquisition: two `install()` calls dispatched
    /// close together could otherwise both observe `installing == false` and both launch
    /// `pkexec pacman -Syu` concurrently against the same db.
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

/// Points a fresh `tempfile::tempdir()` at `pacman_db_root`'s `local/` with one symlink, then
/// syncs and checks against that throwaway db root, never the real `pacman_db_root` (ADR-0034,
/// amended ADR-0113). Only `sync/` is written, and it is written inside the temp dir.
///
/// A symlink and not a copy, which is what `checkupdates` itself does (`ln -s "${DBPath}/local"
/// "$CHECKUPDATES_DB"`): `local/` is the installed-package metadata, which `syncdbs_mut().update()`
/// only reads. The copy this replaces walked ~1,500 package directories off disk on every single
/// check, and ADR-0034's own note says where it came from -- the throwaway prototype that proved
/// the sync needs no `fakeroot` copied the whole tree, and the copy came along with the answer.
///
/// Known limitation, now sharper than it was: with a copy, a check ran against a snapshot; with a
/// symlink it reads the live directory, so a concurrent real install can be observed mid-write. The
/// window is the same one `checkupdates` lives with, the result is a spurious transient
/// `check_error`, and it self-heals on the next scheduled check.
fn check_against_a_throwaway_copy(
    pacman_conf_path: &Path,
    pacman_db_root: &Path,
) -> Result<Vec<UpdateCandidate>, String> {
    let throwaway = tempfile::tempdir().map_err(|err| format!("failed to create a throwaway temp dir: {err}"))?;
    link_local_db(pacman_db_root, throwaway.path())?;

    let repos = resolve_repo_servers(pacman_conf_path);
    if repos.is_empty() {
        return Err(format!("no repos resolved from {}", pacman_conf_path.display()));
    }

    check_for_updates(Path::new("/"), throwaway.path(), &repos).map_err(|err| err.to_string())
}

/// Links `pacman_db_root/local` in as `throwaway/local`, the one name `alpm` looks for when it
/// reads installed packages out of a db root. Split out from
/// [`check_against_a_throwaway_copy`] only so the name and the read-through are testable without
/// a mirror: everything else that function does needs the network.
fn link_local_db(pacman_db_root: &Path, throwaway: &Path) -> Result<(), String> {
    let local_src = pacman_db_root.join("local");
    // Checked, because `symlink` will happily point at nothing and a dangling `local/` is not an
    // error to `alpm` -- it is an empty installed set, which reads as "every package on the
    // mirror is an update". The copy this replaces failed loudly on a missing source; so does this.
    if !local_src.is_dir() {
        return Err(format!("{} is not a directory; cannot check updates against it", local_src.display()));
    }
    std::os::unix::fs::symlink(&local_src, throwaway.join("local"))
        .map_err(|err| format!("failed to link {} into a throwaway dir: {err}", local_src.display()))
}

/// Runs until every `UpdatesController` (and its `Clone`s) drops. `alpm`'s types aren't
/// `Send` and the sync is genuinely blocking network I/O, so each check runs inside
/// `tokio::task::spawn_blocking`, never awaited inline.
async fn run_check_task(
    pacman_conf_path: PathBuf,
    pacman_db_root: PathBuf,
    mut interval_rx: watch::Receiver<Duration>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
) {
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
                // A check can genuinely run longer than a short configured interval (real
                // network I/O); the default `Burst` behavior would then fire every missed tick
                // back-to-back, hammering the mirrors -- `Delay` resumes ticking after the check finishes.
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
/// `/etc/pacman.conf`/`/var/lib/pacman` as root, no throwaway copy. Streams stdout line by
/// line, parsing progress via [`parse_install_step`] and writing it into `state` as it goes --
/// Lua never sees raw subprocess output (ADR-0034). Assumes `state.installing` and its
/// progress fields are already set by [`UpdatesController::install`]'s atomic check-and-set.
async fn run_install(state: Arc<Mutex<UpdatesState>>, events: UnboundedSender<UpdatesSignal>) {
    let child = match process::spawn_group_leader_piped(
        "pkexec",
        &["pacman".to_string(), "-Syu".to_string(), "--noconfirm".to_string()],
        &[],
    ) {
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

/// Split from [`run_install`] so the stdout-driven progress loop can be tested against a stub
/// child process, without a real `pkexec`/`pacman`. Sends `UpdatesSignal::Changed` on every
/// parsed progress line (ADR-0034), not just at the end.
async fn run_install_with_child(
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
    mut child: tokio::process::Child,
) {
    // Drained concurrently on its own task, not left unread: a real `pacman -Syu` upgrade can
    // write enough stderr warnings to fill the pipe's ~64KiB kernel buffer, which blocks
    // pacman's single-threaded process and wedges `installing` at `true` forever. Logged, not discarded.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("updates: pkexec pacman stderr: {line}");
            }
        });
    }

    // Plain local `Vec`, not `Arc<Mutex<_>>`: every read/write happens sequentially within
    // this loop, never shared with another task -- unlike `state`.
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
            // Accumulates (OR), never overwrites: a reboot owed from an earlier install must
            // not be cleared just because this install didn't touch the kernel.
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

        // Correctness: progress must ride the updates signal per-line, not just at the end (ADR-0034).
        let mut signal_count = 0;
        while events_rx.try_recv().is_ok() {
            signal_count += 1;
        }
        assert_eq!(signal_count, 3, "two progress-line signals plus one completion signal");
    }

    #[tokio::test]
    async fn run_install_with_child_reports_a_nonzero_exit_as_an_install_error() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "exit 1".to_string()], &[])
            .expect("spawn a failing stub");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert!(snapshot.install_error.is_some());
    }

    #[tokio::test]
    async fn run_install_with_child_flags_reboot_required_when_a_kernel_package_was_installed() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo '(1/1) upgrading linux'; exit 0".to_string()],
            &[],
        )
        .expect("spawn a stub install script");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, child).await;

        assert!(state.lock().unwrap().reboot_required);
    }

    #[tokio::test]
    async fn reboot_required_stays_true_across_a_later_install_that_did_not_touch_the_kernel() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));

        let kernel_child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo '(1/1) upgrading linux'; exit 0".to_string()],
            &[],
        )
        .expect("spawn a stub install script");
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, kernel_child).await;
        assert!(state.lock().unwrap().reboot_required, "first install touched the kernel");

        let unrelated_child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo '(1/1) upgrading nss'; exit 0".to_string()],
            &[],
        )
        .expect("spawn a stub install script");
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::clone(&state), events_tx, unrelated_child).await;

        assert!(
            state.lock().unwrap().reboot_required,
            "a later install with no kernel package must not clear a still-pending reboot"
        );
    }

    // ---- link_local_db ----

    #[test]
    fn the_throwaway_db_root_reads_installed_packages_through_a_link_named_local() {
        // The name matters as much as the read: `alpm` looks for `local/` under the db root it is
        // given, so a link under any other name is an empty db and every package reads as new.
        let real = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(real.path().join("local").join("bash-5.3-1")).unwrap();
        std::fs::write(real.path().join("local").join("bash-5.3-1").join("desc"), "%NAME%\nbash\n").unwrap();

        let throwaway = tempfile::tempdir().unwrap();
        link_local_db(real.path(), throwaway.path()).unwrap();

        let linked = throwaway.path().join("local");
        assert!(linked.symlink_metadata().unwrap().is_symlink(), "local must be a link, not a copied tree");
        assert_eq!(
            std::fs::read_to_string(linked.join("bash-5.3-1").join("desc")).unwrap(),
            "%NAME%\nbash\n",
            "the real package metadata must be readable through the link"
        );
    }

    #[test]
    fn linking_into_a_throwaway_root_that_already_holds_a_local_is_an_error_not_a_silent_reuse() {
        // `symlink` refuses an existing destination. Surfacing that as a `check_error` beats
        // checking against whatever was there: a reused temp dir would report stale packages.
        let real = tempfile::tempdir().unwrap();
        let throwaway = tempfile::tempdir().unwrap();
        std::fs::create_dir(throwaway.path().join("local")).unwrap();

        assert!(link_local_db(real.path(), throwaway.path()).is_err());
    }

    #[test]
    fn a_db_root_with_no_local_directory_is_refused_rather_than_linked_to_nothing() {
        let missing = tempfile::tempdir().unwrap();
        let throwaway = tempfile::tempdir().unwrap();

        assert!(link_local_db(&missing.path().join("no-such-root"), throwaway.path()).is_err());
        assert!(!throwaway.path().join("local").exists());
    }
}
