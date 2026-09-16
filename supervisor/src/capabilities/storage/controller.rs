//! [`StorageController`] owns JSON files declared with `persistent_table`, keyed by absolute path
//! (ADR-0136).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// Delay after the last write before rewriting. Scroll offsets and search drafts can call `:set()`
/// per keystroke; each otherwise serializes, writes, and renames.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(1000);

/// `obelisk.storage`'s payload (ADR-0136).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StorageState {
    /// One entry per declared `persistent_table`, keyed by the absolute `path` joined from `path`
    /// and `name`. Absent until declared, so unopened files read as `nil`, not an empty table.
    pub files: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageSignal {
    Changed,
}

pub struct StorageController {
    state: Arc<Mutex<StorageState>>,
    /// One pending save per file, replaced by its next write. Files debounce independently; one
    /// file written every second cannot starve another's save.
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

    /// `persistent_table { path, name, defaults }`, re-sent each evaluation (ADR-0136 decision 1).
    /// The first declaration reads the file; later ones use the newer in-memory copy.
    ///
    /// `defaults` fills missing keys without overwriting stored ones, so adding a default is a new
    /// key, not a reset. A changed merge schedules a save, creating the file on first run.
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
            self.schedule_save(path);
        }
        let _ = self.signal_tx.send(StorageSignal::Changed);
    }

    /// `store:set(key, value)` (ADR-0136 decision 2): stores one key, pushes immediately for the
    /// next resolve, and saves after writes stop.
    ///
    /// JSON `null` deletes the key; this is how Lua `nil` arrives.
    ///
    /// A write that changes nothing publishes nothing, sparing a whole-store snapshot and the
    /// renderer it would dirty; configs need no equality guard of their own around each write.
    ///
    /// The save stays unconditional: rewriting is the only repair for the missing, unreadable or
    /// malformed file [`load`] represented as empty.
    pub fn set(&self, path: &str, key: &str, value: serde_json::Value) {
        let Some(path) = absolute_path(path) else {
            eprintln!("storage: refused a write to {path:?}; a store's path must be absolute");
            return;
        };
        if key.is_empty() {
            eprintln!("storage: refused a write to {}; a key cannot be empty", path.display());
            return;
        }

        let changed = {
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
                serde_json::Value::Null => map.remove(key).is_some(),
                value if map.get(key) == Some(&value) => false,
                value => {
                    map.insert(key.to_string(), value);
                    true
                }
            }
        };

        self.schedule_save(path);
        if changed {
            let _ = self.signal_tx.send(StorageSignal::Changed);
        }
    }

    /// Replaces this file's pending save with one [`SAVE_DEBOUNCE`] away.
    ///
    /// ponytail: a save still in the window at session end is lost. The mirror's `saveTimer` has
    /// the same one-second hole. Upgrade with a Supervisor shutdown flush shared by controllers.
    fn schedule_save(&self, path: PathBuf) {
        let state = Arc::clone(&self.state);
        let key = path.to_string_lossy().into_owned();
        let target = path.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(SAVE_DEBOUNCE).await;
            let Some(contents) = state.lock().expect("storage state mutex poisoned").files.get(&key).cloned() else {
                return;
            };
            // Off the Supervisor loop: directory creation, write, and rename are three syscalls,
            // potentially on a spun-down disk or NFS mount.
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(err) = save(&target, &contents) {
                    eprintln!("storage: could not save {}: {err}", target.display());
                }
            })
            .await;
        });
        if let Some(previous) = self.saves.lock().expect("storage saves mutex poisoned").insert(path, handle) {
            previous.abort();
        }
    }
}

/// The named path, or `None` when relative. Relative paths use the unset Supervisor working
/// directory and would land somewhere neither the author nor next session can name
/// (ADR-0136 decision 6).
fn absolute_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

/// All failures become an empty object: missing is normal on first run, and unreadable/malformed
/// data must not stop the shell. [`StorageController::open`] then merges defaults, and the next
/// save rewrites the file as the only repair a config can request.
fn load(path: &Path) -> serde_json::Value {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Copies missing top-level keys from `defaults` into `stored` and reports whether it changed.
/// Values, including tables, replace as one key; nested tables do not merge.
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

/// Writes pretty JSON to a same-directory temporary file, then renames atomically within one
/// filesystem. A crash cannot leave a half-written file; a truncated file would load empty next
/// boot.
fn save(path: &Path, contents: &serde_json::Value) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other(format!("{} has no parent directory", path.display())));
    };
    std::fs::create_dir_all(dir)?;
    let mut serialized = serde_json::to_vec_pretty(contents).map_err(std::io::Error::other)?;
    serialized.push(b'\n');

    // The temporary has to be a name no declared table can also be. `Path::with_extension` was
    // worse than it looked: it derives from the *stem*, so `notes.json` and `notes.db` shared one
    // `notes.json.tmp` and either rename could publish the other's bytes. Appending alone is not
    // enough either -- a config declaring both `foo` and `foo.tmp` would have the first table's
    // temporary land on the second table's file. The leading dot and the pid together are outside
    // what `persistent_table` hands us, and the pid keeps a second supervisor off this one's
    // temporary.
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
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
        // Reload: an author adds a default after the config already ran.
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
    async fn a_write_that_changes_nothing_pushes_nothing_and_still_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        // Read an existing file with no defaults to merge, so `open` schedules no save of its own
        // and only the writes below can recreate the file removed here.
        std::fs::write(&path, r#"{"fit":"cover"}"#).unwrap();
        let (controller, mut rx) = controller();
        controller.open(&path, &json!({}));
        while rx.try_recv().is_ok() {}
        std::fs::remove_file(&path).unwrap();

        controller.set(&path, "fit", json!("cover"));
        controller.set(&path, "never-stored", serde_json::Value::Null);
        assert!(rx.try_recv().is_err(), "a write that edits nothing must not push the whole store");

        tokio::time::sleep(SAVE_DEBOUNCE + Duration::from_millis(150)).await;
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("an unchanged write still repairs the file"))
                .unwrap();
        assert_eq!(written["fit"], json!("cover"));

        controller.set(&path, "fit", json!("fill"));
        assert!(rx.try_recv().is_ok(), "a real edit still pushes");
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
