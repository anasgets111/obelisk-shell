//! Theme name to file path, in the Renderer (ADR-0054 decision 1).
//!
//! The Supervisor plan failed because synchronous lookup would block its dispatch thread on a
//! control-socket round trip. That socket carries one-way commands and `StateSnapshot`s, not
//! request/response pairs, so the answer is resolved locally here.
//!
//! `app_id` -> `.desktop` -> `Icon=` remains deferred with no caller, and no Lua `find_icon`
//! exists (ADR-0054 decision 5): `name` resolution leaves it nothing to do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The file for `name` at `size` pixels, or `None` if the active theme chain has no match.
///
/// An absolute `name` is returned directly. `.desktop` `Icon=` accepts either spelling, letting the
/// tray use `icon { name = item.icon_name or item.icon_path }` without two node kinds.
///
/// Absolute paths are not checked: `ImageCache::image` opens them, logs once, and caches failure.
///
/// Memoized because `ImageCache` keys on the resolved *path*: the texture was cached while this
/// ran every frame for every icon. Idle bar, release build, 30 seconds: 62 calls, 101.6ms, mean
/// 1.6ms, worst 5.5ms, or 62% of `layout::paint::execute` draw-command time, more than text,
/// boxes, and images combined. Misses walk the inheritance chain and `stat` every candidate, so
/// `None` is memoized too.
///
/// Keep `freedesktop-icons`' `with_cache`: it caches parsed theme *indexes*, avoiding reads of
/// every `index.theme` under `/usr/share/icons` on the first lookup, but not the per-name search
/// this map removes.
pub fn resolve(name: &str, size: u16) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if Path::new(name).is_absolute() {
        return Some(PathBuf::from(name));
    }
    memoized(name, size, || freedesktop_icons::lookup(name).with_theme(theme()).with_size(size).with_cache().find())
}

/// [`resolve`]'s memo, with the filesystem walk injected so tests can count it. A correct answer
/// re-derived every frame is the defect.
fn memoized(name: &str, size: u16, lookup: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(hit) = memo().lock().expect("icon memo poisoned").get(&size).and_then(|by_name| by_name.get(name)) {
        return hit.clone();
    }
    let found = lookup();
    // Do not hold the map across `lookup`: its filesystem walk would serialize callers.
    let mut memo = memo().lock().expect("icon memo poisoned");
    let by_name = memo.entry(size).or_default();
    if by_name.len() >= MEMO_CAPACITY {
        by_name.clear();
    }
    by_name.insert(name.to_string(), found.clone());
    found
}

/// Process-lifetime resolved lookups, keyed by size then name so hits borrow a `&str`.
///
/// Process scope matches [`theme`]: the active theme is read once and changes only when reload
/// replaces the Renderer (ADR-0054). Use `Mutex`, not `thread_local!`, because callers are not
/// limited to the render thread; an uncontended lock is nanoseconds against the measured 1.6ms
/// lookup.
///
/// Capped at [`MEMO_CAPACITY`] names per size and cleared wholesale there, because the set of names
/// is not bounded by what is installed: a notification's `app_icon` is a bare theme name chosen by
/// whichever application sent it, and a config drawing it (`icon { name = group.app_icon }`) hands
/// it straight to [`resolve`]. Every novel name also pays the ~1.6ms walk this memo exists to
/// avoid, on the render thread.
///
/// Cleared rather than evicted, following `text::shaping`'s cache: an LRU maintains a recency order
/// on every hit, which is work on the path the memo exists to make cheap.
fn memo() -> &'static Mutex<Memo> {
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(Memo::new()))
}

/// Names remembered per size. Sized like `text::shaping`'s cache: far past any real working set (a
/// bar and its panels resolve tens of icons), while still bounding a stream of novel names.
const MEMO_CAPACITY: usize = 4096;

/// Size -> name -> path, nested so hits borrow the inner `&str` without allocating.
type Memo = HashMap<u16, HashMap<String, Option<PathBuf>>>;

/// The active theme's *directory* name, read once per process. Theme changes appear on reload, the
/// font-chain cadence, rather than rereading settings behind every paint.
fn theme() -> &'static str {
    static THEME: OnceLock<String> = OnceLock::new();
    THEME.get_or_init(|| gtk_icon_theme_name().unwrap_or_else(|| "hicolor".to_string()))
}

/// Do not use `freedesktop-icons::default_theme_gtk()`: it spawns
/// `gsettings get org.gnome.desktop.interface icon-theme` per render-thread call (the Wayland
/// dispatch thread since ADR-0039), maps the setting through `index.theme` to `Name=`
/// (`"Tela circle dracula"`), while `with_theme` needs the directory name
/// (`"Tela-circle-dracula"`). The mismatch silently returns `None`.
///
/// Read the directory name directly from settings, GTK 4 before GTK 3, then `hicolor`, the spec's
/// implicit fallback.
///
/// ponytail: GTK settings only. KDE uses `Icons/Theme` in `kdeglobals`, so Plasma falls to
/// `hicolor`, drawing app icons but almost no status icons. Upgrade: another `find_map` arm; not
/// built because Plasma is untested and a second untested parser is worse than this gap.
fn gtk_icon_theme_name() -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    ["gtk-4.0", "gtk-3.0"].into_iter().find_map(|version| {
        let text = std::fs::read_to_string(config.join(version).join("settings.ini")).ok()?;
        icon_theme_from_settings(&text)
    })
}

