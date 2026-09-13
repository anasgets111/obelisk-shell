//! `TrayItem` hydration from `StatusNotifierItem` properties; `menu` is fetched separately.
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use serde::Serialize;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::OwnedObjectPath;

use super::MAX_TRAY_TEXT_BYTES;
use super::icon::{
    IconPixmap, IconSource, icon_filename_stem, largest_valid_pixmap, resolve_icon_source, write_icon_png,
};
use super::menu::MenuItem;
use super::proxies::StatusNotifierItemProxy;
use super::registration::item_id;
use crate::capabilities::truncate_utf8_bytes;

#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TrayItem {
    /// Sanitized D-Bus unique name with the item's object path appended, e.g.
    /// `"1.234/StatusNotifierItem"`. Used by every `tray:` command.
    pub id: String,
    /// Display name: `Title`, falling back to `Id` when `Title` is empty.
    pub name: String,
    /// Theme icon name for `icon { name = ... }`; exclusive with [`TrayItem::icon_path`].
    pub icon_name: Option<String>,
    /// Decoded, bounds-checked PNG in the runtime directory for `image { source = ... }`; set when
    /// the item sent pixels instead of a theme name.
    pub icon_path: Option<String>,
    /// `NeedsAttention` artwork, resolved like `icon_name`/`icon_path`; draw it instead of the base
    /// pair when `status == "NeedsAttention"`. Both are `nil` when undeclared.
    pub attention_icon_name: Option<String>,
    /// File half of the attention artwork, matching `attention_icon_name`.
    pub attention_icon_path: Option<String>,
    /// Badge to draw over the base icon's corner. Carried, not composited, because a `stack` node
    /// overlays images and the Supervisor has no canvas. Both are `nil` when undeclared.
    pub overlay_icon_name: Option<String>,
    /// File half of the badge, matching `overlay_icon_name`.
    pub overlay_icon_path: Option<String>,
    /// Tooltip title and text flattened to one string, or `nil` when absent.
    pub tooltip: Option<String>,
    /// SNI status: `"Active"`, `"Passive"`, or `"NeedsAttention"`. `"Passive"` asks config to hide
    /// the item.
    pub status: String,
    /// `true` means left click opens the menu instead of activating the item.
    pub item_is_menu: bool,
    /// Top-level menu entries, or `nil` without `com.canonical.dbusmenu`. Fetched at registration
    /// and on layout updates.
    pub menu: Option<Vec<MenuItem>>,
}

/// [`MAX_TRAY_TEXT_BYTES`] applied to one application-supplied property.
fn capped(value: String) -> String {
    truncate_utf8_bytes(&value, MAX_TRAY_TEXT_BYTES)
}

/// Resolves `TrayItem.name`: `Title`, falling back to `Id` when empty (ADR-0031).
fn resolve_display_name(title: &str, id: &str) -> String {
    if title.is_empty() { id.to_string() } else { title.to_string() }
}

/// Flattens `ToolTip`'s title and text (ADR-0031 leaves exact formatting open).
fn flatten_tooltip(title: &str, text: &str) -> Option<String> {
    match (title.is_empty(), text.is_empty()) {
        (true, true) => None,
        (false, true) => Some(title.to_string()),
        (true, false) => Some(text.to_string()),
        (false, false) => Some(format!("{title}\n{text}")),
    }
}

/// Reads every property `tray.items` needs except `menu`, which uses the caller's bound proxy via
/// [`fetch_menu_via`]. A failed property read falls back to that property's empty/default value.
pub(super) async fn fetch_tray_item_base(
    item: &StatusNotifierItemProxy<'static>,
    unique_name: &OwnedUniqueName,
    object_path: &OwnedObjectPath,
) -> TrayItem {
    // Every string below is whatever application owns this item; cap each on the way in
    // (`MAX_TRAY_TEXT_BYTES`) rather than trusting SNI, which bounds none of them.
    let id_prop = capped(item.id().await.unwrap_or_default());
    let title = capped(item.title().await.unwrap_or_default());
    let status = capped(item.status().await.unwrap_or_default());
    let item_is_menu = item.item_is_menu().await.unwrap_or(false);
    let tooltip = item.tool_tip().await.ok();
    // Read once for all three icon variants; the directory belongs to the item (ADR-0074). Not
    // capped with the rest: a path cut short names a *different* directory rather than none, so
    // `theme_path_file` bounds it at `PATH_MAX` where it is used instead.
    let theme_path = item.icon_theme_path().await.unwrap_or_default();

    let id = item_id(unique_name.as_str(), object_path.as_str());
    let stem = icon_filename_stem(&id);
    let name = resolve_display_name(&title, &id_prop);
    let tooltip_flat =
        tooltip.and_then(|(_, _, tt_title, tt_text)| flatten_tooltip(&capped(tt_title), &capped(tt_text)));

    let (icon_name, icon_path) = resolve_variant(
        item.icon_name().await.unwrap_or_default(),
        item.icon_pixmap().await.unwrap_or_default(),
        &theme_path,
        &stem,
        "",
    );
    let (attention_icon_name, attention_icon_path) = resolve_variant(
        item.attention_icon_name().await.unwrap_or_default(),
        item.attention_icon_pixmap().await.unwrap_or_default(),
        &theme_path,
        &stem,
        "-attention",
    );
    let (overlay_icon_name, overlay_icon_path) = resolve_variant(
        item.overlay_icon_name().await.unwrap_or_default(),
        item.overlay_icon_pixmap().await.unwrap_or_default(),
        &theme_path,
        &stem,
        "-overlay",
    );

    TrayItem {
        id,
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

/// One icon triple (`{X}IconName`, `{X}IconPixmap`, `IconThemePath`) resolved to the config's
/// `(name, path)` pair (ADR-0074). `spool_suffix` keeps the three PNGs distinct; otherwise the last
/// write to `{unique_name}.png` would win.
fn resolve_variant(
    icon_name_prop: String,
    pixmaps_raw: Vec<(i32, i32, Vec<u8>)>,
    theme_path: &str,
    stem: &str,
    spool_suffix: &str,
) -> (Option<String>, Option<String>) {
    let pixmaps: Vec<IconPixmap> =
        pixmaps_raw.into_iter().map(|(width, height, bytes)| IconPixmap { width, height, bytes }).collect();
    // Capped here rather than at the three call sites, so no `{X}IconName` can reach a `TrayItem`
    // uncapped by being passed in from a fourth one later.
    match resolve_icon_source(&capped(icon_name_prop), &pixmaps, theme_path) {
        IconSource::ThemePathFile(path) => (None, Some(path)),
        IconSource::Name(name) => (Some(name), None),
        IconSource::Pixmap => match largest_valid_pixmap(&pixmaps) {
            Some(pixmap) => match write_icon_png(&format!("{stem}{spool_suffix}"), pixmap) {
                Ok(path) => (None, Some(path)),
                Err(err) => {
                    eprintln!("tray: failed to spool icon PNG for {stem}{spool_suffix}: {err}");
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
