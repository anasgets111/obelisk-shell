//! `TrayItem` hydration: reading every `StatusNotifierItem` property `tray.items` needs (except
//! `menu`, fetched separately -- see [`super::menu::fetch_menu_via`]).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use serde::Serialize;
use zbus::names::OwnedUniqueName;

use super::icon::{IconPixmap, IconSource, largest_valid_pixmap, resolve_icon_source, write_icon_png};
use super::menu::MenuItem;
use super::proxies::StatusNotifierItemProxy;
use super::registration::sanitize_unique_name;

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TrayItem {
    /// § 2.14's stable id: the registering process's D-Bus unique name, sanitized (e.g. `"1.234"`).
    /// What every `tray:` command takes to name the item it acts on.
    pub id: String,
    /// The display name. `Title`, falling back to `Id` when the item leaves `Title` empty.
    pub name: String,
    /// A theme icon name, for `icon { name = ... }`. Exactly one of this and [`TrayItem::icon_path`]
    /// is ever set, so a config draws whichever is present.
    pub icon_name: Option<String>,
    /// A decoded, bounds-checked PNG spooled to the runtime directory, for `image { source = ... }`.
    /// Set when the item sent pixels rather than a theme name.
    pub icon_path: Option<String>,
    /// The `NeedsAttention` artwork, resolved the same way as `icon_name`/`icon_path`. Draw these
    /// instead of the base pair while `status` is `"NeedsAttention"`. Both stay `nil` for an item
    /// that declares no attention icon, which is most of them.
    pub attention_icon_name: Option<String>,
    /// The file half of the attention artwork, on the same terms as `attention_icon_name`.
    pub attention_icon_path: Option<String>,
    /// A badge, meant to be drawn over the base icon's corner rather than instead of it. Carried
    /// rather than composited: a `stack` node is what puts one image on another, and the Supervisor
    /// has no canvas. Both stay `nil` when the item declares no badge.
    pub overlay_icon_name: Option<String>,
    /// The file half of the badge, on the same terms as `overlay_icon_name`.
    pub overlay_icon_path: Option<String>,
    /// The item's tooltip title and text, flattened to one string. `nil` when it has none.
    pub tooltip: Option<String>,
    /// SNI's own `Status`: `"Active"`, `"Passive"` or `"NeedsAttention"`. `"Passive"` is the
    /// item asking to be hidden, which is a config's decision to honour or ignore.
    pub status: String,
    /// The item saying a left click must open its menu instead of activating it. Honour it, or
    /// a click does nothing on the items that set it.
    pub item_is_menu: bool,
    /// The top-level menu entries, or `nil` for an item with no `com.canonical.dbusmenu` menu.
    /// Fetched once when the item registers, then again on the item's own layout updates.
    pub menu: Option<Vec<MenuItem>>,
}

/// `Title`, falling back to `Id` when empty (ADR-0031's `TrayItem.name` field).
fn resolve_display_name(title: &str, id: &str) -> String {
    if title.is_empty() { id.to_string() } else { title.to_string() }
}

/// Flattens `ToolTip`'s title+text into one display string (ADR-0031: "your call on exact
/// formatting, keep it simple").
fn flatten_tooltip(title: &str, text: &str) -> Option<String> {
    match (title.is_empty(), text.is_empty()) {
        (true, true) => None,
        (false, true) => Some(title.to_string()),
        (true, false) => Some(text.to_string()),
        (false, false) => Some(format!("{title}\n{text}")),
    }
}

