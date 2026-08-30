//! Supervisor-side config-file watcher (build-steps.md Phase 13, extended by Phase 26 item 3;
//! `CONTEXT.md`, Watcher; ADR-0047 decision 3).
//!
//! Watches the whole config directory tree (`~/.config/oblisk/` and everything under it, not
//! just `shell.lua`) for changes to any `.lua` file, debounced: a burst of events one save
//! produces (`CREATE`+`MODIFY`+`CLOSE_WRITE`, or `MOVED_TO` for an atomic-save editor) coalesces
//! into exactly one trigger, sent only after the debounce window elapses since the last relevant
//! event. Watches directories, not files' own inodes: an atomic-save editor unlinks and
//! recreates the file rather than writing in place, which would silently break an inode-bound
//! watch.
//!
//! `inotify` has no recursive watch mode, so one watch is added per directory, and a `wd ->
//! directory path` map turns an event's bare `WatchDescriptor` back into a full path. A
//! directory created after startup gets its own watch added the moment its `CREATE` arrives, and
//! is walked in case it appeared non-empty (`mkdir -p a/b/c`, an archive extracted in one shot)
//! -- see the `EventMask::ISDIR` arm below for the race that remains despite this.
//!
//! A `path -> hash` map (ADR-0047 decision 3) rejects saves that changed no bytes: editors
//! routinely truncate-and-rewrite a file with unchanged contents, which is an inotify event but
//! not a config change. Not a duplicate of the debounce above: debouncing collapses the burst
//! one real save produces, hashing rejects a "save" with no real change. A deleted file has no
//! bytes to hash and is unconditionally a change.

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

/// Every inotify event kind this watcher acts on: creation, in-place modification, an
/// atomic-save editor's rename-into-place, its rename-away counterpart (also how a plain
/// delete-via-rename looks), an outright delete, and a completed write. Read events (`ACCESS`,
/// `OPEN`, ...) are not requested, so they never reach the loop below.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MODIFY
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::DELETE
        | WatchMask::CLOSE_WRITE
}

/// Adds a watch on `dir` itself, then recurses into every subdirectory it currently contains --
/// so a config tree that already has `widgets/` at startup is fully covered before the first
/// event can arrive, and so is a directory that appears later and gets walked from this same
/// function (see the `ISDIR` arm in `spawn_watcher`). `wd_to_dir` records the directory each new
/// watch covers, since an inotify event carries only a bare `WatchDescriptor` and a name
/// relative to it, never a full path.
///
/// Each call starts its own `visited` set rather than sharing one across the process, so a
/// directory that is deleted and recreated is walked again rather than skipped as already seen.
fn watch_tree(watches: &mut Watches, wd_to_dir: &mut HashMap<WatchDescriptor, PathBuf>, dir: &Path) -> io::Result<()> {
    walk(watches, wd_to_dir, &mut HashSet::new(), dir)
}

/// One directory of [`watch_tree`]'s walk. `visited` holds canonical paths already covered, which
/// stops a symlink pointing at an ancestor from recursing until the stack runs out.
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

    // Failing to descend is reported and skipped, never propagated: the caller at startup passes
    // this straight into the Supervisor's `?`, and one unreadable subdirectory must not stop the
    // shell from starting. Not watching a directory is a worse config experience; refusing to
    // boot over one is a worse bug. The `add` above is still fatal: it's the directory this call
    // was asked to watch.
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("config watcher: cannot list {}: {err}; changes inside it will not reload", dir.display());
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // entry.file_type() calls a symlink a symlink, never a directory -- it would skip
        // `widgets -> ~/dotfiles/oblisk/widgets`, a dotfiles-repo layout, and the config would
        // load fine (Lua's require follows the link) and simply never reload. metadata follows
        // the link; visited above cuts the cycle that opens up.
        if !path.metadata().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Err(err) = walk(watches, wd_to_dir, visited, &path) {
            eprintln!("config watcher: cannot watch {}: {err}; changes inside it will not reload", path.display());
        }
    }
    Ok(())
}

