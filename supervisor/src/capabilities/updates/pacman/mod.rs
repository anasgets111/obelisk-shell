//! Arch's `pacman`, as an `obelisk.updates` backend (ADR-0034, ADR-0134). The one implementation
//! of [`super::backend::Backend`] this Supervisor ships. Everything under here knows about
//! `libalpm`, `/etc/pacman.conf` and `pacman`'s own stdout; nothing above the trait does.

pub mod check;
pub mod conf;
pub mod install;

use std::path::{Path, PathBuf};

use super::backend::{Backend, InstallCommand, InstallStep, UpdateCandidate};

/// Checks use a throwaway db root; installs use the real one. Injected paths let tests use a
/// tempdir.
pub struct PacmanBackend {
    conf_path: PathBuf,
    db_root: PathBuf,
}

impl PacmanBackend {
    pub fn new(conf_path: PathBuf, db_root: PathBuf) -> Self {
        Self { conf_path, db_root }
    }
}

impl Backend for PacmanBackend {
    fn name(&self) -> &'static str {
        "pacman"
    }

    fn check(&self) -> Result<Vec<UpdateCandidate>, String> {
        let candidates = check_against_a_throwaway_copy(&self.conf_path, &self.db_root);
        // After dropping `alpm`, return its sync-database pages before the arena sits idle for the
        // session. It is the Supervisor's largest allocation (the memory helper measures this).
        crate::memory::return_free_pages_to_the_kernel();
        candidates
    }

    /// Root upgrade against real `/etc/pacman.conf` and `/var/lib/pacman`. `pkexec` triggers
    /// Obelisk's registered polkit agent instead of requiring a terminal.
    fn install_command(&self) -> InstallCommand {
        InstallCommand {
            program: "pkexec".to_string(),
            arguments: vec!["pacman".to_string(), "-Syu".to_string(), "--noconfirm".to_string()],
        }
    }

    fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
        install::parse_install_step(line)
    }
}

/// Uses a fresh tempdir with one symlink to `db_root/local`, then syncs and checks there, never in
/// the real db (ADR-0034, amended ADR-0113). Only tempdir `sync/` is written.
///
/// Uses a symlink like `checkupdates` (`ln -s "${DBPath}/local" "$CHECKUPDATES_DB"`): `local/` is
/// read-only installed metadata. The replaced copy walked ~1,500 package directories on every
/// check; the throwaway prototype that proved no `fakeroot` was needed copied the whole tree
/// (ADR-0034).
///
/// ponytail: unlike the old snapshot copy, the symlink can observe a concurrent install mid-write,
/// causing a transient `check_error`. This matches `checkupdates` and self-heals next check.
fn check_against_a_throwaway_copy(conf_path: &Path, db_root: &Path) -> Result<Vec<UpdateCandidate>, String> {
    let throwaway = tempfile::tempdir().map_err(|err| format!("failed to create a throwaway temp dir: {err}"))?;
    link_local_db(db_root, throwaway.path())?;

    let repos = conf::resolve_repo_servers(conf_path);
    if repos.is_empty() {
        return Err(format!("no repos resolved from {}", conf_path.display()));
    }

    check::check_for_updates(Path::new("/"), throwaway.path(), &repos).map_err(|err| err.to_string())
}

/// Links `db_root/local` in as `throwaway/local`, the one name `alpm` looks for when it reads
/// installed packages out of a db root. Split out from [`check_against_a_throwaway_copy`] only so
/// the name and the read-through are testable without a mirror: everything else that function does
/// needs the network.
fn link_local_db(db_root: &Path, throwaway: &Path) -> Result<(), String> {
    let local_src = db_root.join("local");
    // Refuse a missing source: `symlink` permits a dangling `local/`, which `alpm` reads as no
    // installed packages and therefore every mirror package being an update.
    if !local_src.is_dir() {
        return Err(format!("{} is not a directory; cannot check updates against it", local_src.display()));
    }
    std::os::unix::fs::symlink(&local_src, throwaway.join("local"))
        .map_err(|err| format!("failed to link {} into a throwaway dir: {err}", local_src.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_names_itself_after_the_command_a_config_would_recognize() {
        let backend = PacmanBackend::new(PathBuf::from("/etc/pacman.conf"), PathBuf::from("/var/lib/pacman"));
        assert_eq!(backend.name(), "pacman");

        let command = backend.install_command();
        assert_eq!(command.program, "pkexec", "elevation goes through polkit, so Obelisk's own agent prompts");
        assert_eq!(command.arguments, vec!["pacman", "-Syu", "--noconfirm"]);
    }

    #[test]
    fn the_throwaway_db_root_reads_installed_packages_through_a_link_named_local() {
        // `alpm` looks specifically for `local/`; another link name makes every package look new.
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
        // An existing destination is an error, preventing reuse of stale package metadata.
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
