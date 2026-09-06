//! Package manager abstraction for `oblisk.updates` (ADR-0134).
//!
//! Separates `controller.rs` scheduling/state from package-manager execution, parsing, and reboot
//! requirements.

use std::path::PathBuf;

/// One installed package with a newer version, also the `updates.packages` Lua shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct UpdateCandidate {
    /// The package name, as the package manager spells it.
    pub name: String,
    /// Installed version in the manager's spelling.
    pub old_version: String,
    /// Version offered by synced repositories.
    pub new_version: String,
    /// Bytes to fetch; `0` when already cached.
    pub download_size: i64,
    /// Bytes occupied unpacked, not a delta. Config subtracts the old size if needed.
    pub installed_size: i64,
}

/// Parsed install progress: which package of how many. `None` for other lines; progress stays put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStep {
    pub current: u32,
    pub total: u32,
    pub package: String,
}

/// Privileged upgrade command. Backends use `pkexec`, routing the prompt to Oblisk's polkit agent
/// (`dbus::polkit`) instead of a terminal.
pub struct InstallCommand {
    pub program: String,
    pub arguments: Vec<String>,
}

/// Package-manager backend. `Send + Sync + 'static` is required because the controller shares it
/// with a spawned task; `alpm` handles remain local to one [`Backend::check`] call because they are
/// not `Send`.
pub trait Backend: Send + Sync + 'static {
    /// Manager name in `UpdatesState::package_manager`, e.g. lowercase command name `"pacman"`.
    fn name(&self) -> &'static str;

    /// Blocking outdated-package check; it syncs repo databases over the network. The caller runs
    /// it in `tokio::task::spawn_blocking`; no async runtime is assumed here.
    ///
    /// Must not modify the real system: a wrong answer is a badge error; side effects can leave a
    /// half-upgraded machine.
    fn check(&self) -> Result<Vec<UpdateCandidate>, String>;

    /// The command that performs the real upgrade.
    fn install_command(&self) -> InstallCommand;

    /// Parses one [`Backend::install_command`] output line as progress, if applicable.
    fn parse_install_step(&self, line: &str) -> Option<InstallStep>;

    /// Whether these packages require a reboot because the running kernel stays old. A naming
    /// heuristic belongs in the backend: Arch uses `linux`/`linux-zen`, Debian `linux-image-*`,
    /// Fedora `kernel-core`.
    fn needs_reboot(&self, package_names: &[String]) -> bool;
}

/// Package manager supported on this machine, or `None`. Mirrors `command -v pacman` without a
/// subprocess and gates `Services/MainService.qml`/`UpdateService.ready`; if the list grows, a
/// machine with two managers installed must put first the one that owns `/`.
pub fn detect() -> Option<Box<dyn Backend>> {
    if on_path("pacman") {
        return Some(Box::new(super::pacman::PacmanBackend::new(
            PathBuf::from("/etc/pacman.conf"),
            PathBuf::from("/var/lib/pacman"),
        )));
    }
    None
}

/// Whether `program` is a file in this process's `PATH`; avoids spawning a shell during capability
/// startup.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| program_is_in(&path, program))
}

/// [`on_path`] against an explicit `PATH`, so tests avoid mutating the process environment.
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
        // `PATH` entries must be files, not a directory named `pacman`.
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
