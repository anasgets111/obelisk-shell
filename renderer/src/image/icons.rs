//! Theme name to file path, in the Renderer (docs/adr/0054, build-steps.md Phase 29 item 3).
//!
//! docs/adr/0054 settles the resolver here rather than in the Supervisor (`oblisk-supervisor-
//! services-dbus.md` § 9.2's original plan): § 3.2 calls `system:find_icon` a synchronous lookup
//! returning a path, and the control socket has no request/response shape to make that true over
//! -- only one-way commands and one-way `StateSnapshot`s.
//!
//! § 9.2's second half, `app_id` to `.desktop` file to `Icon=` key, is not built and has no
//! caller: with `name` resolving theme names here, nothing is left to ask a `find_icon` for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The file for `name` at `size` pixels, or `None` if the active theme and its inheritance chain
/// have nothing under that name.
///
/// An absolute `name` is that path -- the `Icon=` key in every `.desktop` file accepts either
/// spelling. This lets § 2.5's tray collapse to `icon { name = item.icon_name or item.icon_path }`
/// instead of a branch between two node kinds.
///
/// Existence is not checked for the absolute case: `ImageCache::image` is about to open the file
/// anyway, and already logs once and caches the failure.
///
/// Memoized, and that is not a micro-optimization. `ImageCache` keys on the resolved *path*, so
/// the uploaded texture was already cached while this lookup ran again on every frame for every
/// icon. Measured on an idle bar, release build, over 30 seconds: 62 calls, 101.6ms, a mean of
/// 1.6ms and a worst case of 5.5ms, which was 62% of all the time `layout::paint::execute` spent
/// recording draw commands -- more than text, boxes and images put together. A miss is the
/// expensive case, since "not found" means the whole inheritance chain was walked and every
/// candidate stat'd, so a `None` is memoized too.
///
/// `with_cache` is `freedesktop-icons`' own cache and stays: it caches parsed theme *indexes*,
/// which is what keeps the first lookup for a name from reading every `index.theme` under
/// `/usr/share/icons`. It does not cache the per-name search those indexes are then used for,
/// which is the cost this map removes.
pub fn resolve(name: &str, size: u16) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if Path::new(name).is_absolute() {
        return Some(PathBuf::from(name));
    }
    memoized(name, size, || freedesktop_icons::lookup(name).with_theme(theme()).with_size(size).with_cache().find())
}

/// [`resolve`]'s memo, with the filesystem walk passed in so a test can count how often it runs.
/// That count is the whole behaviour: an answer that is right but re-derived every frame is the
/// defect this closes.
fn memoized(name: &str, size: u16, lookup: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(hit) = memo().lock().expect("icon memo poisoned").get(&size).and_then(|by_name| by_name.get(name)) {
        return hit.clone();
    }
    let found = lookup();
    // Re-locked rather than held across `lookup`: it walks the filesystem, and holding the map for
    // that would serialize every other caller behind the slowest possible path.
    memo().lock().expect("icon memo poisoned").entry(size).or_default().insert(name.to_string(), found.clone());
    found
}

/// Resolved lookups for the life of the process, keyed by size then name so a hit can be found
/// from a `&str` without allocating one.
///
/// Process lifetime is the right scope because [`theme`] already has it: the active theme is read
/// once, so what this maps cannot change without the reload that replaces the whole Renderer
/// process (docs/adr/0054). A `Mutex` rather than a `thread_local!` because nothing about the
/// function says render thread, and an uncontended lock is nanoseconds against a lookup that
/// measured 1.6 *milli*seconds.
///
/// ponytail: unbounded, and bounded in practice by how many distinct icon names one session shows
/// -- tray items and whatever the config names. Each entry is a name and a path. A session that
/// cycled through thousands of distinct icon names would grow it, and the generation swap is what
/// frees it, which is the same bargain `ImageCache` takes one layer down.
fn memo() -> &'static Mutex<Memo> {
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(Memo::new()))
}

/// Size to name to resolved path, nested so a lookup can borrow a `&str` for the inner key rather
/// than allocate a `String` per hit.
type Memo = HashMap<u16, HashMap<String, Option<PathBuf>>>;

/// The active icon theme's *directory* name, read once per process: this runs behind a paint, and
/// re-reading a settings file per frame is not a thing to do there. Changing icon theme mid-session
/// shows up at the next reload rather than immediately, the same cadence the font chain has.
fn theme() -> &'static str {
    static THEME: OnceLock<String> = OnceLock::new();
    THEME.get_or_init(|| gtk_icon_theme_name().unwrap_or_else(|| "hicolor".to_string()))
}

