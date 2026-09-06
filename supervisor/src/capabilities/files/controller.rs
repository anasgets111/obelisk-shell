//! [`FilesController`] owns `oblisk.files`, with one listing task per watched folder (ADR-0120).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use inotify::{EventMask, Inotify, WatchMask};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// Relist 200ms after the last event. Forty wallpapers produce forty `CREATE`/`CLOSE_WRITE` pairs;
/// one listing after the burst is the point.
const RELIST_DEBOUNCE: Duration = Duration::from_millis(200);

/// `oblisk.files`'s payload (ADR-0120): watched folders keyed by the path `watch` was given, so
/// `oblisk.files.folders[folder]` reads back with the string the config wrote.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct FilesState {
    /// One entry per active `files:watch(path)`, keyed by `path` with trailing slashes stripped.
    /// Absent until watched, so an unrequested folder is not an empty list.
    pub folders: BTreeMap<String, Folder>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct Folder {
    /// `false` until the first listing lands, for a picker's loading spinner. `true` thereafter,
    /// even when `entries` is empty or `error` is set.
    pub ready: bool,
    /// Plain files directly inside the folder, skipping dotfiles, filtered to `watch`'s extensions
    /// and sorted case-insensitively. Not recursive. Replaced wholesale after each debounced
    /// inotify burst, so a copy in progress lands as one update.
    pub entries: Vec<FileEntry>,
    /// A drawable listing error such as `"No such file or directory"`, or absent on success. Set
    /// with `ready = true`, distinguishing a missing folder from an empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, schemars::JsonSchema)]
pub struct FileEntry {
    /// File name alone, such as `sunrise.jpg`, for drawing and search.
    pub name: String,
    /// Absolute path for `image { source = ... }` and config storage.
    pub path: String,
    /// Last-modification Unix seconds for newest-first sorting; `0` when unavailable.
    pub modified: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesSignal {
    Changed,
}

/// A folder's listing task and filter. `unwatch` aborts the task; repeating the same filter is a
/// no-op.
struct Watch {
    task: JoinHandle<()>,
    extensions: Vec<String>,
}

#[derive(Clone)]
pub struct FilesController {
    state: Arc<Mutex<FilesState>>,
    watches: Arc<Mutex<HashMap<String, Watch>>>,
    events: UnboundedSender<FilesSignal>,
}

impl FilesController {
    /// Builds with nothing watched. A config names folders on demand, so there is no startup scan.
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

    /// Starts following `path`, or re-pushes the current listing for the same `extensions`. A
    /// generation swap calls `watch` again and reads this snapshot; a different filter replaces
    /// the watch because its listing used the old filter.
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

    /// Stops following `path` and removes it from the payload; an unwatched path is a no-op.
    pub fn unwatch(&self, path: &str) {
        let key = folder_key(path);
        let removed = self.watches.lock().expect("files watches mutex poisoned").remove(&key);
        let Some(watch) = removed else { return };
        watch.task.abort();
        self.state.lock().expect("files state mutex poisoned").folders.remove(&key);
        let _ = self.events.send(FilesSignal::Changed);
    }
}

/// Strips trailing slashes for payload keys, making `/walls/` and `/walls` one watch while keeping
/// `/` as `/`.
pub fn folder_key(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
}

/// An empty filter matches every file; otherwise the case-folded extension must be listed. A
/// file without an extension never matches a non-empty filter.
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

/// Lists `dir` for [`Folder::entries`]. Blocking: `read_dir` plus one `stat` per entry, so callers
/// use `spawn_blocking`.
pub fn list_folder(dir: &Path, extensions: &[String]) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || !matches_extension(name, extensions) {
            continue;
        }
        // `metadata`, not `file_type`, includes symlinks to files.
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
            // The folder was unwatched while listing ran.
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

/// Listing events: entries appearing, finishing writes, or going away. Exclude `MODIFY`, which a
/// copy emits per chunk; `CLOSE_WRITE` marks its end. `DELETE_SELF`/`MOVE_SELF` catch folder loss.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::DELETE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
}

/// Lists a folder, then relists after each settled inotify burst until abort or folder loss. A
/// missing/unreadable folder is listed once to record the error and left there. ponytail: watching
/// the parent for it to appear is the upgrade path, but no caller asks for it.
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
                            // Record the folder's disappearance in one last listing.
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

        // First push is `watch`; second is the listing landing.
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
