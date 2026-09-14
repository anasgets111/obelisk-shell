//! [`ApplicationsController`]: `obelisk.applications`'s state owner and two write actions
//! (ADR-0061).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use super::scan::{AppSummary, LaunchTarget, scan};

/// `obelisk.applications`'s payload (ADR-0061 decision 2).
///
/// `by_app_id` repeats summaries instead of indexing `entries`: Lua arrays start at one while the
/// serialized JSON array starts at zero. Repeating three small fields for a few hundred entries
/// avoids an invisible off-by-one.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ApplicationsState {
    /// Visible, launchable installed entries, sorted by name. Rebuilt by
    /// `:invoke("refresh")`; directories are not watched, so mid-session installs wait for it.
    pub entries: Vec<AppSummary>,
    /// The same entries keyed by a window's `app_id`, for callers holding
    /// `workspaces.active_client.class` rather than a desktop id. Exact `StartupWMClass` and
    /// desktop id win over case-folded and last-dot-segment spellings; exact keys are never
    /// displaced. A miss is only a heuristic miss, not proof that the app is uninstalled.
    pub by_app_id: BTreeMap<String, AppSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationsSignal {
    Changed,
}

/// A URL `open_url` refuses to hand to the desktop opener, with the reason (ADR-0103).
#[derive(Debug, PartialEq, Eq)]
pub enum OpenUrlError {
    Refused(&'static str),
    Spawn(String),
}

/// `open_url`'s 2048-byte cap. Browsers accept more; notification bodies cap URLs at 512 bytes, so
/// body URLs cannot exceed that and config-built URLs still get this bound.
pub const MAX_URL_BYTES: usize = 2048;

/// Schemes handed to the desktop opener (ADR-0103): web and mail links from notification bodies.
/// `file:` is excluded because notification bodies are untrusted text and notification paths use
/// trusted-root checks; application schemes (`tg:`, `spotify:`, `steam:`) choose a program.
const OPENABLE_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// Accepts only an allowlisted scheme, no whitespace/control character anywhere (nothing legitimate
/// carries one; a newline would split a log line), and a URL under [`MAX_URL_BYTES`].
pub fn openable_url(url: &str) -> Result<(), &'static str> {
    if url.len() > MAX_URL_BYTES {
        return Err("longer than 2048 bytes");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("holds whitespace or a control character");
    }
    let Some((scheme, _)) = url.split_once(':') else {
        return Err("has no scheme");
    };
    if !OPENABLE_SCHEMES.iter().any(|allowed| scheme.eq_ignore_ascii_case(allowed)) {
        return Err("scheme is not http, https or mailto");
    }
    Ok(())
}

/// Why `launch` failed, so the caller can log a specific reason.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// No entry has that desktop id; a refresh may have replaced the list since it was drawn.
    Unknown,
    /// `Terminal=true` with no `$TERMINAL` set, see [`ApplicationsController::launch`].
    NoTerminal,
    Spawn(String),
}

/// `Arc`-backed fields, so `refresh` hands the blocking scan task its own handles without
/// cloning the controller; nothing clones the whole thing.
pub struct ApplicationsController {
    state: Arc<Mutex<ApplicationsState>>,
    /// Not in the snapshot: exposing argv would let config rewrite it before `launch` (ADR-0061
    /// decision 3).
    launch_targets: Arc<Mutex<HashMap<String, LaunchTarget>>>,
    dirs: Arc<Vec<PathBuf>>,
    events: UnboundedSender<ApplicationsSignal>,
}

