//! [`ProcessesController`] owns the programs a config declared with `session_process`.
//!
//! The difference from `process.run` is lifetime, and it is the whole feature. A `process.run`
//! child belongs to the generation that spawned it, and `reap_generations_processes` kills its
//! group on every swap; a config wanting a program to survive an edit has to orphan it with
//! `setsid` and then re-find it through a lock file and `/proc`, because by then nothing in the
//! shell still holds it. That is what the mirror's `ScreenRecordingService.qml` spends 223 lines
//! on: a pid, the pid's kernel start time, and a two-second poll, all to answer a question the
//! kernel would answer for free to whoever held the handle.
//!
//! Here the Supervisor holds it. The Supervisor does not restart on a config edit, so a swap is
//! not an event a session process can observe, and the identity question never arises.
//!
//! ## One task per running program, and why signalling goes through it
//!
//! [`supervise`] owns the `Child` and selects between its exit and a request channel. Every signal
//! is therefore sent from the task that has not yet reaped the process, so the pid it names is
//! still reserved by the kernel and cannot have been recycled under it. Sending from the
//! controller instead -- by keeping a pid in the map -- would reopen exactly the window the pid
//! plus start-time pair exists to cover elsewhere. `tokio::process::Child::wait` is cancel-safe,
//! which is what lets the `select!` re-arm it after each request.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Child;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// How long a stopping program has after its declared stop signal before `SIGKILL`.
///
/// Deliberately not `process::DEFAULT_REAP_GRACE`. That 100ms suits a `process.run` helper that
/// has nothing to finish; a session process is declared precisely because it is doing something
/// long, and the first thing that will use this writes a video container whose index is appended
/// on the way out. Killing it at 100ms would leave the file unplayable, which is the failure the
/// declared stop signal exists to avoid.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// `obelisk.processes`'s payload.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ProcessesState {
    /// One entry per name a config declared with `session_process`, keyed by that name. A name
    /// nothing declared is absent rather than stopped, so a typo reads `nil` instead of quietly
    /// looking like a program that never starts.
    pub sessions: BTreeMap<String, SessionProcess>,
}

/// One declared program: its current run, or what is left of its last one.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SessionProcess {
    /// Whether it is up now. Every field below describes the current run while this is true, and
    /// the finished one while it is false.
    pub running: bool,
    /// Its process id, which is also its process group. `nil` until the first `start`, and kept
    /// after an exit so a log line can still name what died.
    pub pid: Option<u32>,
    /// Unix seconds when the current or last run began; `nil` until the first `start`. Elapsed
    /// time is this subtracted from `obelisk.system`'s clock, so nothing here needs a second timer.
    pub started_at: Option<u64>,
    /// How the last finished run ended: its exit status, `nil` while running, before the first
    /// run, or when a signal ended it rather than an exit. Cleared by the next `start`.
    pub exit_code: Option<i32>,
    /// Why the last `start` produced no process at all -- a command that is not on `PATH`, most
    /// often. Empty when it spawned, and cleared by the next `start`. Without this a config
    /// waiting on `running` would wait forever with the reason only in the Supervisor's stderr.
    pub start_error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessesSignal {
    Changed,
}

/// What [`supervise`] accepts while its program is up.
enum Request {
    /// One signal to the process itself, not its group: a pause or a reload is addressed to the
    /// program a config named, not to helpers it happened to spawn.
    Signal(Signal),
    /// Stop the group with the signal the declaration names when this is handled, escalating to
    /// `SIGKILL` after [`STOP_GRACE`].
    Stop,
}

/// A declared name. `live` exists only while its program is up.
struct Entry {
    stop_signal: Signal,
    live: Option<Live>,
    public: SessionProcess,
}

/// The running half: the channel [`supervise`] selects on and its handle for the shutdown reap to
/// await.
struct Live {
    requests: UnboundedSender<Request>,
    task: JoinHandle<()>,
}

type Entries = Arc<Mutex<HashMap<String, Entry>>>;