/// `freedesktop-icons` ships `default_theme_gtk()` for this and it cannot be used: it shells out
/// to `gsettings get org.gnome.desktop.interface icon-theme` (a process spawn per call, from the
/// render thread, which since docs/adr/0039 is also the Wayland dispatch thread), and it does not
/// return what `with_theme` takes -- it maps the setting through the theme's `index.theme` and
/// returns the `Name=` field (`"Tela circle dracula"`), while `with_theme` is keyed by directory
/// name (`"Tela-circle-dracula"`). The mismatch fails silently: every lookup returns `None`.
///
/// So the setting is read directly, from the file that holds the directory name in the first
/// place. GTK 4 before GTK 3 because a machine with both usually has the newer one current, and
/// neither before `hicolor`, the icon theme spec's implicit final fallback.
///
/// ponytail: GTK's settings file only. A KDE session sets `Icons/Theme` in `kdeglobals` and would
/// land on `hicolor`, which has app icons and almost no status icons, so a Plasma user's bar would
/// draw app icons and nothing else. The upgrade is another arm in the `find_map` below; not built
/// because this is not tested on Plasma and a second untested parser is worse than one honest gap.
fn gtk_icon_theme_name() -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    ["gtk-4.0", "gtk-3.0"].into_iter().find_map(|version| {
        let text = std::fs::read_to_string(config.join(version).join("settings.ini")).ok()?;
        icon_theme_from_settings(&text)
    })
}

/// The `gtk-icon-theme-name` value out of a `settings.ini` body. Split out from the file reading so
/// the parse is testable without a home directory to arrange.
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
        // The tray case: `dbus/shm_icons.rs` spools here and reports the path as `icon_path`.
        let spooled = "/dev/shm/oblisk-1000/tray/telegram.png";
        assert_eq!(resolve(spooled, 16), Some(PathBuf::from(spooled)));
        // Not stat'd: a path that does not exist still comes back as itself.
        assert_eq!(resolve("/nonexistent/x.png", 16), Some(PathBuf::from("/nonexistent/x.png")));
    }

    #[test]
    fn an_empty_name_resolves_to_nothing() {
        // `icon` with no `name` defaults to `""`, and a `name` bound to an unhydrated capability
        // signal is `nil` until its first push (docs/adr/0037). Both land here.
        assert_eq!(resolve("", 16), None);
    }

    #[test]
    fn a_relative_name_is_not_treated_as_a_path() {
        // Only an absolute path short-circuits, so `./x.png` goes to the lookup and finds
        // nothing rather than resolving against the Renderer's working directory.
        assert_eq!(resolve("./oblisk-does-not-exist.png", 16), None);
    }

    #[test]
    fn the_settings_parse_returns_the_directory_name_gtk_wrote() {
        let ini = "[Settings]\ngtk-theme-name=Adwaita-dark\ngtk-icon-theme-name=Tela-circle-dracula\ngtk-font-name=Cantarell 11\n";
        assert_eq!(icon_theme_from_settings(ini).as_deref(), Some("Tela-circle-dracula"));
        // Spacing around `=` is legal in a GLib key file; quotes are how some tools write it.
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name = Papirus").as_deref(), Some("Papirus"));
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name=\"Papirus\"").as_deref(), Some("Papirus"));
    }

    #[test]
    fn a_settings_file_without_the_key_falls_through_rather_than_matching_a_prefix() {
        assert_eq!(icon_theme_from_settings("[Settings]\ngtk-theme-name=Adwaita\n"), None);
        assert_eq!(icon_theme_from_settings(""), None);
        // An empty value is not a theme: `with_theme("")` would silently lose every icon.
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
        let found = memoized("oblisk-test-alpha", 16, || {
            calls.set(calls.get() + 1);
            Some(PathBuf::from("/memo/alpha-16"))
        });
        assert_eq!(found, Some(PathBuf::from("/memo/alpha-16")));
        assert_eq!(calls.get(), 1);
        // The point of the memo: the second ask must not reach the walk at all. That walk measured
        // a 1.6ms mean per call, once per icon per frame, before this landed.
        let again = memoized("oblisk-test-alpha", 16, || panic!("a remembered name must not be looked up again"));
        assert_eq!(again, Some(PathBuf::from("/memo/alpha-16")));
    }

    #[test]
    fn the_memo_keys_on_both_name_and_size() {
        // A collision here draws the *wrong* icon rather than none, which is the harder failure to
        // notice: a bar full of plausible-looking icons that are not the ones asked for.
        assert_eq!(
            memoized("oblisk-test-beta", 16, || Some(PathBuf::from("/memo/beta-16"))),
            Some(PathBuf::from("/memo/beta-16"))
        );
        assert_eq!(
            memoized("oblisk-test-beta", 32, || Some(PathBuf::from("/memo/beta-32"))),
            Some(PathBuf::from("/memo/beta-32"))
        );
        assert_eq!(
            memoized("oblisk-test-gamma", 16, || Some(PathBuf::from("/memo/gamma-16"))),
            Some(PathBuf::from("/memo/gamma-16"))
        );
        assert_eq!(
            memoized("oblisk-test-beta", 16, || panic!("a remembered name must not be looked up again")),
            Some(PathBuf::from("/memo/beta-16"))
        );
    }

    #[test]
    fn a_name_the_theme_does_not_have_is_remembered_as_absent() {
        // The miss is the expensive case, not the cheap one: "not found" is what the whole
        // inheritance chain being walked and every candidate stat'd looks like. Worst single call
        // measured 5.5ms. So `None` is memoized exactly like a hit.
        assert_eq!(memoized("oblisk-test-missing", 16, || None), None);
        assert_eq!(memoized("oblisk-test-missing", 16, || panic!("an absent name must not be looked up again")), None);
    }
}
