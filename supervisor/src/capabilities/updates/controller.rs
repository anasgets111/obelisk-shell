//! [`UpdatesController`]: the `oblisk.updates` write-action dispatcher and state owner
//! (ADR-0034). Split from `updates` -- see `updates/mod.rs` for the module-level doc.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::backend::{Backend, UpdateCandidate};
use crate::process;

/// `oblisk.updates`'s combined payload. `check_error`/`install_error` are `None` when
/// nothing's gone wrong, not a fabricated empty string. `install_total_steps == 0` while
/// `installing` is true means the transaction size isn't known yet (the package manager hasn't
/// printed it).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct UpdatesState {
    /// Which package manager answered, or `nil` when this machine has none this Supervisor
    /// speaks -- the one field a config can read before anything has been checked, and the one
    /// that tells an indicator whether it has any business being on the bar at all (ADR-0134).
    /// The name of the command: `"pacman"`.
    pub package_manager: Option<String>,
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
    /// Why the last check failed, or `nil` when the last one worked. A check never modifies the
    /// system (`Backend::check` promises that much), so this is a network or parse failure, never a
    /// half-applied change to the system.
    pub check_error: Option<String>,
    /// A check is running right now. Rises before the sync starts and falls when the result is
    /// written, with a push at both edges, so a config can draw a spinner and disable its own
    /// refresh control. `updates:check` refuses a second one while this is true.
    pub checking: bool,
    /// How many checks in a row have failed, reset to `0` by the first success. The count only:
    /// "warn after five" is a threshold somebody has an opinion about, so it lives in the config.
    pub consecutive_check_failures: u32,
    /// An install is running. The `install_*` fields above only describe a run that has started;
    /// `updates:install` refuses a second one while this is true.
    pub installing: bool,
    /// Which package of the transaction the package manager is on, its own 1-based `(2/5)`
    /// counter. `0` before the first line is parsed.
    pub install_current_step: u32,
    /// How many packages the transaction has. `0` while [`UpdatesState::installing`] is true means
    /// the package manager has not printed a step line yet, so a progress bar has no denominator:
    /// show it as indeterminate rather than dividing.
    pub install_total_steps: u32,
    /// The package name from the step line the package manager is on. Empty string before the
    /// first one, not `nil`, because a name is always a string once the transaction is under way.
    pub install_current_package: String,
    /// What the package manager itself answered on the last install: `0` for success, its own code
    /// for a failure, `nil` if none has finished this session. The code and
    /// [`UpdatesState::install_log`] are the two facts about a failure; what to *call* it -- a
    /// network error, a disk-space error, a signature error -- is wording, and wording belongs in
    /// the config (ADR-0113 amendment).
    pub install_exit_code: Option<i32>,
    /// Unix seconds when the last install stopped, however it stopped. With an install's start held
    /// by whatever asked for it, this is what a duration is measured against.
    pub install_finished_at: Option<i64>,
    /// The tail of the last install's output, newest last, both streams interleaved in arrival
    /// order (they are read by two tasks, so the interleaving between them is not exact). Capped at
    /// the last 200: a long upgrade writes thousands of lines and this is a payload pushed over a
    /// socket, not a file. Cleared when an install starts.
    pub install_log: Vec<String>,
    /// Why the Supervisor never got an answer from the package manager at all -- it could not spawn
    /// the install command, or could not wait on it. Distinct from
    /// [`UpdatesState::install_exit_code`], which is the answer: this one means the question was
    /// never asked, and it is the Supervisor's own failure rather than the package manager's.
    pub install_error: Option<String>,
    /// A kernel package was installed at some point this session, per `Backend::needs_reboot`.
    /// Sticky on purpose:
    /// once set it stays set through later installs that do not touch the kernel, because the
    /// running kernel is still the old one until the machine restarts.
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
    /// When the config remembers the last successful check happening, from wherever it keeps
    /// that -- `system.state`, most likely. Optional, and a seed rather than an override: it is
    /// taken only while this process has no check of its own, which is exactly the boot where the
    /// question "has an hour passed?" would otherwise have no answer but "start over".
    pub checked_at: Option<i64>,
}