pub struct ProcessesController {
    entries: Entries,
    signal_tx: UnboundedSender<ProcessesSignal>,
}

impl ProcessesController {
    pub fn new(signal_tx: UnboundedSender<ProcessesSignal>) -> Self {
        Self { entries: Arc::new(Mutex::new(HashMap::new())), signal_tx }
    }

    pub fn snapshot(&self) -> ProcessesState {
        let guard = self.entries.lock().expect("processes entries mutex poisoned");
        ProcessesState { sessions: guard.iter().map(|(name, entry)| (name.clone(), entry.public.clone())).collect() }
    }

    /// `session_process { name, stop_signal }`, re-sent every evaluation the way `storage:open`
    /// is, so an edited stop signal lands on reload without disturbing a program already up.
    ///
    /// Declaring is what makes a name exist; `start` on an undeclared one is refused rather than
    /// creating the entry, so a config cannot run a program it never named.
    pub fn declare(&self, name: &str, stop_signal: Signal) {
        let mut guard = self.entries.lock().expect("processes entries mutex poisoned");
        match guard.get_mut(name) {
            Some(entry) => entry.stop_signal = stop_signal,
            None => {
                guard.insert(name.to_string(), Entry { stop_signal, live: None, public: SessionProcess::default() });
            }
        }
        drop(guard);
        let _ = self.signal_tx.send(ProcessesSignal::Changed);
    }

    /// Spawns `cmd`, replacing this name's last run. A name already up is left alone: `start`
    /// twice is the same program, not two of it, and the config's own `running` says which.
    ///
    /// stdio is inherited rather than piped. A session process outlives the generation that
    /// started it, so there is no callback for its output to reach -- the shell's own log is the
    /// honest destination, and a config that wants a program's output wants `process.run`.
    pub fn start(&self, name: &str, cmd: &str, args: &[String]) {
        let mut guard = self.entries.lock().expect("processes entries mutex poisoned");
        let Some(entry) = guard.get_mut(name) else {
            eprintln!("processes: refused to start {name:?}; no session_process declared that name");
            return;
        };
        if entry.public.running {
            eprintln!("processes: {name:?} is already running as pid {:?}; ignoring start", entry.public.pid);
            return;
        }

        entry.public = SessionProcess::default();
        match crate::process::spawn_group_leader(cmd, args, &[]) {
            Ok(child) => {
                let pid = child.id();
                entry.public.running = true;
                entry.public.pid = pid;
                entry.public.started_at = Some(unix_seconds());
                let (requests_tx, requests_rx) = unbounded_channel();
                let task = tokio::spawn(supervise(
                    name.to_string(),
                    child,
                    requests_rx,
                    Arc::clone(&self.entries),
                    self.signal_tx.clone(),
                ));
                entry.live = Some(Live { requests: requests_tx, task });
            }
            Err(err) => {
                // The config is waiting on `running`; a reason it cannot read is a hang.
                entry.public.start_error = format!("{cmd}: {err}");
                eprintln!("processes: {name:?} failed to start {cmd:?}: {err}");
            }
        }
        drop(guard);
        let _ = self.signal_tx.send(ProcessesSignal::Changed);
    }

    /// Sends one signal to a running program. Silent when it is not up: a config acting on state
    /// one push old is ordinary, not an error worth a log line per click.
    pub fn signal(&self, name: &str, signal: Signal) {
        self.request(name, Request::Signal(signal));
    }

    /// Asks a running program to stop with the signal its declaration named.
    pub fn stop(&self, name: &str) {
        self.request(name, Request::Stop);
    }

    fn request(&self, name: &str, request: Request) {
        let guard = self.entries.lock().expect("processes entries mutex poisoned");
        if let Some(live) = guard.get(name).and_then(|entry| entry.live.as_ref()) {
            let _ = live.requests.send(request);
        }
    }

