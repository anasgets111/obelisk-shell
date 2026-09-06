//! [`UpdatesController`]: `oblisk.updates` write-action dispatcher and state owner (ADR-0034).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::backend::{Backend, UpdateCandidate};
use crate::process;

/// `oblisk.updates` payload. `check_error`/`install_error` are `None` when clear. While
/// `installing`, `install_total_steps == 0` means the manager has not printed the transaction size.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct UpdatesState {
    /// Package manager name, or `nil` when unsupported. Available before any check and used by an
    /// indicator to decide whether it belongs on the bar (ADR-0134), e.g. `"pacman"`.
    pub package_manager: Option<String>,
    /// Number of packages with newer synced-repo versions. Always `#packages`, duplicated so a
    /// badge need not walk the list.
    pub count: u32,
    /// Packages that would upgrade, one per entry. A failed check preserves the last good list and
    /// [`UpdatesState::count`] while [`UpdatesState::check_error`] reports the gap.
    pub packages: Vec<UpdateCandidate>,
    /// Unix seconds when the last check completed successfully, or `nil` this session. Failed
    /// checks preserve the older value.
    pub last_successful_check: Option<i64>,
    /// Last check error, or `nil` after success. Checks never modify the system
    /// (`Backend::check`), so this is a network/parse failure, not a half-applied change.
    pub check_error: Option<String>,
    /// A check is running. Set before sync and cleared when its result is written, with a push at
    /// both edges for spinners/refresh controls. `updates:check` refuses a second check while true.
    pub checking: bool,
    /// Consecutive check failures, reset to `0` by the first success. Thresholds belong in config.
    pub consecutive_check_failures: u32,
    /// An install is running. `install_*` describe a started run; `updates:install` refuses a
    /// second one while true.
    pub installing: bool,
    /// Current package number, using the manager's 1-based `(2/5)` counter. `0` before progress.
    pub install_current_step: u32,
    /// Transaction package count. `0` while [`UpdatesState::installing`] means no step line yet;
    /// show progress as indeterminate rather than divide.
    pub install_total_steps: u32,
    /// Current package name from the step line. Empty before the first line, never `nil`.
    pub install_current_package: String,
    /// Manager exit code from the last install: `0` success, its code on failure, `nil` before one
    /// finishes. Together with [`UpdatesState::install_log`], it is the failure fact; wording such
    /// as network, disk, or signature error belongs in config (ADR-0113 amendment).
    pub install_exit_code: Option<i32>,
    /// Unix seconds when the last install stopped, regardless of outcome. Use it with the caller's
    /// install start to measure duration.
    pub install_finished_at: Option<i64>,
    /// Last install output, newest last, stdout/stderr interleaved by arrival (two readers make the
    /// cross-stream order inexact). Keeps the last 200 lines; cleared when an install starts.
    pub install_log: Vec<String>,
    /// Why Supervisor never got a manager answer: spawn or wait failed. Unlike
    /// [`UpdatesState::install_exit_code`], this means the install was never answered and the
    /// failure is Supervisor's.
    pub install_error: Option<String>,
    /// A kernel package was installed this session per `Backend::needs_reboot`. Sticky: later
    /// installs cannot clear it because the running kernel remains old until restart.
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatesSignal {
    Changed,
}

/// What `updates:configure({ interval, checked_at })` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatesConfigure {
    /// Seconds between scheduled checks. Zero is dormant: nothing checks until a `check` asks.
    pub interval_secs: u64,
    /// Remembered Unix time of the last successful check, likely from `system.state`. Optional
    /// seed,
    /// not override: used only before this process has checked, so restarts can answer "has an hour
    /// passed?" without starting over.
    pub checked_at: Option<i64>,
}

/// `updates:configure({interval})` takes one table argument (ADR-0034). A wrong-typed present key
/// drops the whole call.
pub fn parse_configure_args(arguments: &[serde_json::Value]) -> Option<UpdatesConfigure> {
    let table = arguments.first()?.as_object()?;
    let interval_secs = table.get("interval")?.as_u64()?;
    let checked_at = match table.get("checked_at") {
        Some(value) => Some(value.as_i64()?),
        None => None,
    };
    Some(UpdatesConfigure { interval_secs, checked_at })
}

/// Tail length for [`UpdatesState::install_log`]. Enough to hold a failure and nearby lines; a
/// 2,000-package run belongs in a file, not a state payload reserialized on every progress line.
const LOG_TAIL_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollMode {
    Dormant,
    Ticking(Duration),
}

