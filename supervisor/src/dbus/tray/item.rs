//! `TrayItem` hydration: reading every `StatusNotifierItem` property `tray.items` needs (except
//! `menu`, fetched separately -- see [`super::menu::fetch_menu_via`]).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use serde::Serialize;
use zbus::names::OwnedUniqueName;

use super::icon::{IconPixmap, IconSource, largest_valid_pixmap, resolve_icon_source, write_icon_png};
use super::menu::MenuItem;
use super::proxies::StatusNotifierItemProxy;
use super::registration::sanitize_unique_name;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TrayItem {
    pub id: String,
    pub name: String,
    pub icon_name: Option<String>,
    pub icon_path: Option<String>,
    pub tooltip: Option<String>,
    pub status: String,
    pub item_is_menu: bool,
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
pub(super) async fn fetch_tray_item_base(item: &StatusNotifierItemProxy<'static>, unique_name: &OwnedUniqueName) -> TrayItem {
    let id_prop = item.id().await.unwrap_or_default();
    let title = item.title().await.unwrap_or_default();
    let icon_name_prop = item.icon_name().await.unwrap_or_default();
    let pixmaps_raw = item.icon_pixmap().await.unwrap_or_default();
    let status = item.status().await.unwrap_or_default();
    let item_is_menu = item.item_is_menu().await.unwrap_or(false);
    let tooltip = item.tool_tip().await.ok();

    let sanitized = sanitize_unique_name(unique_name.as_str());
    let name = resolve_display_name(&title, &id_prop);
    let tooltip_flat = tooltip.and_then(|(_, _, tt_title, tt_text)| flatten_tooltip(&tt_title, &tt_text));

    let pixmaps: Vec<IconPixmap> = pixmaps_raw.into_iter().map(|(width, height, bytes)| IconPixmap { width, height, bytes }).collect();
    let (icon_name, icon_path) = match resolve_icon_source(&icon_name_prop, &pixmaps) {
        IconSource::Name(name) => (Some(name), None),
        IconSource::Pixmap => match largest_valid_pixmap(&pixmaps) {
            Some(pixmap) => match write_icon_png(&sanitized, pixmap) {
                Ok(path) => (None, Some(path)),
                Err(err) => {
                    eprintln!("tray: failed to spool icon PNG for {sanitized}: {err}");
                    (None, None)
                }
            },
            None => (None, None),
        },
        IconSource::None => (None, None),
    };

    TrayItem { id: sanitized, name, icon_name, icon_path, tooltip: tooltip_flat, status, item_is_menu, menu: None }
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