    /// Stops every running program and waits for it, the counterpart to `reap_all_processes`.
    ///
    /// Awaited rather than fired and forgotten: these are the processes whose exit path matters,
    /// which is why they were declared with a stop signal in the first place. [`supervise`] bounds
    /// its own wait at `2 * STOP_GRACE`, so this cannot hold shutdown open indefinitely.
    pub async fn reap_all(&self) {
        let live: Vec<(String, Live)> = {
            let mut guard = self.entries.lock().expect("processes entries mutex poisoned");
            guard.iter_mut().filter_map(|(name, entry)| entry.live.take().map(|live| (name.clone(), live))).collect()
        };
        for (name, live) in live {
            let _ = live.requests.send(Request::Stop);
            // Dropping `live.requests` also reads as a stop to `supervise`; the explicit request
            // above covers the ordinary case where it is still selecting.
            drop(live.requests);
            if let Err(err) = live.task.await {
                eprintln!("processes: {name:?}'s supervising task did not finish cleanly on shutdown: {err}");
            }
        }
    }
}

/// Owns one running program until it exits, and is the only place its pid is signalled.
async fn supervise(
    name: String,
    mut child: Child,
    mut requests: UnboundedReceiver<Request>,
    entries: Entries,
    signal_tx: UnboundedSender<ProcessesSignal>,
) {
    let Some(raw_pid) = child.id() else {
        eprintln!("processes: {name:?} spawned without a pid; nothing to supervise");
        return;
    };
    let pid = Pid::from_raw(raw_pid as i32);

    let status = loop {
        tokio::select! {
            // Cancel-safe, so losing this race to a request leaves the wait intact.
            status = child.wait() => break status,
            request = requests.recv() => match request {
                Some(Request::Signal(signal)) => {
                    // The process, not the group: a pause belongs to the program that was named.
                    if let Err(err) = kill_best_effort(pid, signal) {
                        eprintln!("processes: {name:?} could not be sent {signal}: {err}");
                    }
                }
                // `None` means the controller is gone and nothing can ask again; leaving the
                // program orphaned would be worse than stopping it.
                Some(Request::Stop) | None => {
                    break stop_group(&name, &mut child, pid, declared_stop_signal(&entries, &name)).await;
                }
            },
        }
    };

    let exit_code = match status {
        Ok(status) => status.code(),
        Err(err) => {
            eprintln!("processes: {name:?} could not be waited on: {err}");
            None
        }
    };

    {
        let mut guard = entries.lock().expect("processes entries mutex poisoned");
        if let Some(entry) = guard.get_mut(&name) {
            entry.public.running = false;
            entry.public.exit_code = exit_code;
            entry.live = None;
        }
    }
    let _ = signal_tx.send(ProcessesSignal::Changed);
}

/// The declared stop signal to the group, [`STOP_GRACE`], then `SIGKILL` to the group.
///
/// Signals the group rather than the process so a program that spawned helpers takes them with it,
/// matching `process::reap_process_group`. That primitive is not reused because it hardcodes
/// `SIGTERM` and its 100ms, and the declared signal is the entire reason this path exists.
async fn stop_group(
    name: &str,
    child: &mut Child,
    pgid: Pid,
    stop_signal: Signal,
) -> io::Result<std::process::ExitStatus> {
    if let Err(err) = crate::process::signal_group_best_effort(pgid, stop_signal) {
        eprintln!("processes: {name:?} could not be sent {stop_signal}: {err}");
    }
    if let Ok(status) = tokio::time::timeout(STOP_GRACE, child.wait()).await {
        return status;
    }
    eprintln!("processes: {name:?} ignored {stop_signal} for {STOP_GRACE:?}; escalating to SIGKILL");
    if let Err(err) = crate::process::signal_group_best_effort(pgid, Signal::SIGKILL) {
        eprintln!("processes: {name:?} could not be sent SIGKILL: {err}");
    }
    // Bounded like the SIGTERM wait: uninterruptible I/O can defer even SIGKILL.
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(status) => status,
        Err(_elapsed) => Err(io::Error::other("still running after SIGKILL")),
    }
}

