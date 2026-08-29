//! Theme name to file path, in the Renderer (docs/adr/0054, build-steps.md Phase 29 item 3).
//!
//! `oblisk-idl-api-specs.md` § 5.2 item 5 gives `icon` a `name` holding a theme name and
//! `oblisk-supervisor-services-dbus.md` § 9.2 puts the resolver in the Supervisor. docs/adr/0054
//! settles that here rather than there, on the grounds that § 3.2's own row calls
//! `system:find_icon` a synchronous lookup returning a path, and the control socket carries one-way
//! commands one way and one-way `StateSnapshot`s the other with no request/response shape to make
//! that true over.
//!
//! § 9.2's second half, `app_id` to `.desktop` file to `Icon=` key, is not built and has no caller:
//! with `name` resolving theme names here, nothing is left to ask a `find_icon` for. There is no
//! Lua-facing function in this module for the same reason.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The file for `name` at `size` pixels, or `None` if the active theme and its inheritance chain
/// have nothing under that name.
///
/// An absolute `name` is that path. This is not a special case invented here: the `Icon=` key in
/// every `.desktop` file on the system accepts either spelling, so a config author already knows
/// the rule. What it buys is § 2.5's tray, which populates exactly one of `icon_name` and
/// `icon_path` and never both, collapsing to `icon { name = item.icon_name or item.icon_path }`
/// instead of a branch between two node kinds inside a `list`.
///
/// Existence is not checked for the absolute case: `ImageCache::image` is about to open the file
/// anyway, and it already logs once and caches the failure. Checking here would be a second stat
/// and a second, worse error message.
pub fn resolve(name: &str, size: u16) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if Path::new(name).is_absolute() {
        return Some(PathBuf::from(name));
    }
    // `with_cache` is `freedesktop-icons`' own theme-index cache, which is the part § 9.2 wanted an
    // LRU for and the part that would otherwise walk every directory under `/usr/share/icons` on
    // each lookup, on the thread that paints.
    freedesktop_icons::lookup(name).with_theme(theme()).with_size(size).with_cache().find()
}

/// The active icon theme's *directory* name, read once per process.
///
/// Once, because this runs behind a paint and re-reading a settings file per frame is not a thing
/// to do there. The cost is that changing icon theme mid-session shows up at the next reload
/// rather than immediately, which is the same cadence the font chain already has.
fn theme() -> &'static str {
    static THEME: OnceLock<String> = OnceLock::new();
    THEME.get_or_init(|| gtk_icon_theme_name().unwrap_or_else(|| "hicolor".to_string()))
}

/// `freedesktop-icons` ships `default_theme_gtk()` for this and it cannot be used for two
/// independent reasons, both found by running it on a real desktop rather than by reading it.
///
/// It shells out to `gsettings get org.gnome.desktop.interface icon-theme`. That is a process spawn
/// per call, from the render thread, which since docs/adr/0039 is also the Wayland dispatch thread.
///
/// And it does not return what `with_theme` takes. It maps the setting's value through the theme's
/// `index.theme` and returns the `Name=` field, so on this machine it answers `"Tela circle
/// dracula"` while `with_theme` is keyed by directory name and wants `"Tela-circle-dracula"`. The
/// crate's own two functions do not compose, and the mismatch fails silently: every lookup returns
/// `None` and every icon is simply missing.
///
/// So the setting is read directly, from the file that holds the directory name in the first place.
/// GTK 4 before GTK 3 because a machine with both usually has the newer one current, and neither
/// before `hicolor`, which the icon theme spec makes the implicit final fallback anyway.
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
        // Not stat'd, so a path that does not exist still comes back as itself and fails later
        // where the failure is cached.
        assert_eq!(resolve("/nonexistent/x.png", 16), Some(PathBuf::from("/nonexistent/x.png")));
    }

    #[test]
    fn an_empty_name_resolves_to_nothing() {
        // `icon` with no `name` defaults to `""`, and a `name` bound to a capability signal is
        // `nil` until that capability's first push (docs/adr/0037). Both land here, and neither
        // should reach the theme lookup.
        assert_eq!(resolve("", 16), None);
    }

    #[test]
    fn a_relative_name_is_not_treated_as_a_path() {
        // A theme name with a slash in it is still a theme name as far as this is concerned: only
        // an absolute path short-circuits, so `./x.png` goes to the lookup and finds nothing
        // rather than resolving against whatever the Renderer's working directory happens to be.
        assert_eq!(resolve("./oblisk-does-not-exist.png", 16), None);
    }

    #[test]
    fn the_settings_parse_returns_the_directory_name_gtk_wrote() {
        let ini = "[Settings]\ngtk-theme-name=Adwaita-dark\ngtk-icon-theme-name=Tela-circle-dracula\ngtk-font-name=Cantarell 11\n";
        assert_eq!(icon_theme_from_settings(ini).as_deref(), Some("Tela-circle-dracula"));
        // Spacing around the `=` is legal in a GLib key file, and quotes are how some tools write
        // the value.
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name = Papirus").as_deref(), Some("Papirus"));
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name=\"Papirus\"").as_deref(), Some("Papirus"));
    }

    #[test]
    fn a_settings_file_without_the_key_falls_through_rather_than_matching_a_prefix() {
        assert_eq!(icon_theme_from_settings("[Settings]\ngtk-theme-name=Adwaita\n"), None);
        assert_eq!(icon_theme_from_settings(""), None);
        // An empty value is not a theme: taking it would send `with_theme("")` to the lookup and
        // silently lose every icon, which is exactly the failure `default_theme_gtk` produces.
        assert_eq!(icon_theme_from_settings("gtk-icon-theme-name="), None);
    }

    #[test]
    fn the_resolved_theme_is_a_directory_under_an_icon_search_path() {
        // The whole point of not using `freedesktop_icons::default_theme_gtk()`: it answers with an
        // `index.theme` `Name=` field, and `with_theme` is keyed by directory name. A theme this
        // resolves to must therefore exist as a directory, or every lookup silently returns None.
        let theme = theme();
        assert!(!theme.is_empty());
        assert!(
            !theme.contains('/'),
            "the theme is a directory name, not a path, got {theme:?}"
        );
    }
}
