//! Hand-written proxies for SNI, DBusMenu, and the watcher (ADR-0031: no maintained zbus proxy
//! crate).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use std::collections::HashMap;

use serde::Deserialize;
use zbus::names::OwnedBusName;
use zbus::zvariant::{Array, Dict, OwnedObjectPath, OwnedValue, Signature, Str, StructureBuilder, Type, Value};

use super::{RawIconPixmap, RawToolTip};

#[zbus::proxy(interface = "org.kde.StatusNotifierItem")]
pub(super) trait StatusNotifierItem {
    #[zbus(name = "Activate")]
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Middle-click, a separate spec method exported by Telegram, Chromium, and Qt tray.
    #[zbus(name = "SecondaryActivate")]
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Scroll over the icon. `orientation` is normally `"vertical"`/`"horizontal"`; `delta` carries
    /// sign and magnitude.
    #[zbus(name = "Scroll")]
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

    #[zbus(property, name = "Id")]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Status")]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Title")]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property, name = "IconName")]
    fn icon_name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "IconPixmap")]
    fn icon_pixmap(&self) -> zbus::Result<Vec<RawIconPixmap>>;
    #[zbus(property, name = "OverlayIconName")]
    fn overlay_icon_name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "OverlayIconPixmap")]
    fn overlay_icon_pixmap(&self) -> zbus::Result<Vec<RawIconPixmap>>;
    #[zbus(property, name = "AttentionIconName")]
    fn attention_icon_name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "AttentionIconPixmap")]
    fn attention_icon_pixmap(&self) -> zbus::Result<Vec<RawIconPixmap>>;
    #[zbus(property, name = "ToolTip")]
    fn tool_tip(&self) -> zbus::Result<RawToolTip>;
    #[zbus(property, name = "ItemIsMenu")]
    fn item_is_menu(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "Menu")]
    fn menu(&self) -> zbus::Result<OwnedObjectPath>;
    /// Application icon directory, searched before the session theme.
    #[zbus(property, name = "IconThemePath")]
    fn icon_theme_path(&self) -> zbus::Result<String>;

    #[zbus(signal, name = "NewTitle")]
    fn new_title(&self);
    #[zbus(signal, name = "NewIcon")]
    fn new_icon(&self);
    #[zbus(signal, name = "NewAttentionIcon")]
    fn new_attention_icon(&self);
    #[zbus(signal, name = "NewOverlayIcon")]
    fn new_overlay_icon(&self);
    #[zbus(signal, name = "NewToolTip")]
    fn new_tool_tip(&self);
    #[zbus(signal, name = "NewStatus")]
    fn new_status(&self, status: String);
}

/// `GetLayout`'s `(ia{sv}av)` reply, decoded by `#[derive(Type, Deserialize)]` rather than
/// `OwnedValue`. `Body::deserialize` checks the Rust signature, while `OwnedValue` is always `"v"`
/// and would reject the real signature with a mismatch. `properties`/`children` remain
/// `OwnedValue` after successful decoding for [`parse_menu_node`] to walk.
#[derive(Debug, Deserialize, Type)]
pub(super) struct RawMenuLayout {
    id: i32,
    properties: HashMap<String, OwnedValue>,
    children: Vec<OwnedValue>,
}

/// Rebuilds the `Value::Structure` expected by [`parse_menu_node`] so the top-level reply and
/// recursive children share one parser.
pub(super) fn raw_menu_layout_to_value(raw: RawMenuLayout) -> Value<'static> {
    let mut properties = Dict::new(&Signature::Str, &Signature::Variant);
    for (key, value) in raw.properties {
        // The dict declares variant values (`"v"`); only `Value::Value(Box<Value>)` has that
        // signature. Explicit wrapping is required.
        properties
            .append(Value::Str(Str::from(key)), Value::Value(Box::new(Value::from(value))))
            .expect("Str key / explicitly-wrapped-variant value always matches this dict's own declared signature");
    }
    let mut children = Array::new(&Signature::Variant);
    for child in raw.children {
        children
            .append(Value::Value(Box::new(Value::from(child))))
            .expect("an explicitly-wrapped-variant element always matches this array's own declared signature");
    }
    let structure = StructureBuilder::new()
        .add_field(raw.id)
        .append_field(Value::Dict(properties))
        .append_field(Value::Array(children))
        .build()
        .expect("a 3-field (id, properties, children) structure is always well-formed");
    Value::Structure(structure)
}