/// Reads every property `tray.items` needs except `menu` (fetched separately -- see
/// [`fetch_menu_via`] -- since the caller reuses an already-bound [`DBusMenuProxy`] rather
/// than re-resolving `Menu`'s object path on every refresh). A property read failure
/// degrades to that property's empty/default value rather than failing the whole item.
pub(super) async fn fetch_tray_item_base(
    item: &StatusNotifierItemProxy<'static>,
    unique_name: &OwnedUniqueName,
) -> TrayItem {
    let id_prop = item.id().await.unwrap_or_default();
    let title = item.title().await.unwrap_or_default();
    let status = item.status().await.unwrap_or_default();
    let item_is_menu = item.item_is_menu().await.unwrap_or(false);
    let tooltip = item.tool_tip().await.ok();
    // Read once and used by all three icon resolutions below: the directory is the item's, not any
    // one icon's (ADR-0074).
    let theme_path = item.icon_theme_path().await.unwrap_or_default();

    let sanitized = sanitize_unique_name(unique_name.as_str());
    let name = resolve_display_name(&title, &id_prop);
    let tooltip_flat = tooltip.and_then(|(_, _, tt_title, tt_text)| flatten_tooltip(&tt_title, &tt_text));

    let (icon_name, icon_path) = resolve_variant(
        item.icon_name().await.unwrap_or_default(),
        item.icon_pixmap().await.unwrap_or_default(),
        &theme_path,
        &sanitized,
        "",
    );
    let (attention_icon_name, attention_icon_path) = resolve_variant(
        item.attention_icon_name().await.unwrap_or_default(),
        item.attention_icon_pixmap().await.unwrap_or_default(),
        &theme_path,
        &sanitized,
        "-attention",
    );
    let (overlay_icon_name, overlay_icon_path) = resolve_variant(
        item.overlay_icon_name().await.unwrap_or_default(),
        item.overlay_icon_pixmap().await.unwrap_or_default(),
        &theme_path,
        &sanitized,
        "-overlay",
    );

    TrayItem {
        id: sanitized,
        name,
        icon_name,
        icon_path,
        attention_icon_name,
        attention_icon_path,
        overlay_icon_name,
        overlay_icon_path,
        tooltip: tooltip_flat,
        status,
        item_is_menu,
        menu: None,
    }
}

/// One icon triple (`{X}IconName`, `{X}IconPixmap`, the item's `IconThemePath`) resolved to the
/// `(name, path)` pair a config reads, for whichever of the three variants § 2.5 defines
/// (ADR-0074).
///
/// `spool_suffix` distinguishes the spooled PNGs, since an item's three pixmaps would otherwise all
/// land on `{unique_name}.png` and the last write would win.
fn resolve_variant(
    icon_name_prop: String,
    pixmaps_raw: Vec<(i32, i32, Vec<u8>)>,
    theme_path: &str,
    sanitized: &str,
    spool_suffix: &str,
) -> (Option<String>, Option<String>) {
    let pixmaps: Vec<IconPixmap> =
        pixmaps_raw.into_iter().map(|(width, height, bytes)| IconPixmap { width, height, bytes }).collect();
    match resolve_icon_source(&icon_name_prop, &pixmaps, theme_path) {
        IconSource::ThemePathFile(path) => (None, Some(path)),
        IconSource::Name(name) => (Some(name), None),
        IconSource::Pixmap => match largest_valid_pixmap(&pixmaps) {
            Some(pixmap) => match write_icon_png(&format!("{sanitized}{spool_suffix}"), pixmap) {
                Ok(path) => (None, Some(path)),
                Err(err) => {
                    eprintln!("tray: failed to spool icon PNG for {sanitized}{spool_suffix}: {err}");
                    (None, None)
                }
            },
            None => (None, None),
        },
        IconSource::None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resolve_display_name ----

    #[test]
    fn resolve_display_name_prefers_title() {
        assert_eq!(resolve_display_name("Discord", "discord"), "Discord");
    }

    #[test]
    fn resolve_display_name_falls_back_to_id_when_title_is_empty() {
        assert_eq!(resolve_display_name("", "discord"), "discord");
    }

    // ---- flatten_tooltip ----

    #[test]
    fn flatten_tooltip_is_none_when_both_are_empty() {
        assert_eq!(flatten_tooltip("", ""), None);
    }

    #[test]
    fn flatten_tooltip_uses_title_alone() {
        assert_eq!(flatten_tooltip("Battery", ""), Some("Battery".to_string()));
    }

    #[test]
    fn flatten_tooltip_uses_text_alone() {
        assert_eq!(flatten_tooltip("", "80% charged"), Some("80% charged".to_string()));
    }

    #[test]
    fn flatten_tooltip_joins_title_and_text() {
        assert_eq!(flatten_tooltip("Battery", "80% charged"), Some("Battery\n80% charged".to_string()));
    }
}