/// The `gtk-icon-theme-name` value from `settings.ini`, split out so parsing needs no home dir.
fn icon_theme_from_settings(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("gtk-icon-theme-name"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"').to_string())
        .find(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_name_is_its_own_path() {
        // `dbus/shm_icons.rs` spools tray icons here as `icon_path`.
        let spooled = "/dev/shm/obelisk-1000/tray/telegram.png";
        assert_eq!(resolve(spooled, 16), Some(PathBuf::from(spooled)));
        // No stat: nonexistent absolute paths still return themselves.
        assert_eq!(resolve("/nonexistent/x.png", 16), Some(PathBuf::from("/nonexistent/x.png")));
    }

    #[test]
    fn an_empty_name_resolves_to_nothing() {
        // Missing `icon.name` is `""`; an unhydrated capability is `nil` until its first push
        // (ADR-0037). Both land here.
        assert_eq!(resolve("", 16), None);
    }

    #[test]
    fn a_relative_name_is_not_treated_as_a_path() {
        // Only absolute paths short-circuit; `./x.png` is looked up, not resolved against CWD.
        assert_eq!(resolve("./obelisk-does-not-exist.png", 16), None);
    }

    #[test]
    fn the_settings_parse_returns_the_directory_name_gtk_wrote() {
        let ini = "[Settings]\ngtk-theme-name=Adwaita-dark\ngtk-icon-theme-name=Tela-circle-dracula\ngtk-font-name=Cantarell 11\n";
        assert_eq!(icon_theme_from_settings(ini).as_deref(), Some("Tela-circle-dracula"));
        // GLib permits spaces around `=`; some tools quote the value.
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name = Papirus").as_deref(), Some("Papirus"));
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name=\"Papirus\"").as_deref(), Some("Papirus"));
    }

    #[test]
    fn a_settings_file_without_the_key_falls_through_rather_than_matching_a_prefix() {
        assert_eq!(icon_theme_from_settings("[Settings]\ngtk-theme-name=Adwaita\n"), None);
        assert_eq!(icon_theme_from_settings(""), None);
        // Empty is not a theme: `with_theme("")` silently loses every icon.
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name="), None);
    }

    #[test]
    fn the_resolved_theme_is_a_directory_under_an_icon_search_path() {
        let theme = theme();
        assert!(!theme.is_empty());
        assert!(!theme.contains('/'), "the theme is a directory name, not a path, got {theme:?}");
    }

    #[test]
    fn a_resolved_name_is_looked_up_once_and_remembered() {
        let calls = std::cell::Cell::new(0);
        let found = memoized("obelisk-test-alpha", 16, || {
            calls.set(calls.get() + 1);
            Some(PathBuf::from("/memo/alpha-16"))
        });
        assert_eq!(found, Some(PathBuf::from("/memo/alpha-16")));
        assert_eq!(calls.get(), 1);
        // The second ask must skip the walk, measured at 1.6ms mean per icon per frame before this.
        let again = memoized("obelisk-test-alpha", 16, || panic!("a remembered name must not be looked up again"));
        assert_eq!(again, Some(PathBuf::from("/memo/alpha-16")));
    }

    #[test]
    fn the_memo_keys_on_both_name_and_size() {
        // A collision draws the *wrong* icon, harder to notice than a missing one: a plausible bar
        // full of icons other than the requested ones.
        assert_eq!(
            memoized("obelisk-test-beta", 16, || Some(PathBuf::from("/memo/beta-16"))),
            Some(PathBuf::from("/memo/beta-16"))
        );
        assert_eq!(
            memoized("obelisk-test-beta", 32, || Some(PathBuf::from("/memo/beta-32"))),
            Some(PathBuf::from("/memo/beta-32"))
        );
        assert_eq!(
            memoized("obelisk-test-gamma", 16, || Some(PathBuf::from("/memo/gamma-16"))),
            Some(PathBuf::from("/memo/gamma-16"))
        );
        assert_eq!(
            memoized("obelisk-test-beta", 16, || panic!("a remembered name must not be looked up again")),
            Some(PathBuf::from("/memo/beta-16"))
        );
    }

    #[test]
    fn a_name_the_theme_does_not_have_is_remembered_as_absent() {
        // Misses walk the whole inheritance chain and stat every candidate; worst call was 5.5ms.
        // Memoize `None` exactly like a hit.
        assert_eq!(memoized("obelisk-test-missing", 16, || None), None);
        assert_eq!(memoized("obelisk-test-missing", 16, || panic!("an absent name must not be looked up again")), None);
    }

    /// A notification's `app_icon` is an arbitrary name from whichever application sent it, so the
    /// set of names is not bounded by what is installed. Without the cap this map grew for the life
    /// of the generation.
    #[test]
    fn the_memo_drops_its_names_rather_than_growing_without_bound() {
        let mut walks = 0;
        for i in 0..=MEMO_CAPACITY {
            memoized(&format!("name-that-no-theme-has-{i}"), 999, || {
                walks += 1;
                None
            });
        }
        let held = memo().lock().unwrap().get(&999).map(HashMap::len).unwrap_or_default();
        assert!(held <= MEMO_CAPACITY, "the map must not hold more than the cap, held {held}");
        assert_eq!(walks, MEMO_CAPACITY + 1, "every novel name still resolves; the cap bounds memory, not correctness");
    }
}
