//! The applications-directory scan (ADR-0061): which directories, in what precedence, and
//! how an `app_id` finds its entry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::entry::{desktop_file_id, flag, parse_group, tokenize_exec};

/// How deep a walk goes under an applications directory. The specification allows
/// subdirectories and real trees use one level (`kde4/`), so this is slack rather than a limit
/// anyone reaches. It exists to bound a symlink loop without the cost of canonicalizing every
/// directory the way `watcher.rs` has to.
const MAX_DEPTH: usize = 4;

/// One application as the config sees it (ADR-0061). Deliberately the display half only:
/// the argv never crosses into Lua, because `applications:launch(id)` is what runs it and a
/// config that could rewrite a command line before it ran would be a config that could be made
/// to run something else.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct AppSummary {
    /// The desktop file id (`org.telegram.desktop`), and `launch`'s one argument.
    pub id: String,
    /// The unlocalized `Name=`. `Name[xx]` is deliberately not read (ADR-0061), so this is English
    /// on a localized system.
    pub name: String,
    /// The `Icon=` key as written: a theme name, or an absolute path. `icon { name = ... }`
    /// takes either, which is exactly what ADR-0054 decision 2 built it for. `None` for an
    /// entry with no `Icon=` at all, so a config can tell "no icon" from an icon that failed to
    /// resolve.
    pub icon: Option<String>,
}

/// What `launch` needs and Lua never sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    pub command: String,
    pub args: Vec<String>,
    /// `Terminal=true`: the entry is a console program and needs an emulator wrapped around it.
    pub terminal: bool,
}

/// Everything one scan produces.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Sorted by display name, because a launcher shows this in order and sorting it in Lua
    /// would re-sort it on every reload for a result that never changes between scans.
    pub entries: Vec<AppSummary>,
    /// `app_id` to entry, for a caller holding a window's `app_id` rather than a desktop file id
    /// (`workspaces.active_client.class`, a tray item with no icon of its own).
    pub by_app_id: BTreeMap<String, AppSummary>,
    pub launch: HashMap<String, LaunchTarget>,
}

/// The applications directories, in precedence order, per the XDG base directory specification.
///
/// Both defaults are applied here rather than assumed present: `XDG_DATA_HOME` and
/// `XDG_DATA_DIRS` are unset on a plain login shell far more often than not, and a scan that
/// skipped the default `/usr/share` when they were would find nothing at all on a normal system.
///
/// Injected rather than read from the environment inside, the shape `SystemController::new`
/// already uses, so the precedence is testable without touching the process environment.
pub fn application_dirs(data_home: Option<PathBuf>, data_dirs: Option<String>, home: &Path) -> Vec<PathBuf> {
    let home_dir = data_home.unwrap_or_else(|| home.join(".local/share"));
    let dirs = data_dirs.filter(|value| !value.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    std::iter::once(home_dir)
        .chain(dirs.split(':').filter(|part| !part.is_empty()).map(PathBuf::from))
        .map(|dir| dir.join("applications"))
        .collect()
}

/// Scans `dirs` in order, first occurrence of a desktop file id winning.
///
/// First-wins is the specification's rule and it is why the user's own
/// `~/.local/share/applications` comes first in [`application_dirs`]: overriding a system entry
/// by dropping a file with the same name into the home directory is how a user changes an
/// application's name, icon or command, and a last-wins scan would silently ignore every one of
/// those overrides.
pub fn scan(dirs: &[PathBuf]) -> ScanResult {
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<AppSummary> = Vec::new();
    let mut launch: HashMap<String, LaunchTarget> = HashMap::new();
    // Kept beside the summary only until the map is built, since `StartupWMClass` is a matching
    // key rather than anything the config reads.
    let mut wm_classes: Vec<(String, Option<String>)> = Vec::new();

    for dir in dirs {
        let mut files = Vec::new();
        collect_desktop_files(dir, 0, &mut files);
        // Directory order is whatever the filesystem hands back, so a tree with two files
        // resolving to one id would otherwise pick a different winner run to run.
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
            // `Type` defaults to nothing rather than to `Application`: the specification requires
            // the key, and a `Link` or `Directory` entry is not something a launcher can run.
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
            entries.push(AppSummary { id, name: name.clone(), icon: group.get("Icon").cloned() });
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let by_app_id = build_app_id_map(&entries, &wm_classes);
    ScanResult { entries, by_app_id, launch }
}

/// Every way an `app_id` is allowed to find an entry, strongest first.
///
/// Two passes rather than one, and that is the whole point of the function: a weak key belonging
/// to one entry must never beat a strong key belonging to another. Inserting entry by entry would
/// make the answer depend on scan order, so that `org.gnome.Nautilus` could resolve to whichever
/// application happened to be read first rather than to the one whose filename actually says so.
///
/// The last-segment rule is what makes this useful against a real compositor: niri reports
/// Nautilus as `org.gnome.Nautilus` and Files ships `org.gnome.Nautilus.desktop`, which the exact
/// pass already catches, but plenty of applications report a bare `nautilus` against a reverse-DNS
/// filename and nothing shorter would match those.
fn build_app_id_map(entries: &[AppSummary], wm_classes: &[(String, Option<String>)]) -> BTreeMap<String, AppSummary> {
    let by_id: HashMap<&str, &AppSummary> = entries.iter().map(|entry| (entry.id.as_str(), entry)).collect();
    let mut map: BTreeMap<String, AppSummary> = BTreeMap::new();

    // Pass 1, exact: `StartupWMClass` is the key the specification added for exactly this
    // question, so it outranks the filename even though the filename usually agrees.
    for (id, wm_class) in wm_classes {
        let Some(entry) = by_id.get(id.as_str()) else { continue };
        if let Some(class) = wm_class.as_deref().filter(|class| !class.is_empty()) {
            map.entry(class.to_string()).or_insert_with(|| (*entry).clone());
        }
    }
    for entry in entries {
        map.entry(entry.id.clone()).or_insert_with(|| entry.clone());
    }

    // Pass 2, case-folded and shortened. Every key here is a guess, which is why none of them may
    // displace anything above.
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

/// Every `.desktop` file under `dir`, depth-capped. A directory that does not exist contributes
/// nothing: most systems have no `/usr/local/share/applications`, and that is not an error.
fn collect_desktop_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        // `metadata` rather than `file_type`, following a symlink: a packaged application
        // directory routinely symlinks entries in from elsewhere (Flatpak's exports do), and
        // `file_type` calls those symlinks rather than files and would skip every one.
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

    /// Both XDG defaults are the point: a plain login shell has neither variable set, and a scan
    /// that skipped `/usr/share` when `XDG_DATA_DIRS` was unset would find nothing on a normal
    /// system.
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

    /// The user's own copy of an entry replaces the system one entirely, which is how a user
    /// renames an application or changes its command. A last-wins scan would ignore every such
    /// override.
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

    /// The reverse-DNS filename with a bare `app_id` is the common real mismatch: a toplevel
    /// reports `nautilus` where the file is named `org.gnome.Nautilus.desktop`.
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

    /// The two-pass property, and the reason the map is not built entry by entry: one entry's
    /// case-folded guess must never displace another entry's exact filename. Built in one pass,
    /// the winner would depend on which file the scan happened to read first.
    #[test]
    fn an_exact_id_beats_another_entrys_case_folded_wm_class() {
        let dir = tempfile::tempdir().unwrap();
        // Sorts first, so a single-pass build would let it claim "zed" before the real one is read.
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