/// `updates:configure({interval})`'s `arguments: [{...}]` -- a table argument (ADR-0034). A present
/// key with the wrong type drops the whole call rather than half-applying it.
pub fn parse_configure_args(arguments: &[serde_json::Value]) -> Option<UpdatesConfigure> {
    let table = arguments.first()?.as_object()?;
    let interval_secs = table.get("interval")?.as_u64()?;
    let checked_at = match table.get("checked_at") {
        Some(value) => Some(value.as_i64()?),
        None => None,
    };
    Some(UpdatesConfigure { interval_secs, checked_at })
}

/// How many lines of [`UpdatesState::install_log`] survive. Enough to hold a failure and the lines
/// around it -- a config wanting the whole run of a 2,000-package upgrade wants a file, not a state
/// payload that is re-serialized and pushed on every progress line.
const LOG_TAIL_LINES: usize = 200;

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
    /// The package manager this machine has. `None` is a real, supported state: every action
    /// below then does nothing but say so, rather than failing a check to report it.
    backend: Option<Arc<dyn Backend>>,
    state: Arc<Mutex<UpdatesState>>,
    interval_tx: watch::Sender<Duration>,
    /// `updates:check`'s nudge to the scheduler. Capacity one and `try_send`, so a burst of
    /// requests collapses into the single check they were all asking for.
    check_now_tx: tokio::sync::mpsc::Sender<()>,
    events: UnboundedSender<UpdatesSignal>,
}

impl UpdatesController {
    /// Detects this machine's package manager (`backend::detect`) and starts the scheduler
    /// around it. Starts dormant (`Duration::ZERO`) -- nothing checks for updates until Lua calls
    /// `updates:configure` at least once.
    ///
    /// Pushes once, immediately, which no other capability's constructor does: `package_manager`
    /// is the answer to "should this indicator exist", and on a machine with no manager at all
    /// there is no later event to carry it -- the scheduler would sit dormant forever and a config
    /// would never learn why. On this machine it is also the first hour's difference between an
    /// indicator that appears at login and one that appears whenever the first check lands.
    pub fn new(events: UnboundedSender<UpdatesSignal>) -> Self {
        Self::with_backend(super::backend::detect().map(Arc::from), events)
    }

    /// [`UpdatesController::new`] against a backend chosen by the caller rather than detected,
    /// which is how the tests drive the scheduler without a real package manager underneath it.
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

    /// Sets the schedule, and optionally seeds the last-check time the config remembered across a
    /// restart (ADR-0113 amendment). The seed is taken only while this process has none of its own:
    /// a check this session actually ran is fresher than anything a config can tell it, and this
    /// must never move `last_successful_check` backwards.
    ///
    /// Seeding pushes, because the field is Lua-visible: a config that persisted the time and then
    /// read `last_successful_check` back as `nil` for the next hour would be told its own answer is
    /// unknown.
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

    /// `updates:check()`. Runs one check now, whatever the schedule says -- including when there is
    /// no schedule at all, since a config may want the button and never the timer.
    ///
    /// Refused while a check is already running, the way [`UpdatesController::install`] refuses a
    /// second transaction: the answer in flight is the answer being asked for, and a click during a
    /// sync should not queue a second sync behind it.
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

    /// `updates:install()`. A no-op (logged) if an install is already running, or if this machine
    /// has no package manager at all -- no manager worth the name supports two concurrent
    /// transactions against the same database lock. The check-and-set is one atomic critical section
    /// under a single lock acquisition: two `install()` calls dispatched close together could
    /// otherwise both observe `installing == false` and both launch a real upgrade against the same
    /// database.
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

/// Runs until every `UpdatesController` (and its `Clone`s) drops. Spawned only when a backend was
/// detected -- on a machine with no package manager there is no schedule to keep. Every check runs
/// inside `tokio::task::spawn_blocking`, never awaited inline: `Backend::check` is blocking network
/// I/O by contract, and `pacman`'s `alpm` types are not even `Send`.
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
                // `check_now` is answered here too, not only under a schedule: a config that never
                // names an interval and only ever checks on a click is a shape this should allow.
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
                // A check can genuinely run longer than a short configured interval (real
                // network I/O); the default `Burst` behavior would then fire every missed tick
                // back-to-back, hammering the mirrors -- `Delay` resumes ticking after the check finishes.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // `tokio::time::interval`'s first tick fires immediately, and here that is the
                // point: a config that says "every hour" wants to know what is pending now, not in
                // an hour (ADR-0113 amendment). It is consumed only when a check inside this
                // process is still fresh, which is what makes a config reload cheap -- the
                // controller outlives the generation that configured it, so every save would
                // otherwise be another mirror sync, and under the old unconditional consume every
                // save reset the hour and a day of editing never checked at all.
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
                            break; // interval reconfigured -- rebuild dormant/ticking in the outer loop
                        }
                    }
                }
            }
        }
    }
}

