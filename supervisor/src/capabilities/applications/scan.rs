//! The applications-directory scan (ADR-0061): which directories, in what precedence, and
//! how an `app_id` finds its entry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::entry::{desktop_file_id, flag, parse_group, tokenize_exec};

/// Walk depth under an applications directory. Subdirectories are allowed and real trees use one
/// level (`kde4/`); the cap bounds symlink loops without canonicalizing every directory as
/// `watcher.rs` does.
const MAX_DEPTH: usize = 4;

/// One application as config sees it (ADR-0061). Display data only: argv stays private because
/// `applications:launch(id)` runs it, and exposing it would let config rewrite the command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct AppSummary {
    /// Desktop file id (`org.telegram.desktop`), and `launch`'s argument.
    pub id: String,
    /// Unlocalized `Name=`. `Name[xx]` is not read (ADR-0061), so this is English on a localized
    /// system.
    pub name: String,
    /// `Icon=` as written, either a theme name or absolute path; `icon { name = ... }` accepts
    /// both (ADR-0054 decision 2). `None` means no `Icon=` key, distinct from failed resolution.
    pub icon: Option<String>,
    /// Unlocalized `Comment=`, the one-line description/search text under the name, e.g.
    /// `"Web Browser"` under `Firefox` (ADR-0112). `None` means no key, so config can hide it;
    /// unlike `name`, this field is localized.
    pub comment: Option<String>,
}

/// What `launch` needs but Lua never sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    pub command: String,
    pub args: Vec<String>,
    /// `Terminal=true`: wrap this console program in an emulator.
    pub terminal: bool,
}

/// One scan's result.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Entries sorted by display name. Sorting in Lua would repeat work on every reload.
    pub entries: Vec<AppSummary>,
    /// `app_id` to entry, for callers holding `workspaces.active_client.class` or a tray item
    /// with no icon of its own rather than a desktop id.
    pub by_app_id: BTreeMap<String, AppSummary>,
    pub launch: HashMap<String, LaunchTarget>,
}

