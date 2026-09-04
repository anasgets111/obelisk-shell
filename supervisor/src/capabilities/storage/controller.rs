//! [`StorageController`]: every JSON file a config declared with `persistent_table`, keyed by the
//! absolute path it named (ADR-0136).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// How long after the last write a file is rewritten. A scroll offset or a search draft is one
/// `:set()` per keystroke, and each one is a serialize, a write and a rename without this.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(1000);

/// `oblisk.storage`'s payload (ADR-0136).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct StorageState {
    /// One entry per `persistent_table` a config declared, keyed by the absolute path it joined
    /// from `path` and `name`. Absent until that declaration is seen, so a config reads `nil`
    /// rather than an empty table for a file nobody opened.
    pub files: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageSignal {
    Changed,
}

pub struct StorageController {
    state: Arc<Mutex<StorageState>>,
    /// One pending save per file, aborted and replaced by the next write to it. Not a single
    /// writer task: two files debounce independently, and a config that writes one every second
    /// must not keep the other's save from ever landing.
    saves: Mutex<HashMap<PathBuf, JoinHandle<()>>>,
    signal_tx: UnboundedSender<StorageSignal>,
}

impl StorageController {
    pub fn new(signal_tx: UnboundedSender<StorageSignal>) -> Self {
        Self { state: Arc::new(Mutex::new(StorageState::default())), saves: Mutex::new(HashMap::new()), signal_tx }
    }

    pub fn snapshot(&self) -> StorageState {
        self.state.lock().expect("storage state mutex poisoned").clone()
    }

    /// `persistent_table { path, name, defaults }`'s declaration, re-sent by every evaluation
    /// (ADR-0136 decision 1). The first one for a path reads the file; later ones do not, since
    /// the in-memory copy is the newer of the two by then.
    ///
    /// `defaults` fills keys the file does not have and never overwrites one it does, so adding a
    /// default to a config that has already run is a new key rather than a reset. A merge that
    /// changed anything schedules a save, which is what creates the file on a first run.
    pub fn open(&self, path: &str, defaults: &serde_json::Value) {
        let Some(path) = absolute_path(path) else {
            eprintln!("storage: refused to open {path:?}; a store's path must be absolute");
            return;
        };
        let key = path.to_string_lossy().into_owned();

        let changed = {
            let mut guard = self.state.lock().expect("storage state mutex poisoned");
            let stored = guard.files.entry(key).or_insert_with(|| load(&path));
            fill_missing(stored, defaults)
        };

        if changed {
            self.schedule_save(&path);
        }
        let _ = self.signal_tx.send(StorageSignal::Changed);
    }

    /// `store:set(key, value)` (ADR-0136 decision 2): stores one key, pushes immediately so the
    /// config sees its own write on the next resolve, and saves once the writes stop.
    ///
    /// A JSON `null` deletes the key, which is how a Lua `nil` arrives here.
    pub fn set(&self, path: &str, key: &str, value: serde_json::Value) {
        let Some(path) = absolute_path(path) else {
            eprintln!("storage: refused a write to {path:?}; a store's path must be absolute");
            return;
        };
        if key.is_empty() {
            eprintln!("storage: refused a write to {}; a key cannot be empty", path.display());
            return;
        }

        {
            let mut guard = self.state.lock().expect("storage state mutex poisoned");
            let Some(stored) = guard.files.get_mut(&*path.to_string_lossy()) else {
                eprintln!("storage: refused a write to {}; no persistent_table declared it", path.display());
                return;
            };
            let Some(map) = stored.as_object_mut() else {
                eprintln!("storage: refused a write to {}; the file does not hold an object", path.display());
                return;
            };
            match value {
                serde_json::Value::Null => map.remove(key),
                value => map.insert(key.to_string(), value),
            };
        }

        self.schedule_save(&path);
        let _ = self.signal_tx.send(StorageSignal::Changed);
    }

    /// Replaces this file's pending save with one [`SAVE_DEBOUNCE`] away.
    ///
    /// ponytail: a save still in the window when the session ends is lost, since nothing flushes
    /// on the way out. The mirror's `saveTimer` has the same hole and the same one-second window.
    /// The upgrade is a flush on the Supervisor's shutdown path, which is where every controller
    /// would want one and where none has one yet.
    fn schedule_save(&self, path: &Path) {
        let state = Arc::clone(&self.state);
        let key = path.to_string_lossy().into_owned();
        let target = path.to_path_buf();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(SAVE_DEBOUNCE).await;
            let Some(contents) = state.lock().expect("storage state mutex poisoned").files.get(&key).cloned() else {
                return;
            };
            // Off the Supervisor's own loop: `create_dir_all` plus a write plus a rename is three
            // syscalls on a filesystem that can be a spun-down disk or an NFS mount.
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(err) = save(&target, &contents) {
                    eprintln!("storage: could not save {}: {err}", target.display());
                }
            })
            .await;
        });
        if let Some(previous) =
            self.saves.lock().expect("storage saves mutex poisoned").insert(path.to_path_buf(), handle)
        {
            previous.abort();
        }
    }
}

/// The path a config named, or `None` when it is not absolute. A relative path resolves against
/// the Supervisor's working directory, which nothing sets, so it would land somewhere neither the
/// config author nor the next session can name (ADR-0136 decision 6).
fn absolute_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