/// Drops everything this watcher remembers about `dir` and the tree beneath it, and reports
/// whether any `.lua` file it had already hashed was among them.
///
/// Needed because a rename is not a delete: moving a directory out sends one `MOVED_FROM` for
/// the directory and no `DELETE` for anything inside it, so without this the watches leak (one
/// per move, against `fs.inotify.max_user_watches`) and the hashes stay keyed on paths that no
/// longer exist -- recreating that path with the same bytes then matches the stale hash and the
/// reload is suppressed. `rm -rf` does not hit this (it sends a `DELETE` per file); `git stash`
/// and `git checkout` do.
///
/// Reports on hashed files, not any `.lua` present: a directory never touched since startup was
/// never hashed, so its removal doesn't trigger here and waits for the next edit.
fn forget_subtree(
    watches: &mut Watches,
    wd_to_dir: &mut HashMap<WatchDescriptor, PathBuf>,
    hashes: &mut HashMap<PathBuf, u64>,
    dir: &Path,
) -> bool {
    let doomed: Vec<WatchDescriptor> = wd_to_dir.iter().filter(|(_, watched)| watched.starts_with(dir)).map(|(wd, _)| wd.clone()).collect();
    for wd in doomed {
        // A failure means the kernel already invalidated this watch (deleted, not moved).
        // Nothing left to remove either way.
        let _ = watches.remove(wd.clone());
        wd_to_dir.remove(&wd);
    }
    let known = hashes.len();
    hashes.retain(|path, _| !path.starts_with(dir));
    known != hashes.len()
}