/// Applications directories in XDG precedence order.
///
/// Applies both defaults: plain login shells often unset `XDG_DATA_HOME` and `XDG_DATA_DIRS`, and
/// skipping `/usr/share` in that case finds nothing on a normal system.
///
/// Injected, like `SystemController::new`, so precedence is testable without changing the process
/// environment.
pub fn application_dirs(data_home: Option<PathBuf>, data_dirs: Option<String>, home: &Path) -> Vec<PathBuf> {
    let home_dir = data_home.unwrap_or_else(|| home.join(".local/share"));
    let dirs = data_dirs.filter(|value| !value.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    std::iter::once(home_dir)
        .chain(dirs.split(':').filter(|part| !part.is_empty()).map(PathBuf::from))
        .map(|dir| dir.join("applications"))
        .collect()
}

/// Scans `dirs` in order; the first occurrence of each desktop id wins.
///
/// First-wins is the specification's rule, so `~/.local/share/applications` comes first: a same-
/// named home file overrides a system name, icon, or command. Last-wins would ignore the override.
pub fn scan(dirs: &[PathBuf]) -> ScanResult {
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<AppSummary> = Vec::new();
    let mut launch: HashMap<String, LaunchTarget> = HashMap::new();
    // Keep beside the summary until the map is built; `StartupWMClass` is only a matching key.
    let mut wm_classes: Vec<(String, Option<String>)> = Vec::new();

    for dir in dirs {
        let mut files = Vec::new();
        collect_desktop_files(dir, 0, &mut files);
        // Filesystem directory order is unspecified; sort so duplicate ids have a stable winner.
        files.sort();
        for path in files {
            let Some(id) = desktop_file_id(&path, dir) else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue; // a higher-precedence directory already provided this id.
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue; // unreadable entry: one missing app, never a failed scan.
            };
            let Some(group) = parse_group(&contents) else {
                continue; // no `[Desktop Entry]` group at all.
            };
            // `Type` has no `Application` default; a `Link` or `Directory` is not launchable.
            if group.get("Type").map(String::as_str) != Some("Application") {
                continue;
            }
            if flag(&group, "NoDisplay") || flag(&group, "Hidden") {
                continue;
            }
            let (Some(name), Some(exec)) = (group.get("Name"), group.get("Exec")) else {
                continue;
            };
            let Some((command, args)) = tokenize_exec(exec) else {
                continue;
            };
            launch.insert(id.clone(), LaunchTarget { command, args, terminal: flag(&group, "Terminal") });
            wm_classes.push((id.clone(), group.get("StartupWMClass").cloned()));
            entries.push(AppSummary {
                id,
                name: name.clone(),
                icon: group.get("Icon").cloned(),
                comment: group.get("Comment").cloned(),
            });
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let by_app_id = build_app_id_map(&entries, &wm_classes);
    ScanResult { entries, by_app_id, launch }
}

/// Ways an `app_id` finds an entry, strongest first.
///
/// Two passes keep a weak key from one entry from beating a strong key from another. Entry-by-entry
/// insertion would make `org.gnome.Nautilus` depend on scan order instead of its filename.
///
/// The last-segment rule covers bare ids: niri reports Nautilus as `org.gnome.Nautilus`, and
/// Files ships that exact filename, but many apps report `nautilus` against a reverse-DNS name.
fn build_app_id_map(entries: &[AppSummary], wm_classes: &[(String, Option<String>)]) -> BTreeMap<String, AppSummary> {
    let by_id: HashMap<&str, &AppSummary> = entries.iter().map(|entry| (entry.id.as_str(), entry)).collect();
    let mut map: BTreeMap<String, AppSummary> = BTreeMap::new();

    // Pass 1, exact: `StartupWMClass` is the specification's key for this question, so it outranks
    // the filename even when they usually agree.
    for (id, wm_class) in wm_classes {
        let Some(entry) = by_id.get(id.as_str()) else { continue };
        if let Some(class) = wm_class.as_deref().filter(|class| !class.is_empty()) {
            map.entry(class.to_string()).or_insert_with(|| (*entry).clone());
        }
    }
    for entry in entries {
        map.entry(entry.id.clone()).or_insert_with(|| entry.clone());
    }

    // Pass 2, case-folded and shortened. These are guesses and cannot displace exact keys.
    for (id, wm_class) in wm_classes {
        let Some(entry) = by_id.get(id.as_str()) else { continue };
        if let Some(class) = wm_class.as_deref().filter(|class| !class.is_empty()) {
            map.entry(class.to_lowercase()).or_insert_with(|| (*entry).clone());
        }
    }
    for entry in entries {
        map.entry(entry.id.to_lowercase()).or_insert_with(|| entry.clone());
        if let Some(last) = entry.id.rsplit('.').next().filter(|last| *last != entry.id) {
            map.entry(last.to_lowercase()).or_insert_with(|| entry.clone());
        }
    }
    map
}

/// Every depth-capped `.desktop` file under `dir`. A missing directory contributes nothing; most
/// systems lack `/usr/local/share/applications`.
fn collect_desktop_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        // Use `metadata` so symlinked entries are followed; packaged application trees such as
        // Flatpak exports commonly link entries from elsewhere.
        let Ok(meta) = path.metadata() else { continue };
        if meta.is_dir() {
            collect_desktop_files(&path, depth + 1, out);
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_entry(dir: &Path, file: &str, body: &str) {
        if let Some(parent) = dir.join(file).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn application(name: &str, extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec=/usr/bin/{name}\n{extra}")
    }

    /// Both XDG defaults matter: a plain login shell may set neither, and skipping `/usr/share`
    /// when `XDG_DATA_DIRS` is unset finds nothing on a normal system.
    #[test]
    fn application_dirs_applies_both_xdg_defaults_when_the_environment_is_bare() {
        let dirs = application_dirs(None, None, Path::new("/home/someone"));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/someone/.local/share/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
    }

    #[test]
    fn application_dirs_puts_the_users_own_directory_first_so_an_override_wins() {
        let dirs = application_dirs(
            Some(PathBuf::from("/custom/data")),
            Some("/a:/b".to_string()),
            Path::new("/home/someone"),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/custom/data/applications"),
                PathBuf::from("/a/applications"),
                PathBuf::from("/b/applications"),
            ]
        );
    }

    #[test]
    fn application_dirs_treats_an_empty_data_dirs_as_unset_rather_than_as_no_directories() {
        let dirs = application_dirs(None, Some(String::new()), Path::new("/home/someone"));
        assert!(
            dirs.contains(&PathBuf::from("/usr/share/applications")),
            "an empty XDG_DATA_DIRS must fall back to the default"
        );
    }

    /// A user's copy replaces the system entry, allowing a renamed app or changed command;
    /// last-wins would ignore it.
    #[test]
    fn scan_lets_an_earlier_directory_win_the_same_desktop_file_id() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write_entry(home.path(), "editor.desktop", &application("Mine", ""));
        write_entry(system.path(), "editor.desktop", &application("Theirs", ""));

        let result = scan(&[home.path().to_path_buf(), system.path().to_path_buf()]);

        assert_eq!(result.entries.len(), 1, "one id is one entry however many directories carry it");
        assert_eq!(result.entries[0].name, "Mine");
    }

    #[test]
    fn scan_skips_entries_the_specification_says_not_to_show() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "shown.desktop", &application("Shown", ""));
        write_entry(dir.path(), "hidden.desktop", &application("Hidden", "Hidden=true\n"));
        write_entry(dir.path(), "nodisplay.desktop", &application("NoDisplay", "NoDisplay=true\n"));
        write_entry(dir.path(), "link.desktop", "[Desktop Entry]\nType=Link\nName=Link\nURL=http://x\n");
        write_entry(dir.path(), "noexec.desktop", "[Desktop Entry]\nType=Application\nName=NoExec\n");

        let names: Vec<String> = scan(&[dir.path().to_path_buf()]).entries.into_iter().map(|e| e.name).collect();

        assert_eq!(names, vec!["Shown"]);
    }

    #[test]
    fn scan_sorts_entries_by_display_name_so_a_launcher_needs_no_sort_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "c.desktop", &application("Cherry", ""));
        write_entry(dir.path(), "a.desktop", &application("Apple", ""));
        write_entry(dir.path(), "b.desktop", &application("Banana", ""));

        let names: Vec<String> = scan(&[dir.path().to_path_buf()]).entries.into_iter().map(|e| e.name).collect();

        assert_eq!(names, vec!["Apple", "Banana", "Cherry"]);
    }

    #[test]
    fn scan_keeps_the_parsed_argv_out_of_the_entry_a_config_can_read() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(
            dir.path(),
            "browser.desktop",
            "[Desktop Entry]\nType=Application\nName=Browser\nExec=firefox --new-tab %u\n",
        );

        let result = scan(&[dir.path().to_path_buf()]);

        let target = result.launch.get("browser").expect("the launch map carries the argv");
        assert_eq!(target.command, "firefox");
        assert_eq!(target.args, vec!["--new-tab".to_string()], "the field code must not reach argv");
        let serialized = serde_json::to_value(&result.entries[0]).unwrap();
        assert!(
            serialized.get("exec").is_none() && serialized.get("command").is_none(),
            "no argv may appear in the Lua-visible payload"
        );
    }

    #[test]
    fn an_app_id_matching_a_desktop_file_id_exactly_finds_its_entry() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "org.gnome.Nautilus.desktop", &application("Files", ""));

        let map = scan(&[dir.path().to_path_buf()]).by_app_id;

        assert_eq!(map.get("org.gnome.Nautilus").map(|e| e.name.as_str()), Some("Files"));
    }

    /// Common mismatch: a toplevel reports `nautilus` while the file is
    /// `org.gnome.Nautilus.desktop`.
    #[test]
    fn an_app_id_matching_only_the_last_segment_of_a_reverse_dns_id_still_finds_its_entry() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "org.gnome.Nautilus.desktop", &application("Files", ""));

        let map = scan(&[dir.path().to_path_buf()]).by_app_id;

        assert_eq!(map.get("nautilus").map(|e| e.name.as_str()), Some("Files"));
    }

    #[test]
    fn startup_wm_class_is_matched_because_it_is_the_key_written_for_this_question() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "tg.desktop", &application("Telegram", "StartupWMClass=TelegramDesktop\n"));

        let map = scan(&[dir.path().to_path_buf()]).by_app_id;

        assert_eq!(map.get("TelegramDesktop").map(|e| e.name.as_str()), Some("Telegram"));
    }

    /// Two passes prevent one entry's case-folded guess from displacing another's exact filename;
    /// one pass would make the winner depend on read order.
    #[test]
    fn an_exact_id_beats_another_entrys_case_folded_wm_class() {
        let dir = tempfile::tempdir().unwrap();
        // Sort first; a single pass could let a guess claim "zed" before the real entry.
        write_entry(dir.path(), "aaa.desktop", &application("Impostor", "StartupWMClass=Zed\n"));
        write_entry(dir.path(), "zed.desktop", &application("Zed Editor", ""));

        let map = scan(&[dir.path().to_path_buf()]).by_app_id;

        assert_eq!(
            map.get("zed").map(|e| e.name.as_str()),
            Some("Zed Editor"),
            "an exact desktop file id outranks a lowercased WM class"
        );
        assert_eq!(
            map.get("Zed").map(|e| e.name.as_str()),
            Some("Impostor"),
            "the exact WM class still resolves to the entry declaring it"
        );
    }

    #[test]
    fn a_missing_applications_directory_contributes_nothing_rather_than_failing_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "real.desktop", &application("Real", ""));

        let result = scan(&[PathBuf::from("/nonexistent/applications"), dir.path().to_path_buf()]);

        assert_eq!(
            result.entries.len(),
            1,
            "most systems have no /usr/local/share/applications and that is not an error"
        );
    }

    #[test]
    fn scan_descends_into_a_subdirectory_and_keys_it_by_the_specifications_dashed_id() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "kde4/konsole.desktop", &application("Konsole", ""));

        let result = scan(&[dir.path().to_path_buf()]);

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].id, "kde4-konsole");
    }
}
