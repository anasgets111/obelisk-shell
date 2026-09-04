//! [`FilesController`]: the `oblisk.files` state owner, one listing task per watched folder
//! (ADR-0120).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use inotify::{EventMask, Inotify, WatchMask};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// How long after the last inotify event a folder is re-listed. A copy of forty wallpapers is
/// forty `CREATE`/`CLOSE_WRITE` pairs in a burst, and one listing at the end is the point.
const RELIST_DEBOUNCE: Duration = Duration::from_millis(200);

/// `oblisk.files`'s payload (ADR-0120): every watched folder, keyed by the path `watch` was
/// given, so a config reads back `oblisk.files.folders[folder]` with the string it wrote.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct FilesState {
    /// One entry per `files:watch(path)` still in force, keyed by that path with trailing slashes
    /// stripped. Absent until the first `watch`, so a config draws nothing rather than an empty
    /// list for a folder it never asked about.
    pub folders: BTreeMap<String, Folder>,
}

/// One watched folder as the config sees it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct Folder {
    /// `false` between `watch` and the first listing landing, which is the "loading" a picker
    /// draws a spinner for. `true` afterwards, even when `entries` is empty or `error` is set.
    pub ready: bool,
    /// The plain files directly inside the folder, hidden ones (a leading dot) skipped, filtered
    /// to the extensions `watch` named, sorted by name case-insensitively. Not recursive: a
    /// subfolder is not listed and nothing inside it is. Replaced wholesale on every change
    /// inotify reports, debounced, so a copy in progress lands as one update.
    pub entries: Vec<FileEntry>,
    /// Why the last listing produced nothing, in words fit to draw (`"No such file or
    /// directory"`), or absent when it succeeded. Set alongside `ready = true`, so a picker tells a
    /// missing folder from an empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One file in a watched folder.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, schemars::JsonSchema)]