/// Hashes `path`'s current contents, or `None` if it can no longer be read -- a delete or move
/// that raced ahead of this read. `DefaultHasher` is unspecified across Rust versions and not
/// cryptographic; neither matters, since the hash never leaves this process and only has to tell
/// "same bytes as last time" from "different".
fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Watches `dir`'s whole tree for changes to `.lua` files, debounced by `debounce`. Returns a
/// channel that receives one `()` per settled burst of relevant changes -- see the module doc
/// comment for what "relevant" excludes (non-`.lua` files, and a rewrite that changed no bytes).
pub fn spawn_watcher(dir: &Path, debounce: Duration) -> io::Result<mpsc::UnboundedReceiver<()>> {
    let inotify = Inotify::init()?;
    let mut watches = inotify.watches();
    let mut wd_to_dir = HashMap::new();
    watch_tree(&mut watches, &mut wd_to_dir, dir)?;
    let mut stream = inotify.into_event_stream(vec![0u8; 4096])?;

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // path -> last-seen content hash (ADR-0047 decision 3). Starts empty rather than
        // pre-hashing the tree at startup: the first event for any file always counts as a
        // change, the same answer pre-hashing would give, for less bookkeeping.
        let mut hashes: HashMap<PathBuf, u64> = HashMap::new();

        // An absolute deadline, not a relative sleep(debounce) re-armed on every loop iteration:
        // only a relevant event may push this forward. An irrelevant event still loops this
        // select! back, and a relative sleep reconstructed there would silently restart the
        // window from "now", delaying the trigger for as long as unrelated activity kept arriving.
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(Ok(event)) => {
                            if event.mask.contains(EventMask::IGNORED) {
                                // The directory this watch covered is gone -- the kernel already dropped the
                                // watch; this just stops wd_to_dir accumulating dead entries.
                                wd_to_dir.remove(&event.wd);
                                continue;
                            }

                            let Some(parent) = wd_to_dir.get(&event.wd) else {
                                continue; // an event for a watch already cleaned up above -- ignore stragglers.
                            };
                            let Some(name) = event.name.as_deref() else {
                                continue; // event about the watched directory itself, not an entry inside it.
                            };
                            let path = parent.join(name);

                            if event.mask.contains(EventMask::ISDIR) {
                                if event.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO) {
                                    // A directory appeared after startup: watch it, and walk it in case it
                                    // arrived non-empty (mkdir -p a/b/c, a directory moved in). ponytail: this
                                    // does not close every race -- a file written into the new directory
                                    // between the kernel sending this CREATE and this arm running still slips
                                    // past, unwatched, until something else touches that directory. Closing
                                    // that gap needs a real recursive watcher (the notify crate's
                                    // RecommendedWatcher), more machinery than a hand-edited config directory
                                    // has ever justified.
                                    let _ = watch_tree(&mut watches, &mut wd_to_dir, &path);
                                } else if event.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM)
                                    && forget_subtree(&mut watches, &mut wd_to_dir, &mut hashes, &path)
                                {
                                    deadline = Some(tokio::time::Instant::now() + debounce);
                                }
                                continue; // a directory itself is never a config file.
                            }

                            if path.extension() != Some(OsStr::new("lua")) {
                                continue; // not a config file -- README, shell script, editor swap file, whatever.
                            }

                            if event.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                                // Nothing left to hash, and a deletion is unconditionally a change.
                                hashes.remove(&path);
                                deadline = Some(tokio::time::Instant::now() + debounce);
                                continue;
                            }

                            match hash_file(&path) {
                                Some(hash) if hashes.get(&path) == Some(&hash) => {} // same bytes as last time -- not a real change.
                                Some(hash) => {
                                    hashes.insert(path, hash);
                                    deadline = Some(tokio::time::Instant::now() + debounce);
                                }
                                None => {} // read lost the race with a delete/move that followed right behind this event.
                            }
                        }
                        Some(Err(err)) => {
                            eprintln!("config watcher: inotify read failed: {err}");
                        }
                        None => break, // the inotify fd closed -- nothing left to watch.
                    }
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    deadline = None;
                    if tx.send(()).is_err() {
                        break; // receiver dropped -- nobody's listening any more.
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

    /// A directory moved out sends no DELETE for the files inside it, only one MOVED_FROM for
    /// the directory, so nothing clears their hashes -- see forget_subtree.
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
        // The recreated directory has to be watched before the write, or this races the gap the
        // ISDIR arm's ponytail names, testing that race instead of the hash map.
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::fs::write(dir.path().join("widgets/clock.lua"), "return 1").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "a file recreated under a rebuilt directory has to reload, whatever its bytes");
    }

    /// The layout a dotfiles repository produces -- see walk's own doc comment for why
    /// file_type() would skip it and metadata() is used instead.
    #[tokio::test]
    async fn a_lua_file_inside_a_symlinked_subdirectory_still_fires_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("clock.lua"), "return {}").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("widgets")).unwrap();

        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        std::fs::write(outside.path().join("clock.lua"), "return { changed = true }").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "an edit through a symlinked config subdirectory has to reload");
    }

    /// A symlink pointing at an ancestor is a cycle, and following symlinks without a guard walks
    /// it until the stack runs out. Reaching the assertion at all is the test.
    #[tokio::test]
    async fn a_symlink_cycle_in_the_config_tree_does_not_recurse_forever() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("widgets/loop")).unwrap();

        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the tree still has to be watched after the cycle is cut");
    }

    /// A directory the walk cannot read must not stop the shell from starting -- see walk's own
    /// doc comment.
    #[tokio::test]
    async fn an_unreadable_subdirectory_is_skipped_rather_than_failing_the_whole_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        // Root ignores the mode bits, so there would be nothing to skip and nothing to assert.
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
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "must not fire a second trigger for the same settled write");
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
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "a rapid burst must coalesce into exactly one trigger, not two");
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
        // Regression test: an earlier implementation reconstructed sleep(debounce) on every loop
        // iteration for any inotify event, including an irrelevant one, silently restarting the
        // window. A wider debounce than SHORT_DEBOUNCE gives comfortable margin against jitter.
        let debounce = Duration::from_millis(80);
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), debounce).unwrap();

        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();
        tokio::time::sleep(debounce / 2).await;
        std::fs::write(dir.path().join("notes.txt"), "unrelated").unwrap();

        // The real deadline is `debounce`/2 away from here. Correct behavior fires here at
        // `debounce`/2; a buggy from-here restart would instead fire a full `debounce` later --
        // this window sits with 20ms margin inside the former and short of the latter.
        assert!(
            recv_within(&mut rx, debounce / 2 + Duration::from_millis(20)).await.is_some(),
            "an unrelated event during the debounce window must not delay the trigger"
        );
    }

    #[tokio::test]
    async fn a_write_to_a_non_shell_lua_file_at_the_top_level_still_fires_a_trigger() {
        // Pins Phase 26 item 3: any .lua file matches, not just the literal name shell.lua, since
        // a config can be split across files (ADR-0047).
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("colors.lua"), "return {}").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "any .lua file at the top level must trigger a reload, not just shell.lua");
    }

    #[tokio::test]
    async fn a_write_to_a_lua_file_in_a_subdirectory_present_at_startup_fires_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("widgets/clock.lua"), "return {}").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "a .lua file inside a subdirectory that existed at startup must trigger a reload");
    }

    #[tokio::test]
    async fn a_directory_created_after_startup_is_watched_for_lua_changes() {
        // Requirement 1: a config author adding a widgets/ folder after the watcher started must
        // not need a restart to get reloads from files inside it.
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        // Give the watcher's task a moment to process the directory's CREATE event and register
        // its watch before a file is written into it -- this is the specific race the ISDIR arm's
        // doc comment names as not fully closed; a real editor's save is not this instantaneous.
        tokio::time::sleep(SHORT_DEBOUNCE).await;
        std::fs::write(dir.path().join("widgets/clock.lua"), "return {}").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "a .lua file inside a directory created after startup must trigger a reload");
    }

    #[tokio::test]
    async fn rewriting_a_lua_file_with_identical_contents_fires_no_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(&path, "return {}").unwrap();
        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the first write must fire a trigger");
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "must settle before the next write");

        // An editor's write-truncate-rewrite cycle: same final bytes, still a real inotify
        // MODIFY/CLOSE_WRITE pair, must not count as a change.
        std::fs::write(&path, "return {}").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_none(), "rewriting identical contents must not fire a second trigger");
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