fn poll_mode(interval: Duration) -> PollMode {
    if interval.is_zero() { PollMode::Dormant } else { PollMode::Ticking(interval) }
}

/// Cloneable so `main.rs` can hand an `Arc`-backed copy to the spawned install task.
#[derive(Clone)]
pub struct UpdatesController {
    /// Package manager backend, or `None`; actions then no-op instead of failing a check.
    backend: Option<Arc<dyn Backend>>,
    state: Arc<Mutex<UpdatesState>>,
    interval_tx: watch::Sender<Duration>,
    /// `updates:check` nudge. Capacity one plus `try_send` collapses a burst into one check.
    check_now_tx: tokio::sync::mpsc::Sender<()>,
    events: UnboundedSender<UpdatesSignal>,
}

impl UpdatesController {
    /// Detects the package manager and starts a dormant scheduler (`Duration::ZERO`) until
    /// `updates:configure`. Pushes immediately so `package_manager` can decide indicator presence,
    /// including on machines with no backend and no later scheduler event. This makes the indicator
    /// appear at login rather than after the first check.
    pub fn new(events: UnboundedSender<UpdatesSignal>) -> Self {
        Self::with_backend(super::backend::detect().map(Arc::from), events)
    }

    /// [`UpdatesController::new`] with a caller-supplied backend for scheduler tests.
    fn with_backend(backend: Option<Arc<dyn Backend>>, events: UnboundedSender<UpdatesSignal>) -> Self {
        let state = Arc::new(Mutex::new(UpdatesState {
            package_manager: backend.as_ref().map(|backend| backend.name().to_string()),
            ..UpdatesState::default()
        }));
        let (interval_tx, interval_rx) = watch::channel(Duration::ZERO);
        let (check_now_tx, check_now_rx) = tokio::sync::mpsc::channel(1);
        if let Some(backend) = backend.clone() {
            tokio::spawn(run_check_task(backend, interval_rx, check_now_rx, Arc::clone(&state), events.clone()));
        }
        let _ = events.send(UpdatesSignal::Changed);
        Self { backend, state, interval_tx, check_now_tx, events }
    }

    /// Sets the schedule and optionally seeds a remembered check time (ADR-0113 amendment). Uses
    /// the seed only before this process checks, never moving `last_successful_check` backwards.
    /// Seeding pushes because the field is Lua-visible.
    pub fn configure(&self, configure: UpdatesConfigure) {
        if self.backend.is_none() {
            return;
        }
        if let Some(checked_at) = configure.checked_at {
            let mut guard = self.state.lock().unwrap();
            if guard.last_successful_check.is_none() {
                guard.last_successful_check = Some(checked_at);
                drop(guard);
                let _ = self.events.send(UpdatesSignal::Changed);
            }
        }
        if self.interval_tx.send(Duration::from_secs(configure.interval_secs)).is_err() {
            eprintln!("updates: configure called but the check task is gone; ignored");
        }
    }

    /// `updates:check()`: runs one check regardless of schedule, including dormant mode. Refuses a
    /// second request while checking; the in-flight answer is the requested answer.
    pub fn check_now(&self) {
        if self.backend.is_none() {
            eprintln!("updates: check() called on a machine with no package manager this Supervisor speaks; ignored");
            return;
        }
        if self.state.lock().unwrap().checking {
            eprintln!("updates: check() called while a check is already running; ignored");
            return;
        }
        if self.check_now_tx.try_send(()).is_err() {
            eprintln!(
                "updates: check() could not be queued (one is already pending, or the check task is gone); ignored"
            );
        }
    }

    /// `updates:install()`. Logged no-op without a backend or during another install; package
    /// managers share one database lock. The check-and-set is one critical section, so concurrent
    /// calls cannot both observe `installing == false` and launch upgrades.
    pub async fn install(&self) {
        let Some(backend) = self.backend.clone() else {
            eprintln!("updates: install() called on a machine with no package manager this Supervisor speaks; ignored");
            return;
        };
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
            guard.install_exit_code = None;
            guard.install_finished_at = None;
            guard.install_log.clear();
        }
        let _ = self.events.send(UpdatesSignal::Changed);
        run_install(backend, Arc::clone(&self.state), self.events.clone()).await;
    }

    pub fn snapshot(&self) -> UpdatesState {
        self.state.lock().unwrap().clone()
    }
}