pub struct FileEntry {
    /// The file name alone, `sunrise.jpg`, for drawing and for matching a search against.
    pub name: String,
    /// The absolute path, what `image { source = ... }` takes and what a config stores.
    pub path: String,
    /// Unix epoch seconds of the last modification, for a "newest first" sort. `0` when the
    /// filesystem does not say.
    pub modified: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesSignal {
    Changed,
}

/// One folder's watch: the task that lists it and follows inotify, aborted on `unwatch`, and the
/// extensions it was asked for, so a repeat `watch` with the same filter is a no-op.
struct Watch {
    task: JoinHandle<()>,
    extensions: Vec<String>,
}

/// `Clone` so `main.rs`'s dispatch arm can hand a cheap `Arc`-backed copy around, the same shape
/// `ApplicationsController` has.
#[derive(Clone)]
pub struct FilesController {
    state: Arc<Mutex<FilesState>>,
    watches: Arc<Mutex<HashMap<String, Watch>>>,
    events: UnboundedSender<FilesSignal>,
}

impl FilesController {
    /// Builds the controller with nothing watched. There is no folder to list until a config
    /// names one, so unlike `applications` there is no startup scan.
    pub fn new(events: UnboundedSender<FilesSignal>) -> Self {
        FilesController {
            state: Arc::new(Mutex::new(FilesState::default())),
            watches: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }

    pub fn snapshot(&self) -> FilesState {
        self.state.lock().expect("files state mutex poisoned").clone()
    }

    /// Starts following `path`, or re-pushes the current listing when it is already followed with
    /// the same `extensions`: a generation swap re-evaluates the config, which calls `watch` again,
    /// and the new generation's first read is served from the snapshot either way. A different
    /// filter replaces the watch, since the listing it holds was made under the old one.
    pub fn watch(&self, path: &str, extensions: Vec<String>) {
        let key = folder_key(path);
        {
            let mut watches = self.watches.lock().expect("files watches mutex poisoned");
            if let Some(existing) = watches.get(&key) {
                if existing.extensions == extensions {
                    let _ = self.events.send(FilesSignal::Changed);
                    return;
                }
                existing.task.abort();
                watches.remove(&key);
            }
            {
                let mut state = self.state.lock().expect("files state mutex poisoned");
                state.folders.insert(key.clone(), Folder::default());
            }
            let task = tokio::spawn(follow_folder(
                PathBuf::from(&key),
                key.clone(),
                extensions.clone(),
                Arc::clone(&self.state),
                self.events.clone(),
            ));
            watches.insert(key, Watch { task, extensions });
        }
        let _ = self.events.send(FilesSignal::Changed);
    }

    /// Stops following `path` and drops it from the payload. A path never watched is a no-op.
    pub fn unwatch(&self, path: &str) {
        let key = folder_key(path);
        let removed = self.watches.lock().expect("files watches mutex poisoned").remove(&key);
        let Some(watch) = removed else { return };
        watch.task.abort();
        self.state.lock().expect("files state mutex poisoned").folders.remove(&key);
        let _ = self.events.send(FilesSignal::Changed);
    }
}

/// The payload key for a folder: the path with trailing slashes stripped, so `/walls/` and
/// `/walls` are one watch. `/` itself stays `/`.
pub fn folder_key(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
}

/// Whether `name` passes the `extensions` filter: any file when the list is empty, else a file
/// whose extension, case-folded, is in it. A file with no extension never matches a non-empty
/// list.
pub fn matches_extension(name: &str, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    extensions.contains(&ext)
}

/// One listing of `dir` on [`Folder::entries`]'s terms. Blocking: `read_dir` plus one `stat` per
/// entry, so the caller runs it under `spawn_blocking`.
pub fn list_folder(dir: &Path, extensions: &[String]) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || !matches_extension(name, extensions) {
            continue;
        }
        // `metadata`, not `file_type`, so a symlink to a file lists as the file.
        let Ok(metadata) = entry.path().metadata() else { continue };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_secs() as i64);
        entries.push(FileEntry { name: name.to_string(), path: entry.path().to_string_lossy().into_owned(), modified });
    }
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// Lists `dir` once, stores the result under `key`, and pushes when it differs from what is held.
async fn relist(
    dir: &Path,
    key: &str,
    extensions: &[String],
    state: &Mutex<FilesState>,
    events: &UnboundedSender<FilesSignal>,
) {
    let dir = dir.to_path_buf();
    let extensions = extensions.to_vec();
    let listed = tokio::task::spawn_blocking(move || list_folder(&dir, &extensions)).await;
    let folder = match listed {
        Ok(Ok(entries)) => Folder { ready: true, entries, error: None },
        Ok(Err(err)) => Folder { ready: true, entries: Vec::new(), error: Some(err.to_string()) },
        Err(err) => Folder { ready: true, entries: Vec::new(), error: Some(format!("listing task failed: {err}")) },
    };
    let changed = {
        let mut guard = state.lock().expect("files state mutex poisoned");
        match guard.folders.get(key) {
            // Unwatched while the listing ran: nothing to store.
            None => return,
            Some(current) if *current == folder => false,
            Some(_) => {
                guard.folders.insert(key.to_string(), folder);
                true
            }
        }
    };
    if changed {
        let _ = events.send(FilesSignal::Changed);
    }
}

/// The events that change a listing: an entry appearing, finishing a write, or going away.
/// `MODIFY` is left out since a copy in progress fires it per chunk and `CLOSE_WRITE` marks the
/// end; `DELETE_SELF`/`MOVE_SELF` catch the folder itself going.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::DELETE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
}

