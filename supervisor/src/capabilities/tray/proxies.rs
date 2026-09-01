//! Hand-written proxies for `org.kde.StatusNotifierItem`, `com.canonical.dbusmenu`, and
//! `org.kde.StatusNotifierWatcher` (ADR-0031: no maintained zbus proxy crate for SNI/DBusMenu).
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
    /// Middle-click. A separate method in the spec rather than a flag on `Activate`, and Telegram,
    /// Chromium and Qt's own tray all export it.
    #[zbus(name = "SecondaryActivate")]
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// A scroll over the icon. `orientation` is the spec's `"vertical"` or `"horizontal"`, and
    /// `delta` its sign and magnitude, which is how a media player takes volume off the tray.
    #[zbus(name = "Scroll")]
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

    #[zbus(property, name = "Id")]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Category")]
    fn category(&self) -> zbus::Result<String>;
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
    #[zbus(property, name = "WindowId")]
    fn window_id(&self) -> zbus::Result<i32>;
    /// A directory the application ships its own icons in, to be searched before the session theme.
    /// Set by applications that bundle artwork the theme has never heard of, which is most of the
    /// packaged-runtime ones.
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

/// `GetLayout`'s `(ia{sv}av)` reply structure, decoded field-by-field via a real
/// `#[derive(Type, Deserialize)]` struct rather than a bare `zvariant::OwnedValue`. This
/// matters for correctness, not just style: `Body::deserialize` checks the declared Rust
/// type's own static signature against the real message's signature, and `OwnedValue`'s
/// signature is always `"v"` (a bare variant), which does not equal the real wire signature
/// `"(ia{sv}av)"` -- a proxy method declared to return `(u32, OwnedValue)` would fail every
/// real `GetLayout` call with a signature-mismatch error. `properties`/`children` stay
/// `OwnedValue`-typed; their contents are already fully decoded once this struct
/// deserializes successfully, which is what [`parse_menu_node`] walks.
#[derive(Debug, Deserialize, Type)]
pub(super) struct RawMenuLayout {
    id: i32,
    properties: HashMap<String, OwnedValue>,
    children: Vec<OwnedValue>,
}

/// Reconstructs the `zvariant::Value::Structure` shape [`parse_menu_node`] expects from an
/// already-decoded [`RawMenuLayout`] -- lets the top-level `GetLayout` reply and every
/// recursive child share the exact same parsing logic instead of duplicating it.
pub(super) fn raw_menu_layout_to_value(raw: RawMenuLayout) -> Value<'static> {
    let mut properties = Dict::new(&Signature::Str, &Signature::Variant);
    for (key, value) in raw.properties {
        // The dict's own declared value signature is `Variant` ("v") -- `Dict::append` checks
        // the inserted value's *own* `value_signature()` against that, which is only ever `"v"`
        // for a `Value::Value(Box<Value>)` (every other variant's `value_signature()` is its own
        // concrete type, e.g. `Value::Str(_)` -> `"s"`). Explicit wrap required, not optional.
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

/// Client-side proxy for calling `RegisterStatusNotifierHost` against whichever process ends up
/// owning `org.kde.StatusNotifierWatcher` (ADR-0031's "dual-role dance") -- addressed at the
/// well-known name, not a resolved unique name, so D-Bus routing delivers the call to the real
/// owner regardless of whether that's this process or a real DE's tray host.
#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
pub(super) trait StatusNotifierWatcherClient {
    #[zbus(name = "RegisterStatusNotifierHost")]
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;
}

/// `destination` rather than the item's unique name: a Chromium tray item answers only the
/// well-known name it registered under (docs/adr/0072).
pub(super) async fn bind_item(
    connection: &zbus::Connection,
    destination: &OwnedBusName,
    path: &OwnedObjectPath,
) -> zbus::Result<StatusNotifierItemProxy<'static>> {
    StatusNotifierItemProxy::builder(connection).destination(destination.clone())?.path(path.clone())?.build().await
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
        // GetLayout's real reply signature (the DBusMenu spec's own "(ia{sv}av)") -- a mismatch
        // here means every real GetLayout call would fail with a signature-mismatch error
        // despite this module's own tests passing.
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
        let item = parse_menu_node(&root_value, 0).expect("must parse a reconstructed layout");

        assert_eq!(item.id, 0);
        assert_eq!(item.label, Some("Quit".to_string()));
        assert!(!item.enabled);
        assert_eq!(item.children.len(), 1);
        assert_eq!(item.children[0].id, 11);
    }
}