/// Runs until every `UpdatesController` (and its `Clone`s) drops. Spawned only with a backend. Each
/// check uses
/// `tokio::task::spawn_blocking`: `Backend::check` performs blocking network I/O and pacman's
/// `alpm` types are not `Send`.
async fn run_check_task(
    backend: Arc<dyn Backend>,
    mut interval_rx: watch::Receiver<Duration>,
    mut check_now_rx: tokio::sync::mpsc::Receiver<()>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
) {
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                // Dormant mode still answers `check_now`; a config may use a button without a
                // timer.
                tokio::select! {
                    changed = interval_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    asked = check_now_rx.recv() => {
                        if asked.is_none() {
                            return;
                        }
                        run_one_check(&backend, &state, &events).await;
                    }
                }
            }
            PollMode::Ticking(duration) => {
                let mut ticker = tokio::time::interval(duration);
                // Checks can exceed a short interval. `Burst` would hammer mirrors with missed
                // ticks; `Delay` resumes after the check.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // `interval` ticks immediately, so an hourly schedule checks now, not in an hour
                // (ADR-0113 amendment). Skip only when this process has a fresh check; the
                // controller outlives config generations; under the old unconditional consume,
                // every save reset the hour and a day of editing never checked at all.
                if !first_check_is_due(state.lock().unwrap().last_successful_check, now_unix(), duration) {
                    ticker.tick().await;
                }
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            run_one_check(&backend, &state, &events).await;
                        }
                        asked = check_now_rx.recv() => {
                            if asked.is_none() {
                                return;
                            }
                            run_one_check(&backend, &state, &events).await;
                        }
                        changed = interval_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break; // interval reconfigured; rebuild dormant/ticking outer loop
                        }
                    }
                }
            }
        }
    }
}

/// Runs one scheduled or manual check. Pushes when `checking` rises and when the result is written,
/// so the state is visible during the sync. Failures preserve `count`/`packages` (§2.14) and write
/// only `check_error`.
async fn run_one_check(
    backend: &Arc<dyn Backend>,
    state: &Arc<Mutex<UpdatesState>>,
    events: &UnboundedSender<UpdatesSignal>,
) {
    state.lock().unwrap().checking = true;
    let _ = events.send(UpdatesSignal::Changed);

    let backend = Arc::clone(backend);
    let result = tokio::task::spawn_blocking(move || backend.check()).await;

    let mut guard = state.lock().unwrap();
    guard.checking = false;
    match result {
        Ok(Ok(candidates)) => {
            guard.count = candidates.len() as u32;
            guard.packages = candidates;
            guard.last_successful_check = Some(now_unix());
            guard.check_error = None;
            guard.consecutive_check_failures = 0;
        }
        Ok(Err(err)) => {
            guard.check_error = Some(err);
            guard.consecutive_check_failures = guard.consecutive_check_failures.saturating_add(1);
        }
        Err(join_err) => {
            guard.check_error = Some(format!("check task panicked: {join_err}"));
            guard.consecutive_check_failures = guard.consecutive_check_failures.saturating_add(1);
        }
    }
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

/// Whether the interval's immediate first tick should check or be consumed. Due with no process
/// check, or when the last success is at least `interval` old.
fn first_check_is_due(last_successful_check: Option<i64>, now: i64, interval: Duration) -> bool {
    let Some(last) = last_successful_check else { return true };
    now.saturating_sub(last) >= interval.as_secs() as i64
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Assumes `state.installing` and its progress fields were set by `UpdatesController::install`'s
/// atomic check-and-set. Runs `Backend::install_command` against the live system as root, reads
/// stdout line by line, parses progress into `state`, and never exposes raw output to Lua
/// (ADR-0034).
async fn run_install(
    backend: Arc<dyn Backend>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
) {
    let command = backend.install_command();
    let child = match process::spawn_group_leader_piped(&command.program, &command.arguments, &[]) {
        Ok(child) => child,
        Err(err) => {
            let mut guard = state.lock().unwrap();
            guard.installing = false;
            guard.install_error = Some(format!("failed to spawn {}: {err}", command.program));
            drop(guard);
            let _ = events.send(UpdatesSignal::Changed);
            return;
        }
    };
    run_install_with_child(backend, state, events, child).await;
}

/// Testable stdout loop for [`run_install`]. Sends `UpdatesSignal::Changed` on every parsed line
/// (ADR-0034), not only at completion.
async fn run_install_with_child(
    backend: Arc<dyn Backend>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
    mut child: tokio::process::Child,
) {
    // Drain stderr concurrently: ~64KiB of warnings can fill the kernel pipe, block the
    // single-threaded manager, and leave `installing` stuck at `true`. Await the drain after exit;
    // detached reading can lose the final failure lines.
    let stderr_drain = child.stderr.take().map(|stderr| {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("updates: install stderr: {line}");
                push_log_line(&mut state.lock().unwrap().install_log, line);
            }
        })
    });

    // Local `Vec`: only this loop accesses it, unlike shared `state`.
    let mut installed_packages: Vec<String> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let step = backend.parse_install_step(&line);
            let mut guard = state.lock().unwrap();
            push_log_line(&mut guard.install_log, line);
            // Push on progress lines, not every download-meter line. `Changed` re-resolves every
            // surface (ADR-0044 decision 2); all lines still enter `install_log`.
            let Some(step) = step else { continue };
            guard.install_current_step = step.current;
            guard.install_total_steps = step.total;
            guard.install_current_package = step.package.clone();
            drop(guard);
            installed_packages.push(step.package);
            let _ = events.send(UpdatesSignal::Changed);
        }
    }

    let status = child.wait().await;
    // Await after process exit; only then does the pipe close and the drain finish.
    if let Some(drain) = stderr_drain {
        let _ = drain.await;
    }
    let mut guard = state.lock().unwrap();
    guard.installing = false;
    guard.install_finished_at = Some(now_unix());
    match status {
        Ok(status) => {
            // `None` means the process was killed by a signal.
            guard.install_exit_code = status.code();
            if status.success() {
                // Accumulate: a prior kernel install must not be cleared by an unrelated install.
                guard.reboot_required |= backend.needs_reboot(&installed_packages);
            }
        }
        Err(err) => guard.install_error = Some(format!("failed to wait on the install command: {err}")),
    }
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

