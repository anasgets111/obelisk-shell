//! Arch's `pacman`, as an `oblisk.updates` backend (ADR-0034, ADR-0134). The one implementation
//! of [`super::backend::Backend`] this Supervisor ships. Everything under here knows about
//! `libalpm`, `/etc/pacman.conf` and `pacman`'s own stdout; nothing above the trait does.

pub mod check;
pub mod conf;
pub mod install;

use std::path::{Path, PathBuf};

use super::backend::{Backend, InstallCommand, InstallStep, UpdateCandidate};

/// Checks against a throwaway db root, installs through the real one. Both paths are injected
/// rather than hardcoded here, which is what lets the tests point at a temp dir.
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
        // On this thread, after the `alpm` handle is dropped and before its arena is left alone
        // for the rest of the session: `libalpm`'s parse of the sync database is the largest
        // allocation the Supervisor makes, and none of it is live by here
        // (`memory::return_free_pages_to_the_kernel` carries the measurement).
        crate::memory::return_free_pages_to_the_kernel();
        candidates
    }

    /// The real, system-modifying upgrade as root, against the real `/etc/pacman.conf` and
    /// `/var/lib/pacman`, no throwaway copy. `pkexec` rather than `sudo` because it talks to
    /// polkit, which triggers Oblisk's own already-registered agent (`dbus::polkit`) rather
    /// than needing a terminal to type into.
    fn install_command(&self) -> InstallCommand {
        InstallCommand {
            program: "pkexec".to_string(),
            arguments: vec!["pacman".to_string(), "-Syu".to_string(), "--noconfirm".to_string()],
        }
    }

    fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
        install::parse_install_step(line)
    }

    fn needs_reboot(&self, package_names: &[String]) -> bool {
        install::needs_reboot(package_names)
    }
}

/// Points a fresh `tempfile::tempdir()` at `db_root`'s `local/` with one symlink, then syncs and
/// checks against that throwaway db root, never the real `db_root` (ADR-0034, amended ADR-0113).
/// Only `sync/` is written, and it is written inside the temp dir.
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
    // Checked, because `symlink` will happily point at nothing and a dangling `local/` is not an
    // error to `alpm` -- it is an empty installed set, which reads as "every package on the
    // mirror is an update". The copy this replaces failed loudly on a missing source; so does this.
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
        assert_eq!(command.program, "pkexec", "elevation goes through polkit, so Oblisk's own agent prompts");
        assert_eq!(command.arguments, vec!["pacman", "-Syu", "--noconfirm"]);
    }

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
