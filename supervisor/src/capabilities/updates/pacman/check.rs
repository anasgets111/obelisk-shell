//! Real `alpm` sync and outdated-package diff (ADR-0034), replacing `checkupdates`+`expac`
//! subprocesses with the Arch project's own `libalpm` binding.
//! Requires a real handle and mirror I/O, so it is verified live with a throwaway db root. `alpm`
//! wraps non-`Send` C pointers; callers run this inside one `spawn_blocking` closure.

use std::path::Path;

use super::super::backend::UpdateCandidate;
use super::conf::RepoServers;

/// Registers `repos` against throwaway `db_path`, force-syncs them like `checkupdates`, then diffs
/// `root`'s installed packages with `alpm::sync_new_version`. No `fakeroot` is needed for the
/// user-owned temp db.
pub fn check_for_updates(
    root: &Path,
    db_path: &Path,
    repos: &[RepoServers],
) -> Result<Vec<UpdateCandidate>, alpm::Error> {
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
            installed.sync_new_version(syncdbs).map(|newer| UpdateCandidate {
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
