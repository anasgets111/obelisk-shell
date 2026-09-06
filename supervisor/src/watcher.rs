//! Supervisor-side config-file watcher (`CONTEXT.md`, Watcher; ADR-0047 decision 3).
//!
//! Watches the whole config directory tree (`~/.config/oblisk/` by default), not just `shell.lua`,
//! for `.lua` changes. Coalesces save bursts
//! (`CREATE`+`MODIFY`+`CLOSE_WRITE`, or atomic-save `MOVED_TO`) and triggers after the debounce
//! window since the last relevant event. Watches directories, not inodes, because atomic-save
//! editors unlink/recreate files.
//!
//! inotify has no recursive mode, so add one watch per directory and map `WatchDescriptor` to path.
//! Later directories get a watch on `CREATE` and are walked if non-empty (`mkdir -p a/b/c`);
//! the `ISDIR` arm documents the remaining race.
//!
//! A `path -> hash` map (ADR-0047 decision 3) rejects byte-identical saves, which editors often
//! truncate/rewrite; debounce only collapses one burst. Deleted files always count.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use tokio::sync::mpsc;

/// Handled inotify kinds: create, modify, atomic-save rename in/out (including delete-via-rename),
/// delete, and completed write. Read events (`ACCESS`, `OPEN`, ...) are not requested.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MODIFY
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::DELETE
        | WatchMask::CLOSE_WRITE
}

/// Watches `dir` and recursively covers current subdirectories, including later directories walked
/// from `spawn_watcher`'s `ISDIR` arm. `wd_to_dir` expands bare descriptors to paths. Each call has
/// its own `visited` set, so deleted/recreated directories are walked again.
fn watch_tree(watches: &mut Watches, wd_to_dir: &mut HashMap<WatchDescriptor, PathBuf>, dir: &Path) -> io::Result<()> {
    walk(watches, wd_to_dir, &mut HashSet::new(), dir)
}