/// One check, from wherever it was asked for: the schedule's tick, or `updates:check`. Raises
/// `checking` with a push before the sync so a config can say so, and lowers it with another once
/// the answer is written -- two pushes, because "checking" that is only visible after the fact is
/// not visible at all.
///
/// A failed check leaves `count`/`packages` on the last good answer (§ 2.14) and only writes
/// `check_error`, so a mirror hiccup does not blank a list the user is reading.
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

/// Whether the tick `tokio::time::interval` fires the instant it is built should be spent on a
/// real check, or consumed. Due when nothing has checked yet in this process, or when the last
/// success is at least `interval` old -- the same question the ticker would ask a moment later,
/// asked once up front so a fresh process answers "now" and a reconfigured one does not.
fn first_check_is_due(last_successful_check: Option<i64>, now: i64, interval: Duration) -> bool {
    let Some(last) = last_successful_check else { return true };
    now.saturating_sub(last) >= interval.as_secs() as i64
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The real, system-modifying install: whatever `Backend::install_command` names, run for real
/// against the live system as root. Streams stdout line by line, reading progress through
/// `Backend::parse_install_step` and writing it into `state` as it goes -- Lua never sees raw
/// subprocess output (ADR-0034). Assumes `state.installing` and its progress fields are already
/// set by [`UpdatesController::install`]'s atomic check-and-set.
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

/// Split from [`run_install`] so the stdout-driven progress loop can be tested against a stub
/// child process, without a real privileged upgrade. Sends `UpdatesSignal::Changed` on every
/// parsed progress line (ADR-0034), not just at the end.
async fn run_install_with_child(
    backend: Arc<dyn Backend>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
    mut child: tokio::process::Child,
) {
    // Drained concurrently on its own task, not left unread: a real upgrade can write enough stderr
    // warnings to fill the pipe's ~64KiB kernel buffer, which blocks the package manager's
    // single-threaded process and wedges `installing` at `true` forever. Logged, not discarded.
    if let Some(stderr) = child.stderr.take() {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("updates: install stderr: {line}");
                push_log_line(&mut state.lock().unwrap().install_log, line);
            }
        });
    }

    // Plain local `Vec`, not `Arc<Mutex<_>>`: every read/write happens sequentially within
    // this loop, never shared with another task -- unlike `state`.
    let mut installed_packages: Vec<String> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let step = backend.parse_install_step(&line);
            let mut guard = state.lock().unwrap();
            push_log_line(&mut guard.install_log, line);
            // The push rides the progress lines rather than every line. One `Changed` re-resolves
            // every surface in the generation (ADR-0044 decision 2), and a package manager writes a
            // download meter; the lines are in `install_log` either way, they just arrive on
            // screen with the next step rather than on their own frame.
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
    let mut guard = state.lock().unwrap();
    guard.installing = false;
    guard.install_finished_at = Some(now_unix());
    match status {
        Ok(status) => {
            // `None` only for a process killed by a signal, which has no exit code to report.
            guard.install_exit_code = status.code();
            if status.success() {
                // Accumulates (OR), never overwrites: a reboot owed from an earlier install must
                // not be cleared just because this install didn't touch the kernel.
                guard.reboot_required |= backend.needs_reboot(&installed_packages);
            }
        }
        Err(err) => guard.install_error = Some(format!("failed to wait on the install command: {err}")),
    }
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

/// Appends one line to an install log, dropping the oldest once the tail is full. A `Vec` and a
/// `remove(0)` rather than a `VecDeque`: this is serialized as a JSON array on every push, so it
/// has to be one anyway, and [`LOG_TAIL_LINES`] shifts of a pointer-sized element are not the cost
/// in a function that just parsed a line of subprocess output.
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
        // The dormant arm answers `check_now` too: a config may want the button and never the timer.
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

        // Correctness: progress must ride the updates signal per-line, not just at the end (ADR-0034).
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
            "stderr is where pacman says why it failed, so it has to be in the tail too: {log:?}"
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
