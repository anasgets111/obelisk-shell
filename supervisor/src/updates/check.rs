//! Real `alpm` sync + outdated-package diff for `oblisk.updates` (ADR-0034): replaces
//! `checkupdates`+`expac` subprocesses with the Arch project's own `libalpm` binding. Not
//! unit-testable (needs a real libalpm handle and real network I/O against real mirrors) --
//! verified live instead, the same empirical methodology ADR-0034's own design-phase throwaway
//! program used: copy `/var/lib/pacman` to a user-owned temp dir, sync the real repos, diff
//! against the real installed set. `alpm`'s types wrap raw C pointers and aren't `Send`, so
//! every call here must run inside one `tokio::task::spawn_blocking` closure (see
//! `controller.rs`'s scheduler) -- never awaited inline on the async runtime, matching this
//! codebase's async-hygiene rule (build-steps.md Phase 9) for the same reason
//! `hardware::idle::notify::connect_wayland_idle`'s real-Wayland setup does.

use std::path::Path;

use super::pacman_conf::RepoServers;

/// One installed package with a newer version in some sync repo.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UpdateCandidate {
    pub name: String,
    pub old_version: String,
    pub new_version: String,
    pub download_size: i64,
    pub installed_size: i64,
}

/// Registers every repo in `repos` against `db_path` (a throwaway copy of `/var/lib/pacman`,
/// never the real system db -- `controller.rs`'s scheduler owns that copy, this function just
/// operates on whatever path it's given), syncs them all (`force: true`, matching
/// `checkupdates`'s own always-fresh-sync behavior -- ADR-0034's own empirical verification
/// found no `fakeroot` requirement for this against a user-owned temp dir), then diffs every
/// package in `root`'s installed set against its matching sync-repo entry via `alpm`'s own
/// `sync_new_version` -- the same version-comparison logic `pacman -Su` itself uses, not a
/// hand-rolled string compare.
pub fn check_for_updates(root: &Path, db_path: &Path, repos: &[RepoServers]) -> Result<Vec<UpdateCandidate>, alpm::Error> {
    let mut handle = alpm::Alpm::new(root.to_string_lossy().into_owned(), db_path.to_string_lossy().into_owned())?;

    for repo in repos {
        let db = handle.register_syncdb_mut(repo.name.clone(), alpm::SigLevel::USE_DEFAULT)?;
        for server in &repo.servers {
            db.add_server(server.as_str())?;
        }
    }

    handle.syncdbs_mut().update(true)?;

    let syncdbs = handle.syncdbs();
    let candidates = handle
        .localdb()
        .pkgs()
        .into_iter()
        .filter_map(|installed| {
            let newer = installed.sync_new_version(syncdbs)?;
            Some(UpdateCandidate {
                name: installed.name().to_string(),
                old_version: installed.version().to_string(),
                new_version: newer.version().to_string(),
                download_size: newer.download_size(),
                installed_size: newer.isize(),
            })
        })
        .collect();

    Ok(candidates)
}