/// Every failure collapses to an empty object rather than an error: a missing file is the ordinary
/// first run, and an unreadable or malformed one must not stop a shell from starting. The defaults
/// [`StorageController::open`] merges in are then the whole table, and the next save rewrites the
/// file, which is the only repair a config could have asked for anyway.
fn load(path: &Path) -> serde_json::Value {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Copies every key of `defaults` that `stored` does not already have, and answers whether it
/// copied any. Top level only: a table value is one key's value, replaced whole, so a nested
/// default does not merge into a nested stored table.
fn fill_missing(stored: &mut serde_json::Value, defaults: &serde_json::Value) -> bool {
    let (Some(stored), Some(defaults)) = (stored.as_object_mut(), defaults.as_object()) else {
        return false;
    };
    let mut changed = false;
    for (key, value) in defaults {
        if !stored.contains_key(key) {
            stored.insert(key.clone(), value.clone());
            changed = true;
        }
    }
    changed
}

/// Writes pretty JSON through a temporary file in the same directory and a rename, which is atomic
/// on any single filesystem: a debounced writer must not be able to leave a half-written file
/// behind a crash, since the next boot reads whatever is there and a truncated file loads as an
/// empty table.
fn save(path: &Path, contents: &serde_json::Value) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other(format!("{} has no parent directory", path.display())));
    };
    std::fs::create_dir_all(dir)?;
    let mut serialized = serde_json::to_vec_pretty(contents).map_err(std::io::Error::other)?;
    serialized.push(b'\n');

    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, &serialized)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn controller() -> (StorageController, tokio::sync::mpsc::UnboundedReceiver<StorageSignal>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StorageController::new(tx), rx)
    }

    #[tokio::test]
    async fn defaults_fill_a_file_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();

        controller.open(&path, &json!({ "theme": "mocha", "dnd": false }));

        assert_eq!(controller.snapshot().files[&path], json!({ "theme": "mocha", "dnd": false }));
    }

    #[tokio::test]
    async fn a_stored_key_wins_over_a_default_and_a_new_default_is_added_beside_it() {
        // The reload case: an author adds a default to a config that has already run once.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "theme": "latte" }"#).unwrap();
        let path = path.to_string_lossy().into_owned();
        let (controller, _rx) = controller();

        controller.open(&path, &json!({ "theme": "mocha", "dnd": true }));

        assert_eq!(controller.snapshot().files[&path], json!({ "theme": "latte", "dnd": true }));
    }

    #[tokio::test]
    async fn a_reopen_does_not_re_read_the_file_under_a_live_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();
        controller.open(&path, &json!({ "theme": "mocha" }));

        controller.set(&path, "theme", json!("latte"));
        controller.open(&path, &json!({ "theme": "mocha" }));

        assert_eq!(
            controller.snapshot().files[&path]["theme"],
            json!("latte"),
            "the in-memory copy is newer than the disk one by the time an evaluation re-declares it"
        );
    }

    #[tokio::test]
    async fn a_write_takes_a_table_and_a_nil_deletes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();
        controller.open(&path, &json!({}));

        controller.set(&path, "wallpaper", json!({ "path": "/w/1.jpg", "fit": "cover" }));
        assert_eq!(controller.snapshot().files[&path]["wallpaper"]["fit"], json!("cover"));

        controller.set(&path, "wallpaper", serde_json::Value::Null);
        assert_eq!(controller.snapshot().files[&path], json!({}));
    }

    #[tokio::test]
    async fn a_write_to_a_file_no_config_declared_is_refused() {
        let (controller, _rx) = controller();

        controller.set("/tmp/never-opened.json", "key", json!(1));

        assert!(controller.snapshot().files.is_empty());
    }

    #[tokio::test]
    async fn a_relative_path_is_refused_rather_than_resolved_against_the_working_directory() {
        let (controller, _rx) = controller();

        controller.open("settings.json", &json!({ "theme": "mocha" }));

        assert!(controller.snapshot().files.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_write_of_a_burst_is_the_one_that_reaches_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("state.json");
        let path = file.to_string_lossy().into_owned();
        let (controller, _rx) = controller();
        controller.open(&path, &json!({}));

        for value in 1..=5 {
            controller.set(&path, "scroll", json!(value));
        }
        assert!(!file.exists(), "nothing is written while the writes are still coming");

        tokio::time::sleep(SAVE_DEBOUNCE * 2).await;
        tokio::task::yield_now().await;

        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written, json!({ "scroll": 5 }));
    }

    #[tokio::test(start_paused = true)]
    async fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();

        controller.open(&path, &json!({ "a": 1 }));
        tokio::time::sleep(SAVE_DEBOUNCE * 2).await;
        tokio::task::yield_now().await;

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "the rename is the write; nothing else may survive it: {leftovers:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn what_was_saved_reads_back_through_a_fresh_controller() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        let (first, _rx) = controller();
        first.open(&path, &json!({}));
        first.set(&path, "theme", json!("latte"));
        tokio::time::sleep(SAVE_DEBOUNCE * 2).await;
        tokio::task::yield_now().await;

        let (second, _rx) = controller();
        second.open(&path, &json!({ "theme": "mocha" }));

        assert_eq!(second.snapshot().files[&path]["theme"], json!("latte"), "the round trip is the contract");
    }
}
