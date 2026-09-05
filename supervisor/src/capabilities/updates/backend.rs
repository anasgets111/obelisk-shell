//! Package manager abstraction for `oblisk.updates` (ADR-0134).
//!
//! Separates the check schedule and state handling in `controller.rs` from
//! package manager execution, output parsing, and reboot requirements.

use std::path::PathBuf;

/// One installed package with a newer version available. The shape every backend answers in,
/// which is also the shape Lua reads out of `updates.packages`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct UpdateCandidate {
    /// The package name, as the package manager spells it.
    pub name: String,
    /// The installed version, in the manager's own version spelling.
    pub old_version: String,
    /// The version the synced repos offer.
    pub new_version: String,
    /// Bytes to fetch. `0` for a package already sitting in the manager's cache.
    pub download_size: i64,
    /// Bytes the new version occupies once unpacked. Not a delta: subtracting the old version's
    /// size is the config's job if it wants one.
    pub installed_size: i64,
}

/// One parsed progress line from the install command's output: which package of how many.
/// `None` for every other line -- the reader leaves the previous progress in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStep {
    pub current: u32,
    pub total: u32,
    pub package: String,
}

/// The privileged command that upgrades everything, as a program and its arguments. Run through
/// `pkexec` by every backend so far, which is what routes the password prompt to Oblisk's own
/// polkit agent (`dbus::polkit`) rather than a terminal.
pub struct InstallCommand {
    pub program: String,
    pub arguments: Vec<String>,
}

/// One package manager, as the scheduler sees it.
///
/// `Send + Sync` because the controller holds it in an `Arc` shared with the check task, and
/// `'static` because that task outlives the call that spawned it. That constraint is on the
/// backend value, not on what [`Backend::check`] does inside itself: `alpm`'s handle types are
/// not `Send`, and the pacman backend gets away with it by creating and dropping one entirely
/// within a single [`Backend::check`] call.
pub trait Backend: Send + Sync + 'static {
    /// What this manager is called, verbatim into `UpdatesState::package_manager` for a config
    /// to read. Lowercase, the name of the command: `"pacman"`.
    fn name(&self) -> &'static str;

    /// One check for outdated packages. Blocking, and genuinely so -- it syncs repo databases
    /// over the network. The caller runs it inside `tokio::task::spawn_blocking`; do not await
    /// anything in here and do not assume an async runtime is reachable.
    ///
    /// Must never modify the real system: a check answers a question, and the answer being
    /// wrong is a wrong badge, while a check with side effects is a half-upgraded machine.
    fn check(&self) -> Result<Vec<UpdateCandidate>, String>;

    /// The command that performs the real upgrade.
    fn install_command(&self) -> InstallCommand;

    /// One line of [`Backend::install_command`]'s output, read as progress if it is any.
    fn parse_install_step(&self, line: &str) -> Option<InstallStep>;

    /// Whether installing these package names leaves the machine owing a reboot -- the running
    /// kernel is still the old one until it restarts. A heuristic on package naming, which is
    /// exactly why it belongs to the backend: `linux` and `linux-zen` on Arch are
    /// `linux-image-*` on Debian and `kernel-core` on Fedora.
    fn needs_reboot(&self, package_names: &[String]) -> bool;
}

/// Which package manager this machine has, or `None` when it has none this Supervisor speaks.
///
/// The mirror asks `command -v pacman` and gates its whole updates module on the answer
/// (`Services/MainService.qml`, `UpdateService.ready`); this is the same question asked without
/// a subprocess. Ordered, for the day the list is longer than one: the first match wins, and a
/// machine with two managers installed is one whose *first* is the one that owns `/`.
pub fn detect() -> Option<Box<dyn Backend>> {
    if on_path("pacman") {
        return Some(Box::new(super::pacman::PacmanBackend::new(
            PathBuf::from("/etc/pacman.conf"),
            PathBuf::from("/var/lib/pacman"),
        )));
    }
    None
}

/// Whether `program` is an executable on this process's `PATH`. `command -v` without the shell:
/// detection runs at capability start, and spawning a shell to answer a filesystem question is
/// a subprocess on the path to the first frame.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| program_is_in(&path, program))
}

/// [`on_path`] against a given `PATH`, split out so it is testable without mutating this
/// process's environment -- which every other test in the binary is reading concurrently.
fn program_is_in(path: &std::ffi::OsStr, program: &str) -> bool {
    std::env::split_paths(path).any(|directory| directory.join(program).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_found_in_a_directory_that_path_names() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fake-manager"), "").unwrap();

        assert!(program_is_in(directory.path().as_os_str(), "fake-manager"));
        assert!(!program_is_in(directory.path().as_os_str(), "some-other-manager"));
    }

    #[test]
    fn a_directory_of_the_right_name_is_not_a_program() {
        // `PATH` entries hold executables; `is_file` is what keeps a `/usr/bin/pacman` directory
        // from reading as "pacman is installed".
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("pacman")).unwrap();

        assert!(!program_is_in(directory.path().as_os_str(), "pacman"));
    }

    #[test]
    fn a_program_is_found_in_the_second_of_several_path_entries() {
        let empty = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join("pacman"), "").unwrap();
        let path = std::env::join_paths([empty.path(), real.path()]).unwrap();

        assert!(program_is_in(&path, "pacman"));
    }

    #[test]
    fn an_empty_path_finds_nothing() {
        assert!(!program_is_in(std::ffi::OsStr::new(""), "pacman"));
    }
}
