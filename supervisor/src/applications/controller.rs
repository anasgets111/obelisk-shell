//! [`ApplicationsController`]: the `oblisk.applications` state owner and its two write actions
//! (docs/adr/0061).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use super::scan::{AppSummary, LaunchTarget, scan};

/// `oblisk.applications`'s payload (docs/adr/0061 decision 2).
///
/// `by_app_id` repeats the summaries in `entries` rather than indexing into it. An index would
/// have to be a Lua array index, and Lua counts from one while the JSON array this serializes to
/// counts from zero, so every config reading it would carry an off-by-one nobody can see in the
/// payload. Repeating three small fields for a few hundred entries costs less than that trap.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ApplicationsState {
    pub entries: Vec<AppSummary>,
    pub by_app_id: BTreeMap<String, AppSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationsSignal {
    Changed,
}

/// What `launch` could not do, so the caller can log one line naming the reason rather than a
/// generic failure.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// No entry with that desktop file id, which for a config reading `entries` means a scan has
    /// replaced the list since it was drawn.
    Unknown,
    /// `Terminal=true` with no `$TERMINAL` set -- see [`ApplicationsController::launch`].
    NoTerminal,
    Spawn(String),
}

/// `Clone` so `main.rs`'s dispatch arm can hand a cheap `Arc`-backed copy to the blocking scan
/// task, the same shape `UpdatesController` uses for `install`.
#[derive(Clone)]
pub struct ApplicationsController {
    state: Arc<Mutex<ApplicationsState>>,
    /// Not part of the snapshot: an argv the config could read is an argv the config could be
    /// tricked into rewriting before `launch` ran it (docs/adr/0061 decision 3).
    launch_targets: Arc<Mutex<HashMap<String, LaunchTarget>>>,
    dirs: Arc<Vec<PathBuf>>,
    events: UnboundedSender<ApplicationsSignal>,
}

/// The argv `launch` actually spawns, given the entry and whatever `$TERMINAL` says.
///
/// Split out from [`ApplicationsController::launch`] so the `Terminal=true` rule is testable
/// without writing to the process environment, the same shape `system::should_emit` and
/// `layer::exclusive_zone_for` already use for their own decisions. An empty `$TERMINAL` counts as
/// unset: exporting it blank is how a shell leaves a variable it never assigned.
fn command_line(terminal: Option<String>, target: LaunchTarget) -> Result<(String, Vec<String>), LaunchError> {
    if !target.terminal {
        return Ok((target.command, target.args));
    }
    let terminal = terminal.filter(|value| !value.is_empty()).ok_or(LaunchError::NoTerminal)?;
    let mut args = vec!["-e".to_string(), target.command];
    args.extend(target.args);
    Ok((terminal, args))
}

impl ApplicationsController {
    /// Builds the controller empty and starts the first scan in the background.
    ///
    /// Not scanned inline: this runs inside `main`'s startup, and a few hundred `.desktop` files
    /// read off a cold page cache is real milliseconds spent before the first surface is up. The
    /// capability reads `nil` in Lua until the scan lands, which every capability already does
    /// (`shared::CAPABILITIES`' own doc comment), so a config that handles an absent snapshot
    /// handles this with no extra branch.
    pub fn new(dirs: Vec<PathBuf>, events: UnboundedSender<ApplicationsSignal>) -> Self {
        let controller = ApplicationsController {
            state: Arc::new(Mutex::new(ApplicationsState::default())),
            launch_targets: Arc::new(Mutex::new(HashMap::new())),
            dirs: Arc::new(dirs),
            events,
        };
        controller.refresh();
        controller
    }

    pub fn snapshot(&self) -> ApplicationsState {
        self.state.lock().expect("applications state mutex poisoned").clone()
    }