/// One folder's whole life under a watch: list it, then re-list after every settled burst of
/// inotify events until the task is aborted or the folder is gone. A folder that cannot be
/// watched (missing, unreadable) is listed once, which records the error, and left there: the
/// upgrade is watching the parent for it to appear, which nothing has asked for.
async fn follow_folder(
    dir: PathBuf,
    key: String,
    extensions: Vec<String>,
    state: Arc<Mutex<FilesState>>,
    events: UnboundedSender<FilesSignal>,
) {
    relist(&dir, &key, &extensions, &state, &events).await;

    let mut stream = match Inotify::init().and_then(|inotify| {
        inotify.watches().add(&dir, watch_mask())?;
        inotify.into_event_stream(vec![0u8; 4096])
    }) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("files: cannot watch {}: {err}; its listing will not follow changes", dir.display());
            return;
        }
    };

    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            event = stream.next() => {
                match event {
                    Some(Ok(event)) => {
                        if event.mask.intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF | EventMask::IGNORED) {
                            // The folder itself went: one last listing records the error.
                            relist(&dir, &key, &extensions, &state, &events).await;
                            return;
                        }
                        deadline = Some(tokio::time::Instant::now() + RELIST_DEBOUNCE);
                    }
                    Some(Err(err)) => eprintln!("files: inotify read on {} failed: {err}", dir.display()),
                    None => return,
                }
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                deadline = None;
                relist(&dir, &key, &extensions, &state, &events).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    #[test]
    fn folder_key_strips_trailing_slashes_and_keeps_root() {
        assert_eq!(folder_key("/walls/"), "/walls");
        assert_eq!(folder_key("/walls//"), "/walls");
        assert_eq!(folder_key("/walls"), "/walls");
        assert_eq!(folder_key("/"), "/");
    }

    #[test]
    fn an_empty_extension_list_matches_everything_and_a_filled_one_folds_case() {
        assert!(matches_extension("a.JPG", &[]));
        assert!(matches_extension("README", &[]));
        let jpg = vec!["jpg".to_string()];
        assert!(matches_extension("a.JPG", &jpg));
        assert!(!matches_extension("a.png", &jpg));
        assert!(!matches_extension("README", &jpg));
    }

    #[test]
    fn a_listing_skips_hidden_files_and_folders_filters_by_extension_and_sorts_by_name_folded() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "b.jpg");
        touch(dir.path(), "A.png");
        touch(dir.path(), "c.txt");
        touch(dir.path(), ".hidden.jpg");
        std::fs::create_dir(dir.path().join("sub.jpg")).unwrap();
        let all = list_folder(dir.path(), &[]).unwrap();
        let names: Vec<&str> = all.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["A.png", "b.jpg", "c.txt"]);
        assert_eq!(all[0].path, dir.path().join("A.png").to_string_lossy());
        assert!(all[0].modified > 0);

        let images = list_folder(dir.path(), &["jpg".to_string(), "png".to_string()]).unwrap();
        let names: Vec<&str> = images.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["A.png", "b.jpg"]);
    }

    #[test]
    fn a_missing_folder_lists_as_an_error_not_a_panic() {
        assert!(list_folder(Path::new("/nonexistent/oblisk-files-test"), &[]).is_err());
    }

    #[tokio::test]
    async fn watch_lists_the_folder_marks_it_ready_and_unwatch_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "one.jpg");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = FilesController::new(tx);
        let key = dir.path().to_string_lossy().into_owned();

        controller.watch(&format!("{key}/"), vec!["jpg".to_string()]);
        let first = controller.snapshot();
        assert_eq!(first.folders[&key], Folder::default(), "not ready until the listing lands");

        // The first push is the `watch` itself; the second is the listing landing.
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        let listed = controller.snapshot();
        let folder = &listed.folders[&key];
        assert!(folder.ready);
        assert_eq!(folder.error, None);
        assert_eq!(folder.entries.len(), 1);
        assert_eq!(folder.entries[0].name, "one.jpg");

        controller.unwatch(&key);
        assert!(controller.snapshot().folders.is_empty());
    }

    #[tokio::test]
    async fn a_repeat_watch_with_the_same_filter_keeps_the_listing_and_a_new_filter_relists() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "one.jpg");
        touch(dir.path(), "two.png");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = FilesController::new(tx);
        let key = dir.path().to_string_lossy().into_owned();

        controller.watch(&key, vec!["jpg".to_string()]);
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        assert_eq!(controller.snapshot().folders[&key].entries.len(), 1);

        controller.watch(&key, vec!["jpg".to_string()]);
        rx.recv().await.unwrap();
        assert_eq!(controller.snapshot().folders[&key].entries.len(), 1, "same filter, listing kept");

        controller.watch(&key, vec![]);
        assert!(!controller.snapshot().folders[&key].ready, "a new filter starts over");
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        assert_eq!(controller.snapshot().folders[&key].entries.len(), 2);
    }

    #[tokio::test]
    async fn a_missing_folder_reads_ready_with_an_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = FilesController::new(tx);
        controller.watch("/nonexistent/oblisk-files-test", vec![]);
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        let folder = &controller.snapshot().folders["/nonexistent/oblisk-files-test"];
        assert!(folder.ready);
        assert!(folder.entries.is_empty());
        assert!(folder.error.is_some());
    }
}