/// The argv `launch` spawns for an entry and `$TERMINAL`.
///
/// Split from [`ApplicationsController::launch`] so `Terminal=true` is testable without mutating
/// the environment, as `system::should_emit` and `layer::exclusive_zone_for` do. Empty
/// `$TERMINAL` counts as unset, as with a shell-exported blank variable.
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
    /// Builds an empty controller and starts the first scan in the background.
    ///
    /// The scan stays off `main`'s startup path: reading a few hundred `.desktop` files from a
    /// cold page cache costs real milliseconds before the first surface. Lua reads `nil` until it
    /// lands, as every capability does (`shared::Capability::ALL`), so no extra branch is needed.
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

    /// Rescans off-thread, then signals `main`'s `select!` to push.
    ///
    /// `spawn_blocking` is required for `read_dir` plus one `read_to_string` per entry: blocking
    /// filesystem work must not run on a Tokio worker thread.
    ///
    /// Pushes only on change. Every `StateSnapshot` dirties the Renderer and triggers a full
    /// re-resolve/repaint (ADR-0044); launcher-open `refresh` would otherwise repaint an identical
    /// list.
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

    /// Runs the application named by `id`, detached.
    ///
    /// This is separate from config `process.run`: that action pipes stdout/stderr and holds the
    /// `Child` for its exit code (ADR-0026). For a GUI app that keeps two pipes and a child alive
    /// for its whole run; a generation swap would reap it and close an editor when config reloads.
    ///
    /// [`crate::process::spawn_detached`] rather than a new process group: a group leader is still
    /// a direct child of the Supervisor, so every launched app sat under the shell in the process
    /// tree and depended on nobody putting it in the reap registry (ADR-0188).
    ///
    /// ponytail: `Terminal=true` reads `$TERMINAL` and refuses when unset. Probing `PATH` for
    /// known emulators is the upgrade; a set variable gets exactly that terminal, while guessing
    /// wrong is worse than naming the missing variable.
    pub fn launch(&self, id: &str) -> Result<(), LaunchError> {
        let target = {
            let targets = self.launch_targets.lock().expect("applications launch map mutex poisoned");
            targets.get(id).cloned().ok_or(LaunchError::Unknown)?
        };
        let (command, args) = command_line(std::env::var("TERMINAL").ok(), target)?;
        match crate::process::spawn_detached(&command, &args) {
            Ok(()) => Ok(()),
            Err(err) => Err(LaunchError::Spawn(err.to_string())),
        }
    }

    /// After [`openable_url`] accepts it (ADR-0103), hands `url` to `xdg-open` detached. A
    /// notification body is sender text, so the desktop chooses the user's default handler and
    /// the shell still limits which schemes may reach it; config has no other link action.
    ///
    /// Uses `xdg-open`, which every desktop ships and the portal falls back to; this shell has not
    /// met a target with a MIME database but no `xdg-utils`. A missing binary is a URL-tagged
    /// spawn error.
    pub fn open_url(&self, url: &str) -> Result<(), OpenUrlError> {
        openable_url(url).map_err(OpenUrlError::Refused)?;
        match crate::process::spawn_detached("xdg-open", &[url.to_string()]) {
            Ok(()) => Ok(()),
            Err(err) => Err(OpenUrlError::Spawn(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_web_or_mail_url_is_openable_whatever_the_schemes_case() {
        assert_eq!(openable_url("https://example.org/a?b=c#d"), Ok(()));
        assert_eq!(openable_url("http://example.org"), Ok(()));
        assert_eq!(openable_url("HTTPS://EXAMPLE.ORG"), Ok(()));
        assert_eq!(openable_url("mailto:someone@example.org"), Ok(()));
    }

    #[test]
    fn a_local_file_an_app_scheme_or_no_scheme_is_refused() {
        assert!(openable_url("file:///etc/passwd").is_err());
        assert!(openable_url("tg://resolve?domain=x").is_err());
        assert!(openable_url("javascript:alert(1)").is_err());
        assert!(openable_url("example.org").is_err());
        assert!(openable_url("").is_err());
    }

    #[test]
    fn whitespace_control_characters_and_length_are_refused() {
        assert!(openable_url("https://example.org/a b").is_err());
        assert!(openable_url("https://example.org/\n").is_err());
        assert!(openable_url("https://example.org/\u{7f}").is_err());
        assert!(openable_url(&format!("https://example.org/{}", "a".repeat(MAX_URL_BYTES))).is_err());
    }

    /// A refused URL never reaches `xdg-open`.
    #[tokio::test]
    async fn open_url_refuses_before_spawning() {
        let (controller, _dir) = controller_over(&[("thing.desktop", &runnable("Thing", "/bin/true"))]).await;
        assert_eq!(
            controller.open_url("file:///etc/passwd"),
            Err(OpenUrlError::Refused("scheme is not http, https or mailto"))
        );
    }

    /// Builds a controller over one temporary directory and waits for `new`'s background scan;
    /// otherwise tests race it and read an empty list.
    async fn controller_over(entries: &[(&str, &str)]) -> (ApplicationsController, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for (file, body) in entries {
            std::fs::write(dir.path().join(file), body).unwrap();
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ApplicationsController::new(vec![dir.path().to_path_buf()], tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the opening scan must signal")
            .unwrap();
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

    /// `launch` gets argv from this id-keyed map, never through config (ADR-0061 decision 3).
    #[tokio::test]
    async fn launch_runs_the_entrys_own_command() {
        let marker = tempfile::tempdir().unwrap();
        let touched = marker.path().join("ran");
        let (controller, _dir) =
            controller_over(&[("t.desktop", &runnable("Toucher", &format!("/usr/bin/touch {}", touched.display())))])
                .await;

        controller.launch("t").expect("launching a known entry must succeed");

        // Detached child: poll for the side effect because there is no handle to await.
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

    /// Without an emulator, refuse rather than spawn into a session where it exits immediately.
    #[test]
    fn a_terminal_entry_is_refused_when_the_environment_names_no_terminal() {
        assert_eq!(command_line(None, console_program()), Err(LaunchError::NoTerminal));
        assert_eq!(
            command_line(Some(String::new()), console_program()),
            Err(LaunchError::NoTerminal),
            "an empty $TERMINAL is unset"
        );
    }

    #[test]
    fn a_terminal_entry_is_wrapped_in_the_emulator_the_environment_names() {
        let (command, args) =
            command_line(Some("foot".to_string()), console_program()).expect("a named terminal must be accepted");

        assert_eq!(command, "foot");
        assert_eq!(args, vec!["-e".to_string(), "/usr/bin/top".to_string(), "-u".to_string()]);
    }

    #[test]
    fn an_ordinary_entry_is_spawned_directly_whatever_the_environment_says() {
        let target =
            LaunchTarget { command: "firefox".to_string(), args: vec!["--new-tab".to_string()], terminal: false };

        let (command, args) =
            command_line(Some("foot".to_string()), target).expect("a graphical entry needs no terminal");

        assert_eq!(command, "firefox", "$TERMINAL must not wrap an entry that never asked for it");
        assert_eq!(args, vec!["--new-tab".to_string()]);
    }

    /// Unchanged `refresh` must not push: every snapshot repaints the whole scene (ADR-0044), and
    /// a launcher may call it every time it opens.
    #[tokio::test]
    async fn refreshing_an_unchanged_directory_pushes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.desktop"), runnable("A", "/bin/true")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ApplicationsController::new(vec![dir.path().to_path_buf()], tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the opening scan signals")
            .unwrap();

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
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the opening scan signals")
            .unwrap();

        std::fs::write(dir.path().join("b.desktop"), runnable("B", "/bin/true")).unwrap();
        controller.refresh();

        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a changed scan must push")
            .unwrap();
        assert_eq!(controller.snapshot().entries.len(), 2);
    }
}