    /// Rescans the applications directories off-thread, then signals `main`'s `select!` to push.
    ///
    /// `spawn_blocking` rather than a plain task: this is `read_dir` plus a `read_to_string` per
    /// entry, which is exactly the blocking filesystem work a tokio worker thread must not do.
    ///
    /// Pushes only on a real change. Every `StateSnapshot` marks the Renderer's scene dirty and
    /// drives a full re-resolve and repaint (docs/adr/0044), so a config calling `refresh` each
    /// time its launcher opens would otherwise repaint the whole shell for a list that is
    /// identical nearly every time.
    pub fn refresh(&self) {
        let state = Arc::clone(&self.state);
        let launch_targets = Arc::clone(&self.launch_targets);
        let dirs = Arc::clone(&self.dirs);
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let result = scan(&dirs);
            let next = ApplicationsState { entries: result.entries, by_app_id: result.by_app_id };
            *launch_targets.lock().expect("applications launch map mutex poisoned") = result.launch;
            let mut current = state.lock().expect("applications state mutex poisoned");
            if *current == next {
                return;
            }
            *current = next;
            drop(current);
            let _ = events.send(ApplicationsSignal::Changed);
        });
    }

    /// Runs the application `id` names, detached.
    ///
    /// Detached is the point, and it is why this is its own action rather than the config calling
    /// `process.run`: `process.run` pipes stdout and stderr and holds the child for its exit code
    /// (docs/adr/0026), which for a launched GUI application means the Supervisor keeps two pipes
    /// and a `Child` alive for the whole life of a program it has nothing more to say to. A
    /// generation swap would also reap it, so opening a text editor and then editing the config
    /// would close the editor.
    ///
    /// ponytail: `Terminal=true` needs an emulator and there is no specified way to find one, so
    /// this reads `$TERMINAL` and refuses if it is unset rather than guessing. Probing `PATH` for
    /// a list of known emulators is the upgrade, and it is left out because a guess that picks the
    /// wrong one is worse than a refusal that says why: a user who sets `$TERMINAL` gets exactly
    /// the terminal they asked for, and a user who does not gets a log line naming the variable.
    pub fn launch(&self, id: &str) -> Result<(), LaunchError> {
        let target = {
            let targets = self.launch_targets.lock().expect("applications launch map mutex poisoned");
            targets.get(id).cloned().ok_or(LaunchError::Unknown)?
        };
        let (command, args) = command_line(std::env::var("TERMINAL").ok(), target)?;
        // The `Child` is dropped rather than awaited, which is what detaches it: tokio reaps an
        // orphaned child through its own background reaper, so nothing here has to wait on a
        // program the shell has no further relationship with.
        match crate::process::spawn_group_leader(&command, &args, &[]) {
            Ok(_) => Ok(()),
            Err(err) => Err(LaunchError::Spawn(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a controller over one temporary applications directory and waits for its opening
    /// scan to land. `new` starts that scan in the background, so every test here would otherwise
    /// race it and read an empty list.
    async fn controller_over(entries: &[(&str, &str)]) -> (ApplicationsController, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for (file, body) in entries {
            std::fs::write(dir.path().join(file), body).unwrap();
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ApplicationsController::new(vec![dir.path().to_path_buf()], tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("the opening scan must signal").unwrap();
        (controller, dir)
    }

    fn runnable(name: &str, command: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec={command}\n")
    }

    #[tokio::test]
    async fn the_opening_scan_populates_the_snapshot_without_being_asked() {
        let (controller, _dir) = controller_over(&[("thing.desktop", &runnable("Thing", "/bin/true"))]).await;

        let state = controller.snapshot();

        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].name, "Thing");
        assert!(state.by_app_id.contains_key("thing"), "the id must be reachable as an app_id");
    }

    /// The whole point of `launch` being its own action: the argv lives here, keyed by id, and
    /// never travels through the config (docs/adr/0061 decision 3).
    #[tokio::test]
    async fn launch_runs_the_entrys_own_command() {
        let marker = tempfile::tempdir().unwrap();
        let touched = marker.path().join("ran");
        let (controller, _dir) =
            controller_over(&[("t.desktop", &runnable("Toucher", &format!("/usr/bin/touch {}", touched.display())))]).await;

        controller.launch("t").expect("launching a known entry must succeed");

        // The child is detached, so there is no handle to await -- poll for the side effect.
        for _ in 0..50 {
            if touched.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("launch did not run the entry's command");
    }

    #[tokio::test]
    async fn launching_an_id_that_is_not_installed_is_reported_rather_than_spawning_anything() {
        let (controller, _dir) = controller_over(&[("thing.desktop", &runnable("Thing", "/bin/true"))]).await;

        assert_eq!(controller.launch("nothing-like-this"), Err(LaunchError::Unknown));
    }

    fn console_program() -> LaunchTarget {
        LaunchTarget { command: "/usr/bin/top".to_string(), args: vec!["-u".to_string()], terminal: true }
    }

    /// A console program with no emulator to run it in is refused, not spawned into a session with
    /// no terminal where it would exit instantly and look like nothing happened.
    #[test]
    fn a_terminal_entry_is_refused_when_the_environment_names_no_terminal() {
        assert_eq!(command_line(None, console_program()), Err(LaunchError::NoTerminal));
        assert_eq!(command_line(Some(String::new()), console_program()), Err(LaunchError::NoTerminal), "an empty $TERMINAL is unset");
    }

    #[test]
    fn a_terminal_entry_is_wrapped_in_the_emulator_the_environment_names() {
        let (command, args) = command_line(Some("foot".to_string()), console_program()).expect("a named terminal must be accepted");

        assert_eq!(command, "foot");
        assert_eq!(args, vec!["-e".to_string(), "/usr/bin/top".to_string(), "-u".to_string()]);
    }

    #[test]
    fn an_ordinary_entry_is_spawned_directly_whatever_the_environment_says() {
        let target = LaunchTarget { command: "firefox".to_string(), args: vec!["--new-tab".to_string()], terminal: false };

        let (command, args) = command_line(Some("foot".to_string()), target).expect("a graphical entry needs no terminal");

        assert_eq!(command, "firefox", "$TERMINAL must not wrap an entry that never asked for it");
        assert_eq!(args, vec!["--new-tab".to_string()]);
    }

    /// `refresh` must not push when nothing changed: every snapshot repaints the whole scene
    /// (docs/adr/0044), and the dev config calls `refresh` each time its launcher opens.
    #[tokio::test]
    async fn refreshing_an_unchanged_directory_pushes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.desktop"), runnable("A", "/bin/true")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ApplicationsController::new(vec![dir.path().to_path_buf()], tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("the opening scan signals").unwrap();

        controller.refresh();

        let second = tokio::time::timeout(std::time::Duration::from_millis(400), rx.recv()).await;
        assert!(second.is_err(), "a rescan finding the same entries must not push a snapshot");
    }

    #[tokio::test]
    async fn refreshing_after_an_entry_appears_pushes_the_new_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.desktop"), runnable("A", "/bin/true")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ApplicationsController::new(vec![dir.path().to_path_buf()], tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("the opening scan signals").unwrap();

        std::fs::write(dir.path().join("b.desktop"), runnable("B", "/bin/true")).unwrap();
        controller.refresh();

        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("a changed scan must push").unwrap();
        assert_eq!(controller.snapshot().entries.len(), 2);
    }
}