#[zbus::proxy(interface = "com.canonical.dbusmenu")]
pub(super) trait DBusMenu {
    #[zbus(name = "GetLayout")]
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: &[&str],
    ) -> zbus::Result<(u32, RawMenuLayout)>;

    #[zbus(name = "Event")]
    fn event(&self, id: i32, event_id: &str, data: &Value<'_>, timestamp: u32) -> zbus::Result<()>;

    #[zbus(name = "AboutToShow")]
    fn about_to_show(&self, id: i32) -> zbus::Result<bool>;

    #[zbus(signal, name = "LayoutUpdated")]
    fn layout_updated(&self, revision: u32, parent: i32);
}

/// Calls `RegisterStatusNotifierHost` at the well-known watcher name, so D-Bus routes to whichever
/// process owns it (ADR-0031).
#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
pub(super) trait StatusNotifierWatcherClient {
    #[zbus(name = "RegisterStatusNotifierHost")]
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;
}

/// Binds to `destination`, not the item's unique name: Chromium answers only its registered
/// well-known name (ADR-0072).
/// Uncached, like `battery::controller`'s UPower proxy and for a sharper reason: an item announces
/// a changed icon with SNI's own `NewIcon`, never `PropertiesChanged`, which is the only thing
/// zbus's default lazy cache invalidates on. `registry::spawn_item_signal_forwarder` re-reads
/// `IconName` when `NewIcon` fires, and with a cache it would be handed the value from bind time
/// every time.
///
/// libayatana-appindicator is why this is not merely stale but broken: it renumbers its icon file
/// on every update (`tray-icon-<app>-0.png` to `-1.png` ...) and unlinks the old one, so a cached
/// name is a path that no longer exists and the item paints an empty square. Seen with
/// `yerd-gui`, which rotates within a second of launch.
pub(super) async fn bind_item(
    connection: &zbus::Connection,
    destination: &OwnedBusName,
    path: &OwnedObjectPath,
) -> zbus::Result<StatusNotifierItemProxy<'static>> {
    StatusNotifierItemProxy::builder(connection)
        .destination(destination.clone())?
        .path(path.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

pub(super) async fn bind_dbusmenu(
    connection: &zbus::Connection,
    destination: &OwnedBusName,
    path: &OwnedObjectPath,
) -> zbus::Result<DBusMenuProxy<'static>> {
    DBusMenuProxy::builder(connection).destination(destination.clone())?.path(path.clone())?.build().await
}

#[cfg(test)]
mod tests {
    use super::super::menu::parse_menu_node;
    use super::*;

    // ---- RawMenuLayout / raw_menu_layout_to_value ----

    #[test]
    fn raw_menu_layout_signature_matches_the_real_dbusmenu_wire_shape() {
        // The DBusMenu wire signature must match or every real `GetLayout` fails, even if local
        // tests pass.
        assert_eq!(RawMenuLayout::SIGNATURE.to_string(), "(ia{sv}av)");
    }

    #[test]
    fn raw_menu_layout_to_value_round_trips_through_parse_menu_node() {
        let mut properties = HashMap::new();
        properties.insert("label".to_string(), OwnedValue::try_from(Value::Str(Str::from("Quit"))).unwrap());
        properties.insert("enabled".to_string(), OwnedValue::try_from(Value::Bool(false)).unwrap());

        let child = RawMenuLayout { id: 11, properties: HashMap::new(), children: Vec::new() };
        let child_value = raw_menu_layout_to_value(child);
        let root = RawMenuLayout { id: 0, properties, children: vec![OwnedValue::try_from(child_value).unwrap()] };

        let root_value = raw_menu_layout_to_value(root);
        let item = parse_menu_node(&root_value, 0, &mut { crate::capabilities::tray::MAX_MENU_NODES })
            .expect("must parse a reconstructed layout");

        assert_eq!(item.id, 0);
        assert_eq!(item.label, Some("Quit".to_string()));
        assert!(!item.enabled);
        assert_eq!(item.children.len(), 1);
        assert_eq!(item.children[0].id, 11);
    }
}