/// Appends to the install-log tail, dropping its oldest line at capacity. Keep a `Vec`, not a
/// `VecDeque`: the state serializes as a JSON array, and shifting 200 pointers is not the cost
/// here.
fn push_log_line(log: &mut Vec<String>, line: String) {
    if log.len() >= LOG_TAIL_LINES {
        log.remove(0);
    }
    log.push(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::updates::backend::{InstallCommand, InstallStep};

    /// A backend whose check always fails without touching the network, and whose install-side
    /// answers are `pacman`'s real ones. What is under test around it is the scheduler and the
    /// stdout loop; the parsing has its own tests next to the parser.
    struct StubBackend;

    impl Backend for StubBackend {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn check(&self) -> Result<Vec<UpdateCandidate>, String> {
            Err("this stub cannot check anything".to_string())
        }

        fn install_command(&self) -> InstallCommand {
            InstallCommand { program: "true".to_string(), arguments: Vec::new() }
        }

        fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
            crate::capabilities::updates::pacman::install::parse_install_step(line)
        }

        fn needs_reboot(&self, package_names: &[String]) -> bool {
            crate::capabilities::updates::pacman::install::needs_reboot(package_names)
        }
    }

    #[test]
    fn parse_configure_args_reads_the_interval_from_a_table() {
        let args = vec![serde_json::json!({"interval": 3600})];
        assert_eq!(parse_configure_args(&args), Some(UpdatesConfigure { interval_secs: 3600, checked_at: None }));
    }

    #[test]
    fn parse_configure_args_reads_a_remembered_check_time_beside_the_interval() {
        let args = vec![serde_json::json!({"interval": 3600, "checked_at": 1_800_000_000_i64})];
        assert_eq!(
            parse_configure_args(&args),
            Some(UpdatesConfigure { interval_secs: 3600, checked_at: Some(1_800_000_000) })
        );
    }

    #[test]
    fn parse_configure_args_is_none_for_a_missing_or_wrong_typed_argument() {
        assert_eq!(parse_configure_args(&[]), None);
        assert_eq!(parse_configure_args(&[serde_json::json!(3600)]), None);
        assert_eq!(parse_configure_args(&[serde_json::json!({"wrong_key": 3600})]), None);
        assert_eq!(
            parse_configure_args(&[serde_json::json!({"interval": 3600, "checked_at": "yesterday"})]),
            None,
            "a present key with the wrong type drops the call rather than half-applying it"
        );
    }

    #[tokio::test]
    async fn a_remembered_check_time_seeds_an_empty_slot_and_never_overwrites_a_real_one() {
        let (controller, mut events_rx) = failing_controller().await;

        controller.configure(UpdatesConfigure { interval_secs: 0, checked_at: Some(1_800_000_000) });
        assert_eq!(controller.snapshot().last_successful_check, Some(1_800_000_000));
        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed), "a seed is Lua-visible, so it pushes");

        controller.configure(UpdatesConfigure { interval_secs: 0, checked_at: Some(1_700_000_000) });
        assert_eq!(
            controller.snapshot().last_successful_check,
            Some(1_800_000_000),
            "this must never move the last-check time backwards"
        );
    }

    #[test]
    fn poll_mode_is_dormant_at_zero_and_ticking_otherwise() {
        assert_eq!(poll_mode(Duration::ZERO), PollMode::Dormant);
        assert_eq!(poll_mode(Duration::from_secs(1)), PollMode::Ticking(Duration::from_secs(1)));
    }

    /// A controller over [`StubBackend`], so every check fails without touching the network. What
    /// is under test is the scheduler around the check, not any real package manager: a real sync
    /// needs a real mirror and is verified live (see `pacman/check.rs`).
    ///
    /// The construction push is consumed here, so each test's own assertions start from the first
    /// signal it actually caused.
    async fn failing_controller() -> (UpdatesController, tokio::sync::mpsc::UnboundedReceiver<UpdatesSignal>) {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = UpdatesController::with_backend(Some(Arc::new(StubBackend)), events_tx);
        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed), "construction pushes the backend's name");
        (controller, events_rx)
    }

    /// The two pushes one check makes: `checking` up, then the answer.
    async fn await_one_check(events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<UpdatesSignal>) {
        for _ in 0..2 {
            events_rx.recv().await.expect("the check task must push at both edges of a check");
        }
    }

    #[tokio::test]
    async fn a_manual_check_runs_with_no_schedule_configured_at_all() {
        // Dormant mode answers `check_now`; a config may use a button without a timer.
        let (controller, mut events_rx) = failing_controller().await;

        controller.check_now();
        await_one_check(&mut events_rx).await;

        let snapshot = controller.snapshot();
        assert!(!snapshot.checking, "checking must fall again once the answer is written");
        assert!(snapshot.check_error.is_some(), "a db root with no local/ cannot be checked");
        assert_eq!(snapshot.consecutive_check_failures, 1);
        assert_eq!(snapshot.last_successful_check, None);
    }

    #[tokio::test]
    async fn failed_checks_count_up_and_leave_the_last_good_answer_alone() {
        let (controller, mut events_rx) = failing_controller().await;
        // A count from an earlier good check, which a failure must not blank (§ 2.14).
        controller.state.lock().unwrap().count = 3;

        controller.check_now();
        await_one_check(&mut events_rx).await;
        controller.check_now();
        await_one_check(&mut events_rx).await;

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.consecutive_check_failures, 2);
        assert_eq!(snapshot.count, 3, "a failed check reports the failure, it does not clear the list");
    }

    #[test]
    fn the_first_check_of_a_process_is_due_immediately() {
        // The boot case, and the whole point of the change: a config asking for an hourly check
        // wants to know what is pending now, not at the end of the first hour.
        assert!(first_check_is_due(None, 1_800_000_000, Duration::from_secs(3600)));
    }

    #[test]
    fn a_reconfigure_within_the_interval_waits_rather_than_syncing_again() {
        // A config reload re-invokes `configure`, and the controller outlives the generation that
        // did it. Without this, every save would be another sync against a mirror.
        let last = 1_800_000_000;
        assert!(!first_check_is_due(Some(last), last + 60, Duration::from_secs(3600)));
    }

    #[test]
    fn a_check_older_than_the_interval_is_due_again() {
        let last = 1_800_000_000;
        let interval = Duration::from_secs(3600);
        assert!(first_check_is_due(Some(last), last + 3600, interval), "exactly one interval old is due");
        assert!(first_check_is_due(Some(last), last + 7200, interval));
    }

    #[test]
    fn a_last_check_stamped_in_the_future_does_not_underflow_into_due() {
        // A clock stepped backwards (an NTP correction, a suspend across a timezone fix) leaves a
        // stamp ahead of `now`. Saturating, so that reads as "checked recently", not as a negative
        // age that compares below the interval by accident.
        let last = 1_800_000_000;
        assert!(!first_check_is_due(Some(last), last - 5000, Duration::from_secs(3600)));
    }

    #[tokio::test]
    async fn a_machine_with_no_package_manager_says_so_once_and_then_refuses_every_action() {
        // The whole point of the field: an indicator asks `package_manager` whether it belongs on
        // the bar, and on a machine with no manager nothing else would ever push to tell it.
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = UpdatesController::with_backend(None, events_tx);

        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed));
        assert_eq!(controller.snapshot().package_manager, None);

        controller.configure(UpdatesConfigure { interval_secs: 3600, checked_at: Some(1_800_000_000) });
        controller.check_now();
        controller.install().await;

        assert_eq!(
            controller.snapshot(),
            UpdatesState::default(),
            "no action may write state on a machine there is no manager to act with"
        );
        assert_eq!(events_rx.try_recv().ok(), None, "and none of them may push");
    }

    #[tokio::test]
    async fn a_detected_backend_names_itself_before_anything_has_been_checked() {
        let (controller, _events_rx) = failing_controller().await;

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.package_manager.as_deref(), Some("stub"));
        assert_eq!(snapshot.count, 0);
        assert_eq!(snapshot.last_successful_check, None, "naming the manager is not a check");
    }

    #[test]
    fn updates_state_default_has_no_updates_and_no_errors() {
        let state = UpdatesState::default();
        assert_eq!(state.package_manager, None);
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
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!(snapshot.install_current_step, 2);
        assert_eq!(snapshot.install_total_steps, 2);
        assert_eq!(snapshot.install_current_package, "gnome-autoar");
        assert_eq!(snapshot.install_error, None);

        // Progress must ride the updates signal per line, not just at the end (ADR-0034).
        let mut signal_count = 0;
        while events_rx.try_recv().is_ok() {
            signal_count += 1;
        }
        assert_eq!(signal_count, 3, "two progress-line signals plus one completion signal");
    }

    #[tokio::test]
    async fn run_install_with_child_reports_pacmans_own_exit_code_rather_than_a_sentence() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "exit 1".to_string()], &[])
            .expect("spawn a failing stub");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!(snapshot.install_exit_code, Some(1));
        assert!(snapshot.install_finished_at.is_some());
        assert_eq!(
            snapshot.install_error, None,
            "the package manager answering with a failure is not the Supervisor failing to ask"
        );
    }

    #[tokio::test]
    async fn the_install_log_keeps_both_streams_and_survives_a_line_that_is_not_progress() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo ':: Synchronizing package databases...'; echo 'error: target not found' 1>&2; echo '(1/1) upgrading nss'; exit 0".to_string()],
            &[],
        )
        .expect("spawn a chatty stub");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

        let log = state.lock().unwrap().install_log.clone();
        assert!(log.contains(&":: Synchronizing package databases...".to_string()), "{log:?}");
        assert!(log.contains(&"(1/1) upgrading nss".to_string()), "{log:?}");
        assert!(
            log.contains(&"error: target not found".to_string()),
            "stderr is where a package manager says why it failed, so it has to be in the tail by \
             the time the install is reported finished: {log:?}"
        );
    }

    #[test]
    fn the_install_log_drops_the_oldest_line_once_it_is_full() {
        let mut log: Vec<String> = Vec::new();
        for index in 0..(LOG_TAIL_LINES + 5) {
            push_log_line(&mut log, index.to_string());
        }

        assert_eq!(log.len(), LOG_TAIL_LINES);
        assert_eq!(log.first().map(String::as_str), Some("5"), "the oldest five are the ones gone");
        assert_eq!(log.last().map(String::as_str), Some((LOG_TAIL_LINES + 4).to_string().as_str()));
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
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

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
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, kernel_child).await;
        assert!(state.lock().unwrap().reboot_required, "first install touched the kernel");

        let unrelated_child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo '(1/1) upgrading nss'; exit 0".to_string()],
            &[],
        )
        .expect("spawn a stub install script");
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, unrelated_child).await;

        assert!(
            state.lock().unwrap().reboot_required,
            "a later install with no kernel package must not clear a still-pending reboot"
        );
    }
}