/// The signal `name`'s declaration names *now*, read here rather than captured when [`supervise`]
/// started: a reload redeclares under a program already up, and `start` is long past. Names are
/// only ever added to the map, so the fallback is unreachable and `SIGTERM` merely has to stop a
/// program nothing can describe any more.
fn declared_stop_signal(entries: &Entries, name: &str) -> Signal {
    let guard = entries.lock().expect("processes entries mutex poisoned");
    guard.get(name).map_or(Signal::SIGTERM, |entry| entry.stop_signal)
}

/// `ESRCH` is success here as in `process::signal_group_best_effort`: the target may die between
/// the check and the signal.
fn kill_best_effort(pid: Pid, signal: Signal) -> io::Result<()> {
    match kill(pid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Wall clock, matching `obelisk.system`'s so a config can subtract the two.
fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|since| since.as_secs()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    /// A controller whose signal channel is drained on demand; tests read `snapshot` instead.
    fn controller() -> (ProcessesController, UnboundedReceiver<ProcessesSignal>) {
        let (tx, rx) = unbounded_channel();
        (ProcessesController::new(tx), rx)
    }

    fn session(controller: &ProcessesController, name: &str) -> SessionProcess {
        controller.snapshot().sessions.get(name).cloned().unwrap_or_default()
    }

    /// Polls `snapshot` until `predicate` holds. A real process is being waited on, so the exit
    /// lands whenever the kernel says so; a fixed sleep would either flake or be slow.
    async fn until(controller: &ProcessesController, name: &str, predicate: impl Fn(&SessionProcess) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if predicate(&session(controller, name)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting on {name:?}; last state was {:?}", session(controller, name));
    }

    fn shell(script: &str) -> (String, Vec<String>) {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }

    /// A shell that installs `trap`, then touches `marker`, then idles.
    ///
    /// The marker is the point of this: `start` sets `running` synchronously, before the child has
    /// reached its own first instruction, so a test that signals as soon as `running` is true
    /// races the `exec` and kills a process whose trap does not exist yet. That race produced a
    /// signalled death with no status, which is exactly what these tests assert did not happen.
    fn trapping_shell(trap: &str, marker: &Path) -> (String, Vec<String>) {
        shell(&format!("{trap}; : > {}; while :; do sleep 0.05; done", marker.display()))
    }

    async fn until_ready(marker: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if marker.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the child never reported its trap installed at {}", marker.display());
    }

    #[tokio::test]
    async fn a_declared_program_runs_and_its_exit_status_comes_back() {
        let (controller, _rx) = controller();
        controller.declare("t", Signal::SIGTERM);
        assert!(!session(&controller, "t").running, "declaring names a program; it does not run one");

        let (cmd, args) = shell("exit 3");
        controller.start("t", &cmd, &args);
        assert!(session(&controller, "t").pid.is_some(), "a spawned program has its pid immediately");

        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(3));
    }

    #[tokio::test]
    async fn a_signal_reaches_the_program_itself() {
        let (controller, _rx) = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGTERM);
        let (cmd, args) = trapping_shell("trap 'exit 42' USR2", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        controller.signal("t", Signal::SIGUSR2);
        until(&controller, "t", |s| !s.running).await;
        // 42 is the trap's own status, so this could not have come from the default disposition.
        assert_eq!(session(&controller, "t").exit_code, Some(42));
    }

    #[tokio::test]
    async fn stop_sends_the_signal_the_declaration_named_rather_than_sigterm() {
        let (controller, _rx) = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGINT);
        // Exits 9 only on INT. TERM would end it with no status at all, which is the bug this
        // guards: a recorder reaped with the wrong signal leaves its container unfinished.
        let (cmd, args) = trapping_shell("trap 'exit 9' INT; trap '' TERM", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        controller.stop("t");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(9));
    }

    #[tokio::test]
    async fn a_redeclared_stop_signal_reaches_a_program_that_was_already_running() {
        let (controller, _rx) = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGTERM);
        // Two statuses so the signal that arrived is readable: 11 is the one `supervise` used to
        // capture at `start` and keep.
        let (cmd, args) = trapping_shell("trap 'exit 11' TERM; trap 'exit 22' INT", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        // A reload re-sends every declaration. This one changed and the program did not restart.
        controller.declare("t", Signal::SIGINT);
        controller.stop("t");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(22), "stop must use the declaration as it stands now");
    }

    #[tokio::test]
    async fn the_shutdown_reap_also_uses_the_current_declaration() {
        let (controller, _rx) = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGTERM);
        let (cmd, args) = trapping_shell("trap 'exit 11' TERM; trap 'exit 22' INT", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        controller.declare("t", Signal::SIGINT);
        controller.reap_all().await;
        assert_eq!(session(&controller, "t").exit_code, Some(22), "shutdown is the other path the stale signal took");
    }

    #[tokio::test]
    async fn starting_a_name_no_declaration_claimed_is_refused_rather_than_creating_it() {
        let (controller, _rx) = controller();
        let (cmd, args) = shell("exit 0");
        controller.start("never-declared", &cmd, &args);
        assert!(
            controller.snapshot().sessions.is_empty(),
            "an undeclared name must stay absent, so a typo reads nil instead of a program that never starts"
        );
    }

    #[tokio::test]
    async fn a_second_start_leaves_the_running_program_alone() {
        let (controller, _rx) = controller();
        controller.declare("t", Signal::SIGTERM);
        let (cmd, args) = shell("while :; do sleep 0.05; done");
        controller.start("t", &cmd, &args);
        until(&controller, "t", |s| s.running).await;
        let first = session(&controller, "t").pid;

        controller.start("t", &cmd, &args);
        assert_eq!(session(&controller, "t").pid, first, "start twice is the same program, not two of it");

        controller.reap_all().await;
    }

    #[tokio::test]
    async fn a_command_that_is_not_there_says_why_instead_of_looking_like_a_slow_start() {
        let (controller, _rx) = controller();
        controller.declare("t", Signal::SIGTERM);
        controller.start("t", "obelisk-no-such-binary", &[]);

        let state = session(&controller, "t");
        assert!(!state.running);
        assert!(
            state.start_error.contains("obelisk-no-such-binary"),
            "the reason has to name the command; a config watching `running` would otherwise wait forever: {state:?}"
        );
    }

    #[tokio::test]
    async fn a_start_clears_what_the_last_run_left_behind() {
        let (controller, _rx) = controller();
        controller.declare("t", Signal::SIGTERM);
        controller.start("t", "obelisk-no-such-binary", &[]);
        assert!(!session(&controller, "t").start_error.is_empty());

        let (cmd, args) = shell("exit 0");
        controller.start("t", &cmd, &args);
        assert!(session(&controller, "t").start_error.is_empty(), "a new run must not wear the last one's failure");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(0));
    }

    #[tokio::test]
    async fn the_shutdown_reap_stops_everything_and_waits_for_it() {
        let (controller, _rx) = controller();
        controller.declare("a", Signal::SIGTERM);
        controller.declare("b", Signal::SIGTERM);
        let (cmd, args) = shell("while :; do sleep 0.05; done");
        controller.start("a", &cmd, &args);
        controller.start("b", &cmd, &args);
        until(&controller, "a", |s| s.running).await;
        until(&controller, "b", |s| s.running).await;

        controller.reap_all().await;
        // Awaited, not fired and forgotten: the state is already settled when this returns.
        assert!(!session(&controller, "a").running);
        assert!(!session(&controller, "b").running);
    }

    #[tokio::test]
    async fn a_program_that_ignores_its_stop_signal_is_escalated() {
        let (controller, _rx) = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGINT);
        let (cmd, args) = trapping_shell("trap '' INT", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        // `STOP_GRACE` is real time, so this is the one test that waits it out.
        controller.stop("t");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(
            session(&controller, "t").exit_code,
            None,
            "SIGKILL is not an exit status; a signalled death reports no code"
        );
    }
}