/// One [`watch_tree`] walk. `visited` holds canonical paths and stops symlink cycles.
fn walk(
    watches: &mut Watches,
    wd_to_dir: &mut HashMap<WatchDescriptor, PathBuf>,
    visited: &mut HashSet<PathBuf>,
    dir: &Path,
) -> io::Result<()> {
    if !visited.insert(dir.canonicalize()?) {
        return Ok(()); // a link back to somewhere this walk has already covered.
    }
    let wd = watches.add(dir, watch_mask())?;
    wd_to_dir.insert(wd, dir.to_path_buf());

    // Report and skip unreadable subdirectories; startup must not fail for one. The requested root
    // `add` remains fatal.
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("config watcher: cannot list {}: {err}; changes inside it will not reload", dir.display());
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type()` would skip symlinked `widgets -> ~/dotfiles/oblisk/widgets`; `metadata()`
        // follows the link.
        if !path.metadata().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Err(err) = walk(watches, wd_to_dir, visited, &path) {
            eprintln!("config watcher: cannot watch {}: {err}; changes inside it will not reload", path.display());
        }
    }
    Ok(())
}

/// Forgets `dir` and descendants, reporting whether a hashed `.lua` was removed. Directory rename
/// emits one `MOVED_FROM`, not child deletes; without this, watches leak toward
/// `fs.inotify.max_user_watches` and recreated identical files keep stale hashes. `rm -rf` emits
/// per-file deletes; `git stash`/`git checkout` hit this. Report hashed files only; an untouched
/// directory was never hashed, so its removal waits for the next edit.
fn forget_subtree(
    watches: &mut Watches,
    wd_to_dir: &mut HashMap<WatchDescriptor, PathBuf>,
    hashes: &mut HashMap<PathBuf, u64>,
    dir: &Path,
) -> bool {
    let doomed: Vec<WatchDescriptor> =
        wd_to_dir.iter().filter(|(_, watched)| watched.starts_with(dir)).map(|(wd, _)| wd.clone()).collect();
    for wd in doomed {
        // Kernel already invalidated this watch, typically by deletion.
        let _ = watches.remove(wd.clone());
        wd_to_dir.remove(&wd);
    }
    let known = hashes.len();
    hashes.retain(|path, _| !path.starts_with(dir));
    known != hashes.len()
}

/// Hashes current contents, or `None` if delete/move won the race. `DefaultHasher` is neither
/// specified nor cryptographic; neither matters here.
fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Watches `dir`'s tree for `.lua` changes, debounced by `debounce`, and emits one `()` per settled
/// burst. See the module comment for irrelevant events.
pub fn spawn_watcher(dir: &Path, debounce: Duration) -> io::Result<mpsc::UnboundedReceiver<()>> {
    let inotify = Inotify::init()?;
    let mut watches = inotify.watches();
    let mut wd_to_dir = HashMap::new();
    watch_tree(&mut watches, &mut wd_to_dir, dir)?;
    let mut stream = inotify.into_event_stream(vec![0u8; 4096])?;

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Last-seen path hash (ADR-0047 decision 3); empty so every first event counts.
        let mut hashes: HashMap<PathBuf, u64> = HashMap::new();

        // Absolute deadline, not a relative sleep re-armed by irrelevant events, which could delay
        // the trigger forever under unrelated activity.
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(Ok(event)) => {
                            if event.mask.contains(EventMask::IGNORED) {
                                // Kernel dropped this watch; remove it from `wd_to_dir`.
                                wd_to_dir.remove(&event.wd);
                                continue;
                            }

                            let Some(parent) = wd_to_dir.get(&event.wd) else {
                                continue; // watch already cleaned up above: ignore stragglers.
                            };
                            let Some(name) = event.name.as_deref() else {
                                continue; // event about the watched directory itself, not an entry.
                            };
                            let path = parent.join(name);

                            if event.mask.contains(EventMask::ISDIR) {
                                if event.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO) {
                                    // Watch new dirs; `mkdir -p a/b/c` may be non-empty.
                                    // ponytail: a write between CREATE and walk can slip past.
                                    // Upgrade: recursive `notify` watcher (`RecommendedWatcher`)
                                    // if more than a config directory justifies it.
                                    let _ = watch_tree(&mut watches, &mut wd_to_dir, &path);
                                } else if event.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM)
                                    && forget_subtree(&mut watches, &mut wd_to_dir, &mut hashes, &path)
                                {
                                    deadline = Some(tokio::time::Instant::now() + debounce);
                                }
                                continue; // a directory itself is never a config file.
                            }

                            if path.extension() != Some(OsStr::new("lua")) {
                                continue; // not a config file: README, script, or editor swap.
                            }

                            if event.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                                // No bytes to hash; deletion always changes state.
                                hashes.remove(&path);
                                deadline = Some(tokio::time::Instant::now() + debounce);
                                continue;
                            }

                            match hash_file(&path) {
                                Some(hash) if hashes.get(&path) == Some(&hash) => {} // unchanged.
                                Some(hash) => {
                                    hashes.insert(path, hash);
                                    deadline = Some(tokio::time::Instant::now() + debounce);
                                }
                                None => {} // delete/move won the read race.
                            }
                        }
                        Some(Err(err)) => {
                            eprintln!("config watcher: inotify read failed: {err}");
                        }
                        None => break, // the inotify fd closed: nothing left to watch.
                    }
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    deadline = None;
                    if tx.send(()).is_err() {
                        break; // receiver dropped: nobody's listening any more.
                    }
                }
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moved directory emits no child DELETEs, only one MOVED_FROM; `forget_subtree` clears its
    /// hashes.
    #[tokio::test]
    async fn a_subdirectory_moved_away_then_recreated_with_the_same_bytes_still_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(dir.path().join("widgets/clock.lua"), "return 1").unwrap();

        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        std::fs::write(dir.path().join("widgets/clock.lua"), "return 1").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the first write records the hash this test is about");

        std::fs::rename(dir.path().join("widgets"), elsewhere.path().join("moved")).unwrap();
        let _ = recv_within(&mut rx, WAIT).await;

        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        // Watch before writing; otherwise this hits the ISDIR race, not the hash map.
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::fs::write(dir.path().join("widgets/clock.lua"), "return 1").unwrap();
        assert!(
            recv_within(&mut rx, WAIT).await.is_some(),
            "a file recreated under a rebuilt directory has to reload, whatever its bytes"
        );
    }

    /// Dotfiles-repository layout; `walk` explains why `metadata()`, not `file_type()`, follows it.
    #[tokio::test]
    async fn a_lua_file_inside_a_symlinked_subdirectory_still_fires_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("clock.lua"), "return {}").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("widgets")).unwrap();

        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        std::fs::write(outside.path().join("clock.lua"), "return { changed = true }").unwrap();

        assert!(
            recv_within(&mut rx, WAIT).await.is_some(),
            "an edit through a symlinked config subdirectory has to reload"
        );
    }

    /// An ancestor symlink is a cycle; without the guard recursion exhausts the stack. The
    /// assertion proves the walk terminates.
    #[tokio::test]
    async fn a_symlink_cycle_in_the_config_tree_does_not_recurse_forever() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("widgets/loop")).unwrap();

        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the tree still has to be watched after the cycle is cut");
    }

    /// An unreadable directory must not stop shell startup; see `walk`.
    #[tokio::test]
    async fn an_unreadable_subdirectory_is_skipped_rather_than_failing_the_whole_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        // Root ignores mode bits, leaving nothing to skip or assert.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            return;
        }

        let watcher = spawn_watcher(dir.path(), SHORT_DEBOUNCE);
        let restore = std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        let mut rx = watcher.expect("an unreadable subdirectory must not fail the watcher");
        restore.unwrap();

        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the rest of the tree still has to be watched");
    }

    async fn recv_within(rx: &mut mpsc::UnboundedReceiver<()>, timeout: Duration) -> Option<()> {
        tokio::time::timeout(timeout, rx.recv()).await.unwrap_or(None)
    }

    const SHORT_DEBOUNCE: Duration = Duration::from_millis(40);
    const WAIT: Duration = Duration::from_millis(500);

    #[tokio::test]
    async fn a_single_write_to_shell_lua_fires_exactly_one_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "a write to shell.lua must fire a trigger");
        assert!(
            recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(),
            "must not fire a second trigger for the same settled write"
        );
    }

    #[tokio::test]
    async fn a_burst_of_rapid_writes_coalesces_into_one_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        let path = dir.path().join("shell.lua");

        std::fs::write(&path, "return {}").unwrap();
        tokio::time::sleep(SHORT_DEBOUNCE / 4).await;
        std::fs::write(&path, "return { id = 2 }").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the burst must fire a trigger");
        assert!(
            recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(),
            "a rapid burst must coalesce into exactly one trigger, not two"
        );
    }

    #[tokio::test]
    async fn writing_an_unrelated_file_fires_no_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("notes.txt"), "not shell.lua").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_none(), "an unrelated file must not trigger a reload");
    }

    #[tokio::test]
    async fn an_unrelated_event_during_the_debounce_window_does_not_push_back_the_deadline() {
        // Regression: rebuilding sleep(debounce) for every inotify event, even irrelevant ones,
        // restarted the window. A wider debounce than SHORT_DEBOUNCE leaves jitter margin.
        let debounce = Duration::from_millis(80);
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), debounce).unwrap();

        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();
        tokio::time::sleep(debounce / 2).await;
        std::fs::write(dir.path().join("notes.txt"), "unrelated").unwrap();

        // The deadline is `debounce`/2 away. Correct behavior fires here; a from-here restart fires
        // a full `debounce` later. This has 20ms margin on either side.
        assert!(
            recv_within(&mut rx, debounce / 2 + Duration::from_millis(20)).await.is_some(),
            "an unrelated event during the debounce window must not delay the trigger"
        );
    }

    #[tokio::test]
    async fn a_write_to_a_non_shell_lua_file_at_the_top_level_still_fires_a_trigger() {
        // Any `.lua` matches; configs can split across files (ADR-0047).
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("colors.lua"), "return {}").unwrap();

        assert!(
            recv_within(&mut rx, WAIT).await.is_some(),
            "any .lua file at the top level must trigger a reload, not just shell.lua"
        );
    }

    #[tokio::test]
    async fn a_write_to_a_lua_file_in_a_subdirectory_present_at_startup_fires_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("widgets/clock.lua"), "return {}").unwrap();

        assert!(
            recv_within(&mut rx, WAIT).await.is_some(),
            "a .lua file inside a subdirectory that existed at startup must trigger a reload"
        );
    }

    #[tokio::test]
    async fn a_directory_created_after_startup_is_watched_for_lua_changes() {
        // A post-start `widgets/` folder must reload without restarting.
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        // Let CREATE register the watch before writing; this is the ISDIR race, though real editor
        // saves are slower.
        tokio::time::sleep(SHORT_DEBOUNCE).await;
        std::fs::write(dir.path().join("widgets/clock.lua"), "return {}").unwrap();

        assert!(
            recv_within(&mut rx, WAIT).await.is_some(),
            "a .lua file inside a directory created after startup must trigger a reload"
        );
    }

    #[tokio::test]
    async fn rewriting_a_lua_file_with_identical_contents_fires_no_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(&path, "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the first write must fire a trigger");
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "must settle before the next write");

        // Write-truncate-rewrite with identical bytes emits MODIFY/CLOSE_WRITE but is no change.
        std::fs::write(&path, "return {}").unwrap();

        assert!(
            recv_within(&mut rx, WAIT).await.is_none(),
            "rewriting identical contents must not fire a second trigger"
        );
    }

    #[tokio::test]
    async fn rewriting_a_lua_file_with_different_contents_still_fires_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(&path, "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the first write must fire a trigger");
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "must settle before the next write");

        std::fs::write(&path, "return { id = 2 }").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "a genuinely changed rewrite must still fire a trigger");
    }

    #[tokio::test]
    async fn deleting_a_lua_file_fires_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(&path, "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the write must fire a trigger");
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "must settle before the delete");

        std::fs::remove_file(&path).unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "deleting a .lua file must fire a trigger");
    }
}
