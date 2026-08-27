//! System tray host (`oblisk.tray`, docs/oblisk-supervisor-services-dbus.md §2;
//! docs/oblisk-idl-api-specs.md §2.14; docs/adr/0031).
//!
//! Hosts `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` and client-handles every
//! registered `org.kde.StatusNotifierItem` (plus its optional `com.canonical.dbusmenu` menu).
//! Mirrors `dbus::bluetooth`'s shapes throughout: hand-written `#[zbus::proxy]` traits (ADR-0031:
//! no crate reuse -- `system-tray` has a source-verified pixmap-squaring bug and wouldn't save
//! the security-critical bounds-checking/PNG-encoding work this controller has to do regardless),
//! a `*Controller` struct holding the proxies/registry write actions need, a `HashMap<Key, Entry>`
//! dynamic per-object registry hydrated live and kept live via per-item forwarder tasks (one
//! `JoinHandle` per tracked object, aborted on removal), and pure pretty-printable/parsing helper
//! functions unit-testable without a live D-Bus connection.
//!
//! Unlike BlueZ's `ObjectManager`-driven liveness (`InterfacesRemoved`), the base SNI spec has no
//! signal telling a host when a client unregisters -- liveness is tracked via
//! `org.freedesktop.DBus.NameOwnerChanged`: one global forwarder task removes every registry entry
//! for a unique name the instant that name drops off the bus (see
//! [`spawn_name_owner_changed_forwarder`]).
//!
//! ponytail: `TrayController::new` never fails outright, same reasoning as
//! `BluetoothController::new` (docs/adr/0030) -- a session with no other tray host running (the
//! overwhelmingly common case for niri/sway) is not an error, and `RequestName` losing the race to
//! an already-running DE tray (Plasma/GNOME) is an expected, handled outcome (ADR-0031's "dual-role
//! dance"), not a startup failure either.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use enumflags2::BitFlags;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use zbus::fdo::RequestNameFlags;
use zbus::names::{BusName, OwnedUniqueName, WellKnownName};
use zbus::zvariant::{Array, Dict, OwnedObjectPath, OwnedValue, Signature, Str, StructureBuilder, Type, Value};

use super::shm_icons::{self, PngEncodeError};

/// Well-known bus name and object path this controller hosts `org.kde.StatusNotifierWatcher` at
/// (docs/oblisk-supervisor-services-dbus.md §2's literal path).
const WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";
/// `RegisterStatusNotifierItem`'s `service` argument, when it names a bus name rather than an
/// object path, always means this fixed item object path (ADR-0031).
const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";
/// ARGB pixmaps are rejected above this size (docs/oblisk-supervisor-services-dbus.md §2.1,
/// ADR-0031: "largest available pixmap capped at the spec's 128px limit").
const MAX_PIXMAP_DIMENSION: i32 = 128;

/// `IconPixmap`'s D-Bus wire shape (`a(iiay)`): width, height, raw ARGB32 bytes.
type RawIconPixmap = (i32, i32, Vec<u8>);
/// `ToolTip`'s D-Bus wire shape (`(sa(iiay)ss)`): icon name, icon pixmaps, title, text.
type RawToolTip = (String, Vec<RawIconPixmap>, String, String);

// -------------------------------------------------------------------------------------------
// Hand-written proxies (ADR-0031: no maintained zbus proxy crate for SNI/DBusMenu).
// -------------------------------------------------------------------------------------------

#[zbus::proxy(interface = "org.kde.StatusNotifierItem")]
trait StatusNotifierItem {
    #[zbus(name = "Activate")]
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;

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

/// `GetLayout`'s `(ia{sv}av)` reply structure, decoded field-by-field via a real `#[derive(Type,
/// Deserialize)]` struct (mirrors `zbus_polkit::policykit1::TemporaryAuthorization`'s own use of
/// this exact pattern for a mixed-field D-Bus structure) rather than declaring the return type as
/// a bare `zvariant::OwnedValue`. This matters for correctness, not just style: `Body::deserialize`
/// checks the *declared Rust type's own static signature* against the real message's signature
/// (`zvariant::DynamicDeserialize`'s blanket impl, `zvariant-5.15.0/src/type/dynamic.rs`) --
/// `OwnedValue`'s own signature is always `"v"` (a bare variant), which does not equal the real
/// wire signature `"(ia{sv}av)"`, so a proxy method declared to return `(u32, OwnedValue)` would
/// fail every real `GetLayout` call with a signature-mismatch error. `properties`/`children` stay
/// `OwnedValue`-typed (matching `a{sv}`/`av`'s own per-element `"v"` typing exactly) -- their
/// *contents* are already fully decoded in memory once this struct itself deserializes
/// successfully, which is what [`parse_menu_node`] walks.
#[derive(Debug, Deserialize, Type)]
struct RawMenuLayout {
    id: i32,
    properties: HashMap<String, OwnedValue>,
    children: Vec<OwnedValue>,
}

/// Reconstructs the `zvariant::Value::Structure` shape [`parse_menu_node`] expects from an
/// already-decoded [`RawMenuLayout`] -- lets the top-level `GetLayout` reply and every recursive
/// child (themselves already-decoded `Value::Structure`s once unwrapped, see
/// [`parse_menu_node`]'s own children handling) share the exact same parsing logic instead of
/// duplicating it for "the first level" vs. "every level after that".
fn raw_menu_layout_to_value(raw: RawMenuLayout) -> Value<'static> {
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
trait DBusMenu {
    #[zbus(name = "GetLayout")]
    fn get_layout(&self, parent_id: i32, recursion_depth: i32, property_names: &[&str]) -> zbus::Result<(u32, RawMenuLayout)>;

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
trait StatusNotifierWatcherClient {
    #[zbus(name = "RegisterStatusNotifierHost")]
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;
}

async fn bind_item(connection: &zbus::Connection, unique_name: &OwnedUniqueName, path: &OwnedObjectPath) -> zbus::Result<StatusNotifierItemProxy<'static>> {
    StatusNotifierItemProxy::builder(connection).destination(unique_name.clone())?.path(path.clone())?.build().await
}

async fn bind_dbusmenu(connection: &zbus::Connection, unique_name: &OwnedUniqueName, path: &OwnedObjectPath) -> zbus::Result<DBusMenuProxy<'static>> {
    DBusMenuProxy::builder(connection).destination(unique_name.clone())?.path(path.clone())?.build().await
}

// -------------------------------------------------------------------------------------------
// Registration-string resolution (ADR-0031: pure classification, TDD'd; the D-Bus round trip
// for the well-known-name branch is a thin async wrapper around it).
// -------------------------------------------------------------------------------------------

/// How `RegisterStatusNotifierItem`'s raw `service` argument classifies, before any D-Bus I/O
/// (ADR-0031). Pure and total: every `&str` lands in exactly one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistrationTarget {
    /// `service` was an object path (`service.starts_with('/')`) -- the bus name comes from the
    /// calling message's own sender, not from `service` itself.
    ObjectPathFromSender { object_path: String },
    /// `service` was already a unique name (`:N.M`) -- used directly.
    UniqueName { unique_name: String },
    /// `service` was a well-known bus name -- needs a `GetNameOwner` round trip (done by the
    /// caller) to resolve to a unique name.
    WellKnownName { well_known_name: String },
}

fn classify_service_arg(service: &str) -> RegistrationTarget {
    if service.starts_with('/') {
        RegistrationTarget::ObjectPathFromSender { object_path: service.to_string() }
    } else if service.starts_with(':') {
        RegistrationTarget::UniqueName { unique_name: service.to_string() }
    } else {
        RegistrationTarget::WellKnownName { well_known_name: service.to_string() }
    }
}

#[derive(Debug)]
enum RegistrationError {
    NoSender,
    InvalidName(String),
    Dbus(String),
    /// `service` was already a unique name (`:N.M`) but didn't equal the real, authenticated
    /// sender of this `RegisterStatusNotifierItem` call (Correctness review: a connection can only
    /// ever truthfully claim its own unique name -- `header.sender()` is filled in by the bus
    /// daemon itself and cannot be spoofed by the calling process, so any mismatch here means the
    /// caller fabricated a unique name it doesn't own).
    UniqueNameMismatch { claimed: String, sender: String },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSender => write!(f, "an object-path service argument requires a message sender, but none was present"),
            Self::InvalidName(err) => write!(f, "invalid D-Bus name: {err}"),
            Self::Dbus(err) => write!(f, "{err}"),
            Self::UniqueNameMismatch { claimed, sender } => {
                write!(f, "claimed unique name {claimed:?} does not match the real sender {sender:?}")
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

/// Resolves `RegisterStatusNotifierItem`'s `service` argument plus the calling message's sender
/// into `(unique_name, object_path)` -- the registry key this controller actually uses
/// (ADR-0031's "registry keys on the resolved D-Bus unique name" decision). The well-known-name
/// branch is the only one that performs I/O (`GetNameOwner`); the other two are resolved
/// synchronously from already-known data.
async fn resolve_registration(connection: &zbus::Connection, service: &str, sender: Option<&str>) -> Result<(OwnedUniqueName, OwnedObjectPath), RegistrationError> {
    match classify_service_arg(service) {
        RegistrationTarget::ObjectPathFromSender { object_path } => {
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            let unique_name = OwnedUniqueName::try_from(sender).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let object_path = OwnedObjectPath::try_from(object_path).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            Ok((unique_name, object_path))
        }
        RegistrationTarget::UniqueName { unique_name } => {
            // A connection can only ever truthfully claim its own real unique name (Correctness
            // review, security bug: unbounded ghost-registry DoS) -- reject any claimed unique
            // name that doesn't equal the real, bus-daemon-authenticated sender of this call. A
            // well-behaved client's self-claimed unique name is always its own, so this rejects
            // nothing legitimate; it only closes the fabrication vector (any session-bus peer
            // could otherwise call `RegisterStatusNotifierItem(":999.1")` repeatedly with distinct
            // made-up names, each planting a registry entry with forwarder tasks that can never be
            // cleaned up, since `NameOwnerChanged`-based removal only fires for a name that was
            // ever real and then genuinely disconnects).
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            if unique_name != sender {
                return Err(RegistrationError::UniqueNameMismatch { claimed: unique_name, sender: sender.to_string() });
            }
            let unique_name = OwnedUniqueName::try_from(unique_name).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            Ok((unique_name, default_item_object_path()))
        }
        RegistrationTarget::WellKnownName { well_known_name } => {
            let dbus_proxy = zbus::fdo::DBusProxy::new(connection).await.map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            let well_known = WellKnownName::try_from(well_known_name).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let owner = dbus_proxy.get_name_owner(BusName::WellKnown(well_known)).await.map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            Ok((owner, default_item_object_path()))
        }
    }
}

fn default_item_object_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from(DEFAULT_ITEM_OBJECT_PATH).expect("DEFAULT_ITEM_OBJECT_PATH is a valid object path literal")
}

/// The leading `:` stripped from a `:N.M` unique name -- both the internal Lua-facing `id` and
/// the PNG spool filename component (ADR-0031). D-Bus guarantees a unique name contains only
/// digits/colons/dots, so no further sanitization is needed.
fn sanitize_unique_name(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').to_string()
}

// -------------------------------------------------------------------------------------------
// Icon pipeline (ADR-0031: prefer IconName, decode IconPixmap only as fallback; pure functions).
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct IconPixmap {
    width: i32,
    height: i32,
    bytes: Vec<u8>,
}

/// Bounds-checks one raw `IconPixmap` (docs/oblisk-supervisor-services-dbus.md §2.1): square,
/// non-empty, capped at [`MAX_PIXMAP_DIMENSION`], and its byte length matches `width * height *
/// 4` (ARGB32, 4 bytes/pixel) exactly.
fn pixmap_is_valid(width: i32, height: i32, byte_len: usize) -> bool {
    width > 0 && width == height && width <= MAX_PIXMAP_DIMENSION && (width as usize) * (height as usize) * 4 == byte_len
}

/// The single largest pixmap that passes [`pixmap_is_valid`] (ADR-0031: "no target-size guess,
/// largest capped at 128" -- downscaling a large source always beats upscaling a small one).
fn largest_valid_pixmap(pixmaps: &[IconPixmap]) -> Option<&IconPixmap> {
    pixmaps.iter().filter(|pixmap| pixmap_is_valid(pixmap.width, pixmap.height, pixmap.bytes.len())).max_by_key(|pixmap| pixmap.width)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IconSource {
    /// `IconName` was non-empty -- preferred (ADR-0031), skips the whole decode/PNG-spool
    /// pipeline.
    Name(String),
    /// `IconName` was empty but at least one pixmap passed bounds-checking.
    Pixmap,
    /// Neither source is usable.
    None,
}

/// Which icon source to use, and whether a pixmap decode is even necessary (ADR-0031's
/// IconName-preference decision) -- kept separate from the pixmap *value* itself so this stays a
/// cheap, pure decision function; the caller re-derives the actual largest pixmap via
/// [`largest_valid_pixmap`] only when this returns [`IconSource::Pixmap`].
fn resolve_icon_source(icon_name: &str, pixmaps: &[IconPixmap]) -> IconSource {
    if !icon_name.is_empty() {
        IconSource::Name(icon_name.to_string())
    } else if largest_valid_pixmap(pixmaps).is_some() {
        IconSource::Pixmap
    } else {
        IconSource::None
    }
}

/// Encodes a bounds-checked ARGB32 (network byte order: A, R, G, B per pixel) buffer to a PNG
/// byte stream via the `png` crate (ADR-0031: pure Rust, encode-only, minimal dependency tree).
fn encode_argb32_to_png(width: u32, height: u32, argb: &[u8]) -> Result<Vec<u8>, PngEncodeError> {
    let mut rgba = Vec::with_capacity(argb.len());
    let (chunks, _remainder) = argb.as_chunks::<4>();
    for &[a, r, g, b] in chunks {
        rgba.push(r);
        rgba.push(g);
        rgba.push(b);
        rgba.push(a);
    }

    let mut buffer = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buffer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(PngEncodeError::Png)?;
        writer.write_image_data(&rgba).map_err(PngEncodeError::Png)?;
    }
    Ok(buffer)
}

/// Writes `pixmap` (already bounds-checked) as a PNG to
/// `/dev/shm/oblisk-$UID/tray/{sanitized_unique_name}.png` ([`shm_icons::write_png`]), creating the
/// directory tree if missing. Same path overwritten in place on every call -- no cache-busting
/// (ADR-0031).
fn write_icon_png(sanitized_unique_name: &str, pixmap: &IconPixmap) -> std::io::Result<String> {
    let png_bytes = encode_argb32_to_png(pixmap.width as u32, pixmap.height as u32, &pixmap.bytes).map_err(std::io::Error::other)?;
    shm_icons::write_png("tray", &format!("{sanitized_unique_name}.png"), &png_bytes)
}

// -------------------------------------------------------------------------------------------
// DBusMenu layout parsing (ADR-0031: parsed by hand, recursively, from the raw zvariant Value).
// -------------------------------------------------------------------------------------------

/// One node of a DBusMenu layout tree, already resolved into what `tray.items[].menu` needs
/// (docs/oblisk-idl-api-specs.md §2.14).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MenuItem {
    pub id: i32,
    pub menu_type: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub icon_name: Option<String>,
    pub toggle_type: Option<String>,
    pub toggle_state: Option<i32>,
    pub children: Vec<MenuItem>,
}

/// Unwraps a nested D-Bus variant (`Value::Value(Box<Value>)`) down to the real payload --
/// DBusMenu's `av` (array-of-variant) children come back this way, one variant layer per
/// element (mirrors `zvariant::Value::downcast`'s own unwrap-one-`Value::Value`-layer handling).
fn unwrap_variant<'a>(value: &'a Value<'_>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => unwrap_variant(inner),
        other => other,
    }
}

fn value_as_str<'a>(value: &'a Value<'_>) -> Option<&'a str> {
    match unwrap_variant(value) {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn value_as_bool(value: &Value<'_>) -> Option<bool> {
    match unwrap_variant(value) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn value_as_i32(value: &Value<'_>) -> Option<i32> {
    match unwrap_variant(value) {
        Value::I32(i) => Some(*i),
        _ => None,
    }
}

fn dict_str_key<'a>(key: &'a Value<'_>) -> Option<&'a str> {
    match key {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Hard cap on [`parse_menu_node`]'s own recursion depth (Correctness review: a `GetLayout`
/// reply's tree structure is entirely controlled by whichever process registered the tray item's
/// `Menu` object -- any session-bus peer -- so a well-formed but deeply nested reply, each level a
/// trivial, legally-encoded 3-field structure easily within normal D-Bus message size limits,
/// could otherwise stack-overflow this task via unbounded Rust recursion). Generous for any real,
/// human-authored menu (DBusMenu trees nested more than a handful of levels deep don't happen in
/// practice) while staying well under any realistic native-stack-overflow threshold.
const MAX_MENU_DEPTH: u32 = 32;

/// Parses one `(ia{sv}av)`-shaped DBusMenu layout node -- `id`, its properties dict, and its
/// `av` children array -- recursively into a [`MenuItem`] tree. `None` on any structural
/// mismatch (not the real DBusMenu wire shape this was built against); a missing/malformed
/// property falls back to its DBusMenu spec default rather than failing the whole node.
///
/// `depth` is this node's own recursion depth (`0` for the tree's root, `fetch_menu_via`'s only
/// caller). At [`MAX_MENU_DEPTH`], this node itself still parses normally (id/properties), but its
/// `children` are truncated to empty instead of recursing further -- logged, since a legitimate
/// app should never hit this.
fn parse_menu_node(value: &Value<'_>, depth: u32) -> Option<MenuItem> {
    let structure = match unwrap_variant(value) {
        Value::Structure(structure) => structure,
        _ => return None,
    };
    let [id_field, properties_field, children_field] = structure.fields() else {
        return None;
    };
    let id = value_as_i32(id_field)?;

    let mut menu_type = "standard".to_string();
    let mut label = None;
    let mut enabled = true;
    let mut icon_name = None;
    let mut toggle_type = None;
    let mut toggle_state_raw = None;
    if let Value::Dict(dict) = unwrap_variant(properties_field) {
        for (key, val) in dict.iter() {
            match dict_str_key(key) {
                Some("type") => {
                    if let Some(s) = value_as_str(val) {
                        menu_type = s.to_string();
                    }
                }
                Some("label") => label = value_as_str(val).map(str::to_string),
                Some("enabled") => {
                    if let Some(b) = value_as_bool(val) {
                        enabled = b;
                    }
                }
                Some("icon-name") => icon_name = value_as_str(val).filter(|s| !s.is_empty()).map(str::to_string),
                Some("toggle-type") => toggle_type = value_as_str(val).filter(|s| !s.is_empty()).map(str::to_string),
                Some("toggle-state") => toggle_state_raw = value_as_i32(val),
                _ => {}
            }
        }
    }
    let toggle_state = toggle_type.as_ref().map(|_| toggle_state_raw.unwrap_or(-1));

    let children = if depth >= MAX_MENU_DEPTH {
        eprintln!("tray: GetLayout reply exceeded the maximum menu depth ({MAX_MENU_DEPTH}) at node id {id}; truncating its children");
        Vec::new()
    } else {
        match unwrap_variant(children_field) {
            Value::Array(array) => array.iter().filter_map(|child| parse_menu_node(child, depth + 1)).collect(),
            _ => Vec::new(),
        }
    };

    Some(MenuItem { id, menu_type, label, enabled, icon_name, toggle_type, toggle_state, children })
}

async fn fetch_menu_via(menu: &DBusMenuProxy<'static>) -> zbus::Result<Vec<MenuItem>> {
    let (_, raw_root) = menu.get_layout(0, -1, &[]).await?;
    let root_value = raw_menu_layout_to_value(raw_root);
    Ok(parse_menu_node(&root_value, 0).map(|root| root.children).unwrap_or_default())
}

// -------------------------------------------------------------------------------------------
// TrayItem hydration.
// -------------------------------------------------------------------------------------------

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
/// [`fetch_menu_via`] -- since the caller reuses an already-bound [`DBusMenuProxy`] rather than
/// re-resolving `Menu`'s object path on every refresh). A property read failure degrades to that
/// property's empty/default value rather than failing the whole item, matching this codebase's
/// established `unwrap_or_default`/`unwrap_or(false)` discipline (`dbus::bluetooth`).
async fn fetch_tray_item_base(item: &StatusNotifierItemProxy<'static>, unique_name: &OwnedUniqueName) -> TrayItem {
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

// -------------------------------------------------------------------------------------------
// Registry.
// -------------------------------------------------------------------------------------------

struct ItemEntry {
    item: StatusNotifierItemProxy<'static>,
    menu: Option<DBusMenuProxy<'static>>,
    last_known: TrayItem,
    properties_forwarder: JoinHandle<()>,
    menu_forwarder: Option<JoinHandle<()>>,
}

type ItemKey = (OwnedUniqueName, OwnedObjectPath);
type ItemRegistry = Arc<Mutex<HashMap<ItemKey, ItemEntry>>>;

/// Binds `unique_name`/`object_path` as a `StatusNotifierItem`, hydrates its full [`TrayItem`]
/// (including its menu tree, if it has one -- ADR-0031's "eager top-level fetch"), spawns its
/// signal forwarder(s), and inserts the resulting entry into `registry`. Used both by
/// `RegisterStatusNotifierItem` and, in principle, by any future re-registration path. Aborts and
/// replaces a prior entry at the same key rather than leaking its forwarder tasks (mirrors
/// `dbus::bluetooth::register_device`'s own `insert`-returns-previous handling).
async fn register_item(
    connection: &zbus::Connection,
    registry: &ItemRegistry,
    events: &UnboundedSender<TraySignal>,
    unique_name: OwnedUniqueName,
    object_path: OwnedObjectPath,
) -> Result<(), String> {
    let item = bind_item(connection, &unique_name, &object_path).await.map_err(|err| format!("failed to bind StatusNotifierItem: {err}"))?;

    let mut tray_item = fetch_tray_item_base(&item, &unique_name).await;

    let menu_path = item.menu().await.ok();
    let menu = match &menu_path {
        Some(path) if !path.as_str().is_empty() && path.as_str() != "/" => match bind_dbusmenu(connection, &unique_name, path).await {
            Ok(menu) => Some(menu),
            Err(err) => {
                eprintln!("tray: failed to bind DBusMenu for {unique_name} at {path}: {err}");
                None
            }
        },
        _ => None,
    };
    if let Some(menu) = &menu {
        match fetch_menu_via(menu).await {
            Ok(items) => tray_item.menu = Some(items),
            Err(err) => eprintln!("tray: GetLayout failed for {unique_name}: {err}"),
        }
    }

    let key: ItemKey = (unique_name.clone(), object_path.clone());

    // Narrow TOCTOU guard (Correctness review): every `.await` above (property reads, an
    // optional GetLayout) is a window in which the registering connection could have
    // disconnected -- NameOwnerChanged-based cleanup (spawn_name_owner_changed_forwarder) only
    // ever removes an entry that already exists, so a disconnect landing in that window would
    // otherwise plant an unreachable ghost entry no later signal can ever remove (a narrower
    // version of the fabricated-unique-name bug resolve_registration's UniqueName branch now
    // rejects -- but for a connection that legitimately existed and then genuinely disconnected
    // mid-registration, not a fabricated one). One more liveness check, right here before the
    // insert below (nothing else `.await`s between this and it), narrows that whole multi-await
    // window down to a single check-then-insert. Reuses the same org.freedesktop.DBus mechanism
    // resolve_registration's WellKnownName branch already uses for GetNameOwner. Best-effort: a
    // failure to even ask (proxy bind or the call itself erroring) proceeds with the insert rather
    // than blocking a legitimate registration on an unrelated D-Bus hiccup -- this narrows the
    // race, it doesn't need to be perfect.
    if let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(connection).await {
        match dbus_proxy.name_has_owner(BusName::from(unique_name.clone())).await {
            Ok(false) => {
                eprintln!("tray: {unique_name} disconnected during registration; not inserting a registry entry");
                return Ok(());
            }
            Ok(true) => {}
            Err(err) => eprintln!("tray: pre-insert liveness check for {unique_name} failed (proceeding anyway): {err}"),
        }
    }

    let properties_forwarder = spawn_item_signal_forwarder(item.clone(), unique_name.clone(), menu.clone(), key.clone(), registry.clone(), events.clone());
    let menu_forwarder = menu.clone().map(|menu| spawn_menu_signal_forwarder(menu, key.clone(), registry.clone(), events.clone()));

    let entry = ItemEntry { item, menu, last_known: tray_item, properties_forwarder, menu_forwarder };
    let previous = registry.lock().unwrap().insert(key, entry);
    if let Some(previous) = previous {
        previous.properties_forwarder.abort();
        if let Some(handle) = previous.menu_forwarder {
            handle.abort();
        }
    }
    let _ = events.send(TraySignal::RegistryChanged);
    Ok(())
}

/// Runs until every `NewX` signal stream ends, re-fetching the full [`TrayItem`] (base properties
/// plus, if `menu` is `Some`, a full menu-tree refetch via the already-bound proxy) on any of
/// them and updating the registry entry in place -- no debounce, no fine-grained per-property
/// patching (mirrors `dbus::bluetooth`/`dbus::network`'s established "full re-derivation on any
/// relevant event" discipline). One instance per tracked item; its `JoinHandle` lives in the
/// item's own [`ItemEntry`] and is aborted on unregistration.
fn spawn_item_signal_forwarder(
    item: StatusNotifierItemProxy<'static>,
    unique_name: OwnedUniqueName,
    menu: Option<DBusMenuProxy<'static>>,
    key: ItemKey,
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut new_title) = item.receive_new_title().await else { return };
        let Ok(mut new_icon) = item.receive_new_icon().await else { return };
        let Ok(mut new_attention_icon) = item.receive_new_attention_icon().await else { return };
        let Ok(mut new_overlay_icon) = item.receive_new_overlay_icon().await else { return };
        let Ok(mut new_tool_tip) = item.receive_new_tool_tip().await else { return };
        let Ok(mut new_status) = item.receive_new_status().await else { return };

        loop {
            let fired = tokio::select! {
                Some(_) = new_title.next() => true,
                Some(_) = new_icon.next() => true,
                Some(_) = new_attention_icon.next() => true,
                Some(_) = new_overlay_icon.next() => true,
                Some(_) = new_tool_tip.next() => true,
                Some(_) = new_status.next() => true,
                else => false,
            };
            if !fired {
                break;
            }

            let mut refreshed = fetch_tray_item_base(&item, &unique_name).await;
            if let Some(menu) = &menu {
                refreshed.menu = fetch_menu_via(menu).await.ok();
            }

            let mut guard = registry.lock().unwrap();
            let Some(entry) = guard.get_mut(&key) else { break };
            entry.last_known = refreshed;
            drop(guard);

            if events.send(TraySignal::RegistryChanged).is_err() {
                break;
            }
        }
    })
}

/// Runs until `LayoutUpdated` stops firing, re-fetching `menu`'s full layout and updating the
/// registry entry's `menu` field in place on every occurrence (ADR-0031: "`GetLayout`... is
/// re-fetched on `LayoutUpdated`"). One instance per tracked item that has a menu; aborted
/// alongside [`spawn_item_signal_forwarder`]'s handle on unregistration.
fn spawn_menu_signal_forwarder(menu: DBusMenuProxy<'static>, key: ItemKey, registry: ItemRegistry, events: UnboundedSender<TraySignal>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut layout_updated) = menu.receive_layout_updated().await else { return };
        while layout_updated.next().await.is_some() {
            match fetch_menu_via(&menu).await {
                Ok(items) => {
                    let mut guard = registry.lock().unwrap();
                    let Some(entry) = guard.get_mut(&key) else { break };
                    entry.last_known.menu = Some(items);
                    drop(guard);
                    if events.send(TraySignal::RegistryChanged).is_err() {
                        break;
                    }
                }
                Err(err) => eprintln!("tray: GetLayout (LayoutUpdated refresh) failed: {err}"),
            }
        }
    })
}

/// Runs until the underlying signal stream ends, removing every registry entry whose unique name
/// just dropped off the bus (`new_owner` empty) -- the base SNI spec has no
/// `UnregisterStatusNotifierItem` signal, so this is the only liveness signal a host has
/// (ADR-0031's module doc comment). One global subscription, not per-item.
fn spawn_name_owner_changed_forwarder(dbus_proxy: zbus::fdo::DBusProxy<'static>, registry: ItemRegistry, events: UnboundedSender<TraySignal>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut stream) = dbus_proxy.receive_name_owner_changed().await else { return };
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.new_owner.is_some() {
                continue;
            }
            let dropped_name = args.name.to_string();

            let removed: Vec<ItemEntry> = {
                let mut guard = registry.lock().unwrap();
                let stale_keys: Vec<ItemKey> = guard.keys().filter(|(unique_name, _)| unique_name.as_str() == dropped_name).cloned().collect();
                stale_keys.into_iter().filter_map(|key| guard.remove(&key)).collect()
            };
            if removed.is_empty() {
                continue;
            }
            for entry in removed {
                entry.properties_forwarder.abort();
                if let Some(handle) = entry.menu_forwarder {
                    handle.abort();
                }
            }
            if events.send(TraySignal::RegistryChanged).is_err() {
                break;
            }
        }
    })
}

// -------------------------------------------------------------------------------------------
// org.kde.StatusNotifierWatcher.
// -------------------------------------------------------------------------------------------

struct StatusNotifierWatcher {
    connection: zbus::Connection,
    registry: ItemRegistry,
    host_registered: Arc<Mutex<bool>>,
    events: UnboundedSender<TraySignal>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    #[zbus(name = "RegisterStatusNotifierItem")]
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string());
        let (unique_name, object_path) = resolve_registration(&self.connection, &service, sender.as_deref())
            .await
            .map_err(|err| zbus::fdo::Error::Failed(format!("RegisterStatusNotifierItem({service:?}) could not be resolved: {err}")))?;

        register_item(&self.connection, &self.registry, &self.events, unique_name.clone(), object_path)
            .await
            .map_err(|err| zbus::fdo::Error::Failed(format!("RegisterStatusNotifierItem({service:?}) failed: {err}")))?;

        let _ = emitter.status_notifier_item_registered(unique_name.as_str()).await;
        Ok(())
    }

    #[zbus(name = "RegisterStatusNotifierHost")]
    async fn register_status_notifier_host(&self, _service: String, #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>) {
        // Accepted trivially (ADR-0031): Oblisk is the only host that matters to this
        // controller's own item-registration logic; this exists for spec completeness (we may
        // be registering ourselves as our own host too, see TrayController::new).
        let was_registered = {
            let mut guard = self.host_registered.lock().unwrap();
            let was = *guard;
            *guard = true;
            was
        };
        if !was_registered {
            let _ = emitter.status_notifier_host_registered().await;
        }
    }

    #[zbus(property, name = "RegisteredStatusNotifierItems")]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registry.lock().unwrap().keys().map(|(unique_name, _)| unique_name.to_string()).collect()
    }

    #[zbus(property, name = "IsStatusNotifierHostRegistered")]
    async fn is_status_notifier_host_registered(&self) -> bool {
        *self.host_registered.lock().unwrap()
    }

    #[zbus(property, name = "ProtocolVersion")]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(signal_emitter: &zbus::object_server::SignalEmitter<'_>, service: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_item_unregistered(signal_emitter: &zbus::object_server::SignalEmitter<'_>, service: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_host_registered(signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_host_unregistered(signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

// -------------------------------------------------------------------------------------------
// State shape pushed as `oblisk.tray`'s StateSnapshot (docs/oblisk-idl-api-specs.md §2.14).
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TrayState {
    pub items: Vec<TrayItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraySignal {
    RegistryChanged,
}

#[derive(Debug)]
enum TrayActionError {
    UnknownItem,
    NoMenu,
}

impl std::fmt::Display for TrayActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownItem => write!(f, "no tray item with that id has been registered"),
            Self::NoMenu => write!(f, "that tray item has no registered dbusmenu"),
        }
    }
}

impl std::error::Error for TrayActionError {}

/// `tray:activate(id, x, y)`'s click-vs-menu gate (ADR-0031): an item with `ItemIsMenu == true`
/// must show its menu instead of activating -- enforced here, once, centrally, rather than
/// trusted to every `shell.lua` author.
fn should_call_activate(item_is_menu: bool) -> bool {
    !item_is_menu
}

fn unix_timestamp_u32() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// `tray:activate(id, x, y)`'s `arguments: [id, x, y]`.
pub fn parse_activate_args(arguments: &[serde_json::Value]) -> Option<(String, i32, i32)> {
    let id = arguments.first()?.as_str()?.to_string();
    let x = arguments.get(1)?.as_i64()? as i32;
    let y = arguments.get(2)?.as_i64()? as i32;
    Some((id, x, y))
}

/// `tray:activate_menu_item(id, menu_item_id)`'s `arguments: [id, menu_item_id]`.
pub fn parse_activate_menu_item_args(arguments: &[serde_json::Value]) -> Option<(String, i32)> {
    let id = arguments.first()?.as_str()?.to_string();
    let menu_item_id = arguments.get(1)?.as_i64()? as i32;
    Some((id, menu_item_id))
}

/// `tray:menu_will_show(id, submenu_id)`'s `arguments: [id, submenu_id]` -- same shape as
/// [`parse_activate_menu_item_args`], kept as a distinct function so each write action's parser
/// matches its own command name at the call site (mirrors `dbus::network`'s
/// `parse_connect_args`/`parse_ssid_arg` staying separate despite overlapping shapes).
pub fn parse_menu_will_show_args(arguments: &[serde_json::Value]) -> Option<(String, i32)> {
    parse_activate_menu_item_args(arguments)
}

// -------------------------------------------------------------------------------------------
// Controller.
// -------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct TrayController {
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
}

impl TrayController {
    /// Requests `org.kde.StatusNotifierWatcher` with no `ReplaceExisting`/`DoNotQueue` flags
    /// (ADR-0031's "dual-role dance"): with `DoNotQueue` unset, zbus 5's own
    /// `request_name_with_flags` never returns `Err(NameTaken)` for this case -- it queues
    /// instead, returning `Ok(RequestNameReply::InQueue)` (verified against zbus 5.19.0's
    /// `Connection::request_name_with_flags` source: `RequestNameReply::Exists` -- the only reply
    /// mapped to `Err`, is only reachable when `DoNotQueue` *is* set). So every `Ok` reply here
    /// (`PrimaryOwner`, `InQueue`, or `AlreadyOwner`) is a real success path; only a hard `Err`
    /// (e.g. no session bus at all) is logged as a genuine failure, and even that doesn't stop
    /// construction -- the Watcher object is still attached and `RegisterStatusNotifierHost` is
    /// still attempted, same "degrade to inert, don't take the Supervisor down" precedent
    /// `BluetoothController::new` already established for missing hardware/daemons.
    ///
    /// The `StatusNotifierWatcher` object is attached at [`WATCHER_OBJECT_PATH`] regardless of
    /// who ends up owning the name, then `RegisterStatusNotifierHost` is called against the
    /// well-known name itself (not a resolved unique name) -- D-Bus routing delivers that call to
    /// whichever process actually owns it, so this works identically whether this process won the
    /// name (a self-call, routed through the bus daemon back to this same object) or lost it to a
    /// real DE session already running one.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<TraySignal>) -> Self {
        match connection.request_name_with_flags(WATCHER_BUS_NAME, BitFlags::<RequestNameFlags>::empty()).await {
            Ok(reply) => eprintln!("tray: RequestName({WATCHER_BUS_NAME}) -> {reply}"),
            Err(err) => eprintln!("tray: RequestName({WATCHER_BUS_NAME}) failed: {err}"),
        }

        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let host_registered = Arc::new(Mutex::new(false));
        let watcher = StatusNotifierWatcher { connection: connection.clone(), registry: registry.clone(), host_registered, events: events.clone() };
        // Logged-and-continue, not `?`-propagated (Standards review): an export failure here must
        // not abort the whole Supervisor, same "degrade to inert" precedent `BluetoothController::
        // new` already established -- this controller still constructs and every other tray
        // functionality (or at minimum the rest of Supervisor) keeps working either way.
        if let Err(err) = connection.object_server().at(WATCHER_OBJECT_PATH, watcher).await {
            eprintln!("tray: failed to export StatusNotifierWatcher at {WATCHER_OBJECT_PATH}: {err}");
        }

        match StatusNotifierWatcherClientProxy::new(&connection).await {
            Ok(watcher_client) => {
                let our_unique_name = connection.unique_name().map(|name| name.to_string()).unwrap_or_default();
                if let Err(err) = watcher_client.register_status_notifier_host(&our_unique_name).await {
                    eprintln!("tray: RegisterStatusNotifierHost failed: {err}");
                }
            }
            Err(err) => eprintln!("tray: failed to bind the StatusNotifierWatcher client proxy for RegisterStatusNotifierHost: {err}"),
        }

        match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(dbus_proxy) => {
                spawn_name_owner_changed_forwarder(dbus_proxy, registry.clone(), events.clone());
            }
            Err(err) => eprintln!("tray: failed to bind org.freedesktop.DBus for NameOwnerChanged tracking: {err}"),
        }

        Self { registry, events }
    }

    /// Fully inert controller: empty registry, no forwarder tasks, nothing exported on any
    /// connection. Used when a dedicated session-bus connection for the tray host itself couldn't
    /// even be established -- same "degrade to inert, don't take the Supervisor down" precedent
    /// this module's own doc comment and `TrayController::new` already apply to a lost
    /// `RequestName` race; a session bus genuinely not being available in some environment is not
    /// a reason to abort Supervisor boot either. Every read/write action behaves exactly as it
    /// would against a live controller that simply has no tray items registered yet (`build_state`
    /// returns an empty `TrayState`, every `find_*` lookup misses).
    pub fn inert(events: UnboundedSender<TraySignal>) -> Self {
        Self { registry: Arc::new(Mutex::new(HashMap::new())), events }
    }

    /// Full, live re-derivation of `tray.items` from the entire tracked registry (mirrors
    /// `dbus::bluetooth::build_device_lists`'s "no debounce" discipline). Synchronous: every
    /// registry entry's `last_known` is already up to date (the forwarder tasks recompute it
    /// before ever sending a [`TraySignal`]), so no further D-Bus round trip is needed here.
    pub fn build_state(&self) -> TrayState {
        TrayState { items: self.registry.lock().unwrap().values().map(|entry| entry.last_known.clone()).collect() }
    }

    fn find_item_id(&self, id: &str) -> Option<(ItemKey, TrayItem)> {
        let guard = self.registry.lock().unwrap();
        guard.iter().find(|(key, _)| sanitize_unique_name(key.0.as_str()) == id).map(|(key, entry)| (key.clone(), entry.last_known.clone()))
    }

    fn find_item_proxy(&self, key: &ItemKey) -> Option<StatusNotifierItemProxy<'static>> {
        self.registry.lock().unwrap().get(key).map(|entry| entry.item.clone())
    }

    fn find_menu_proxy(&self, key: &ItemKey) -> Option<DBusMenuProxy<'static>> {
        self.registry.lock().unwrap().get(key).and_then(|entry| entry.menu.clone())
    }

    /// `tray:activate(id, x, y)`. No-ops (does not call the real `Activate`) when the item's
    /// `ItemIsMenu` is `true` -- SNI's own documented semantics, enforced centrally
    /// (ADR-0031, [`should_call_activate`]).
    pub async fn activate(&self, id: &str, x: i32, y: i32) {
        let Some((key, tray_item)) = self.find_item_id(id) else {
            eprintln!("tray: activate({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        if !should_call_activate(tray_item.item_is_menu) {
            return;
        }
        let Some(item) = self.find_item_proxy(&key) else {
            eprintln!("tray: activate({id:?}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        if let Err(err) = item.activate(x, y).await {
            eprintln!("tray: activate({id:?}) failed: {err}");
        }
    }

    /// `tray:activate_menu_item(id, menu_item_id)`: `DBusMenu.Event(menu_item_id, "clicked",
    /// &Value::I32(0), timestamp)` (ADR-0031).
    pub async fn activate_menu_item(&self, id: &str, menu_item_id: i32) {
        let Some((key, _)) = self.find_item_id(id) else {
            eprintln!("tray: activate_menu_item({id:?}, {menu_item_id}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        let Some(menu) = self.find_menu_proxy(&key) else {
            eprintln!("tray: activate_menu_item({id:?}, {menu_item_id}) failed: {}", TrayActionError::NoMenu);
            return;
        };
        let data = Value::I32(0);
        if let Err(err) = menu.event(menu_item_id, "clicked", &data, unix_timestamp_u32()).await {
            eprintln!("tray: activate_menu_item({id:?}, {menu_item_id}) failed: {err}");
        }
    }

    /// `tray:menu_will_show(id, submenu_id)`: calls `AboutToShow(submenu_id)` (DBusMenu's own
    /// lazy-population signal -- ADR-0031), then re-fetches and re-pushes the item's *entire*
    /// menu tree. A full re-fetch, not an in-place splice of just `submenu_id`'s own children:
    /// this codebase's established "no debounce, no incremental patching" discipline
    /// (`dbus::bluetooth`/`dbus::network`) applies here too, and menu trees are human-scale
    /// (ADR-0031's own "Consequences" section), so the extra round trip costs nothing a user
    /// would notice.
    pub async fn menu_will_show(&self, id: &str, submenu_id: i32) {
        let Some((key, _)) = self.find_item_id(id) else {
            eprintln!("tray: menu_will_show({id:?}, {submenu_id}) failed: {}", TrayActionError::UnknownItem);
            return;
        };
        let Some(menu) = self.find_menu_proxy(&key) else {
            eprintln!("tray: menu_will_show({id:?}, {submenu_id}) failed: {}", TrayActionError::NoMenu);
            return;
        };
        if let Err(err) = menu.about_to_show(submenu_id).await {
            eprintln!("tray: menu_will_show({id:?}, {submenu_id}) AboutToShow failed: {err}");
        }
        match fetch_menu_via(&menu).await {
            Ok(items) => {
                let mut guard = self.registry.lock().unwrap();
                if let Some(entry) = guard.get_mut(&key) {
                    entry.last_known.menu = Some(items);
                }
                drop(guard);
                let _ = self.events.send(TraySignal::RegistryChanged);
            }
            Err(err) => eprintln!("tray: menu_will_show({id:?}, {submenu_id}) GetLayout failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classify_service_arg ----

    #[test]
    fn classify_service_arg_recognizes_an_object_path() {
        assert_eq!(
            classify_service_arg("/org/example/StatusNotifierItem"),
            RegistrationTarget::ObjectPathFromSender { object_path: "/org/example/StatusNotifierItem".to_string() }
        );
    }

    #[test]
    fn classify_service_arg_recognizes_a_unique_name() {
        assert_eq!(classify_service_arg(":1.42"), RegistrationTarget::UniqueName { unique_name: ":1.42".to_string() });
    }

    #[test]
    fn classify_service_arg_recognizes_a_well_known_name() {
        assert_eq!(
            classify_service_arg("org.example.NmApplet"),
            RegistrationTarget::WellKnownName { well_known_name: "org.example.NmApplet".to_string() }
        );
    }

    // ---- sanitize_unique_name ----

    #[test]
    fn sanitize_unique_name_strips_the_leading_colon() {
        assert_eq!(sanitize_unique_name(":1.234"), "1.234");
    }

    #[test]
    fn sanitize_unique_name_is_a_no_op_without_a_leading_colon() {
        assert_eq!(sanitize_unique_name("1.234"), "1.234");
    }

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

    // ---- pixmap_is_valid ----

    #[test]
    fn pixmap_is_valid_accepts_a_well_formed_square_pixmap() {
        assert!(pixmap_is_valid(2, 2, 2 * 2 * 4));
    }

    #[test]
    fn pixmap_is_valid_rejects_a_non_square_pixmap() {
        assert!(!pixmap_is_valid(4, 2, 4 * 2 * 4));
    }

    #[test]
    fn pixmap_is_valid_rejects_oversized_pixmaps() {
        assert!(!pixmap_is_valid(129, 129, 129 * 129 * 4));
        assert!(pixmap_is_valid(128, 128, 128 * 128 * 4));
    }

    #[test]
    fn pixmap_is_valid_rejects_a_byte_length_mismatch() {
        assert!(!pixmap_is_valid(2, 2, 10));
    }

    #[test]
    fn pixmap_is_valid_rejects_non_positive_dimensions() {
        assert!(!pixmap_is_valid(0, 0, 0));
        assert!(!pixmap_is_valid(-1, -1, 4));
    }

    // ---- largest_valid_pixmap ----

    fn pixmap(size: i32) -> IconPixmap {
        IconPixmap { width: size, height: size, bytes: vec![0u8; (size * size * 4) as usize] }
    }

    #[test]
    fn largest_valid_pixmap_picks_the_biggest() {
        let pixmaps = vec![pixmap(16), pixmap(64), pixmap(32)];
        assert_eq!(largest_valid_pixmap(&pixmaps), Some(&pixmaps[1]));
    }

    #[test]
    fn largest_valid_pixmap_ignores_invalid_entries() {
        let oversized = IconPixmap { width: 200, height: 200, bytes: vec![0u8; 200 * 200 * 4] };
        let valid = pixmap(16);
        let pixmaps = vec![oversized, valid.clone()];
        assert_eq!(largest_valid_pixmap(&pixmaps), Some(&pixmaps[1]));
        assert_eq!(pixmaps[1], valid);
    }

    #[test]
    fn largest_valid_pixmap_is_none_when_every_entry_is_invalid() {
        let pixmaps = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(largest_valid_pixmap(&pixmaps), None);
    }

    #[test]
    fn largest_valid_pixmap_is_none_for_an_empty_list() {
        assert_eq!(largest_valid_pixmap(&[]), None);
    }

    // ---- resolve_icon_source ----

    #[test]
    fn resolve_icon_source_prefers_icon_name_even_with_pixmaps_present() {
        let pixmaps = vec![pixmap(32)];
        assert_eq!(resolve_icon_source("battery-full", &pixmaps), IconSource::Name("battery-full".to_string()));
    }

    #[test]
    fn resolve_icon_source_falls_back_to_pixmap_when_icon_name_is_empty() {
        let pixmaps = vec![pixmap(32)];
        assert_eq!(resolve_icon_source("", &pixmaps), IconSource::Pixmap);
    }

    #[test]
    fn resolve_icon_source_is_none_when_neither_is_usable() {
        assert_eq!(resolve_icon_source("", &[]), IconSource::None);
        let invalid = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(resolve_icon_source("", &invalid), IconSource::None);
    }

    // ---- encode_argb32_to_png (round trip through the real png crate, both encode and decode) ----

    #[test]
    fn encode_argb32_to_png_round_trips_a_known_pixel() {
        // One 1x1 pixel: A=0x11, R=0x22, G=0x33, B=0x44 (network byte order per the SNI spec).
        let argb = vec![0x11, 0x22, 0x33, 0x44];
        let png_bytes = encode_argb32_to_png(1, 1, &argb).expect("encoding must succeed");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("valid PNG frame");
        let rgba = &buf[..info.buffer_size()];

        // R, G, B, A -- the encoder must reorder from the source's A, R, G, B.
        assert_eq!(rgba, &[0x22, 0x33, 0x44, 0x11]);
    }

    // ---- parse_menu_node (against real zvariant Value/Structure literals, not a re-derivation) ----

    fn menu_node_value<'a>(id: i32, properties: Vec<(&'a str, Value<'a>)>, children: Vec<Value<'a>>) -> Value<'a> {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        for (key, value) in properties {
            dict.append(Value::Str(Str::from(key)), Value::Value(Box::new(value))).expect("dict insert must succeed in this test");
        }
        let mut array = Array::new(&Signature::Variant);
        for child in children {
            array.append(Value::Value(Box::new(child))).expect("array insert must succeed in this test");
        }
        let structure = StructureBuilder::new().add_field(id).append_field(Value::Dict(dict)).append_field(Value::Array(array)).build().expect("well-formed test structure");
        Value::Structure(structure)
    }

    #[test]
    fn parse_menu_node_parses_a_leaf_standard_item() {
        let value = menu_node_value(7, vec![("type", Value::Str(Str::from("standard"))), ("label", Value::Str(Str::from("Quit"))), ("enabled", Value::Bool(true))], vec![]);

        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.id, 7);
        assert_eq!(item.menu_type, "standard");
        assert_eq!(item.label, Some("Quit".to_string()));
        assert!(item.enabled);
        assert_eq!(item.icon_name, None);
        assert_eq!(item.toggle_type, None);
        assert_eq!(item.toggle_state, None);
        assert!(item.children.is_empty());
    }

    #[test]
    fn parse_menu_node_defaults_type_to_standard_and_enabled_to_true_when_absent() {
        let value = menu_node_value(1, vec![], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.menu_type, "standard");
        assert!(item.enabled);
        assert_eq!(item.label, None);
    }

    #[test]
    fn parse_menu_node_parses_a_separator() {
        let value = menu_node_value(2, vec![("type", Value::Str(Str::from("separator")))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.menu_type, "separator");
    }

    #[test]
    fn parse_menu_node_respects_enabled_false() {
        let value = menu_node_value(3, vec![("enabled", Value::Bool(false))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert!(!item.enabled);
    }

    #[test]
    fn parse_menu_node_parses_toggle_type_and_state() {
        let value = menu_node_value(4, vec![("toggle-type", Value::Str(Str::from("checkmark"))), ("toggle-state", Value::I32(1))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.toggle_type, Some("checkmark".to_string()));
        assert_eq!(item.toggle_state, Some(1));
    }

    #[test]
    fn parse_menu_node_defaults_toggle_state_to_negative_one_when_toggle_type_present_but_state_absent() {
        let value = menu_node_value(5, vec![("toggle-type", Value::Str(Str::from("radio")))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.toggle_type, Some("radio".to_string()));
        assert_eq!(item.toggle_state, Some(-1));
    }

    #[test]
    fn parse_menu_node_parses_icon_name() {
        let value = menu_node_value(6, vec![("icon-name", Value::Str(Str::from("edit-cut")))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.icon_name, Some("edit-cut".to_string()));
    }

    #[test]
    fn parse_menu_node_recurses_into_children() {
        let child_a = menu_node_value(11, vec![("label", Value::Str(Str::from("Copy")))], vec![]);
        let child_b = menu_node_value(12, vec![("label", Value::Str(Str::from("Paste")))], vec![]);
        let root = menu_node_value(0, vec![], vec![child_a, child_b]);

        let item = parse_menu_node(&root, 0).expect("must parse a well-formed node");
        assert_eq!(item.children.len(), 2);
        assert_eq!(item.children[0].id, 11);
        assert_eq!(item.children[0].label, Some("Copy".to_string()));
        assert_eq!(item.children[1].id, 12);
        assert_eq!(item.children[1].label, Some("Paste".to_string()));
    }

    #[test]
    fn parse_menu_node_recurses_multiple_levels_deep() {
        let grandchild = menu_node_value(21, vec![("label", Value::Str(Str::from("Deep")))], vec![]);
        let child = menu_node_value(11, vec![("label", Value::Str(Str::from("Submenu")))], vec![grandchild]);
        let root = menu_node_value(0, vec![], vec![child]);

        let item = parse_menu_node(&root, 0).expect("must parse a well-formed node");
        assert_eq!(item.children[0].children[0].id, 21);
        assert_eq!(item.children[0].children[0].label, Some("Deep".to_string()));
    }

    #[test]
    fn parse_menu_node_rejects_a_non_structure_value() {
        assert_eq!(parse_menu_node(&Value::I32(42), 0), None);
    }

    #[test]
    fn parse_menu_node_truncates_at_the_depth_cap_without_panicking_or_overflowing() {
        // A chain well deeper than MAX_MENU_DEPTH -- each level a trivial, legally-encoded node,
        // exactly the shape a malicious (or just buggy) GetLayout reply could hand this parser
        // (Correctness review: unbounded recursion through this tree is a stack-overflow DoS any
        // session-bus peer registering a tray item's Menu object could otherwise trigger).
        fn deep_chain(remaining: u32, id: i32) -> Value<'static> {
            if remaining == 0 {
                menu_node_value(id, vec![], vec![])
            } else {
                menu_node_value(id, vec![], vec![deep_chain(remaining - 1, id + 1)])
            }
        }

        let root = deep_chain(MAX_MENU_DEPTH + 20, 0);

        // Must complete (no panic, no stack overflow) and return a real, if truncated, tree.
        let item = parse_menu_node(&root, 0).expect("the root node itself must still parse");

        let mut current = &item;
        let mut depth = 0;
        while !current.children.is_empty() {
            current = &current.children[0];
            depth += 1;
        }
        assert_eq!(depth, MAX_MENU_DEPTH, "parsing must truncate children exactly at the depth cap, not keep recursing into the deeper levels the raw tree actually has");
    }

    // ---- RawMenuLayout / raw_menu_layout_to_value (the real GetLayout wire-shape seam) ----

    #[test]
    fn raw_menu_layout_signature_matches_the_real_dbusmenu_wire_shape() {
        // GetLayout's real reply signature (the DBusMenu spec's own "(ia{sv}av)") -- this is the
        // exact check `zvariant::DynamicDeserialize`'s blanket impl performs against the live
        // message at call time; a mismatch here means every real GetLayout call would fail with
        // a signature-mismatch error despite this module's own tests passing (see
        // RawMenuLayout's own doc comment).
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

    // ---- should_call_activate ----

    #[test]
    fn should_call_activate_is_true_when_item_is_not_a_menu() {
        assert!(should_call_activate(false));
    }

    #[test]
    fn should_call_activate_is_false_when_item_is_a_menu() {
        assert!(!should_call_activate(true));
    }

    // ---- arg parsers ----

    #[test]
    fn parse_activate_args_reads_id_x_y() {
        assert_eq!(parse_activate_args(&[serde_json::json!("1.42"), serde_json::json!(10), serde_json::json!(20)]), Some(("1.42".to_string(), 10, 20)));
    }

    #[test]
    fn parse_activate_args_rejects_a_malformed_shape() {
        assert_eq!(parse_activate_args(&[]), None, "missing every element");
        assert_eq!(parse_activate_args(&[serde_json::json!(1), serde_json::json!(10), serde_json::json!(20)]), None, "id is not a string");
        assert_eq!(parse_activate_args(&[serde_json::json!("1.42"), serde_json::json!("x"), serde_json::json!(20)]), None, "x is not a number");
    }

    #[test]
    fn parse_activate_menu_item_args_reads_id_and_menu_item_id() {
        assert_eq!(parse_activate_menu_item_args(&[serde_json::json!("1.42"), serde_json::json!(7)]), Some(("1.42".to_string(), 7)));
    }

    #[test]
    fn parse_activate_menu_item_args_rejects_a_malformed_shape() {
        assert_eq!(parse_activate_menu_item_args(&[]), None);
        assert_eq!(parse_activate_menu_item_args(&[serde_json::json!("1.42")]), None, "missing menu_item_id");
    }

    #[test]
    fn parse_menu_will_show_args_reads_id_and_submenu_id() {
        assert_eq!(parse_menu_will_show_args(&[serde_json::json!("1.42"), serde_json::json!(3)]), Some(("1.42".to_string(), 3)));
    }

    // ---- resolve_registration / register_status_notifier_item (Correctness review: unbounded
    //      ghost-registry/task-leak DoS via a fabricated unique name -- a connection can only ever
    //      truthfully claim its own real unique name, so a claimed `:N.M` must equal the real,
    //      bus-daemon-authenticated sender) ----

    use tokio::net::UnixStream;

    /// A connected pair of p2p zbus connections, no bus daemon involved -- based on
    /// `dbus::bluetooth`'s own test helper of the same name (see its doc comment for why both
    /// builders must be driven concurrently via `try_join!`), with one addition: the server side
    /// (`connection`, the first element -- what `register_item`'s own outgoing proxy calls use as
    /// `self.connection`) gets a short `method_timeout`. Unlike bluetooth's tests, which only ever
    /// call *into* the agent side, `register_item`'s real code path calls *out* from this side
    /// (property reads against the fabricated `StatusNotifierItem`, plus Fix 5's own
    /// `NameHasOwner` liveness check) to a peer that never registers any object server handler for
    /// them -- with zbus's default (multi-second) per-call timeout, each such call would hang
    /// until it lapses instead of erroring quickly, and `register_item` awaits several of them
    /// sequentially.
    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder =
            zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p().method_timeout(std::time::Duration::from_millis(200));
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    #[tokio::test]
    async fn resolve_registration_accepts_a_unique_name_matching_the_real_sender() {
        let (connection, _peer) = p2p_pair().await;
        match resolve_registration(&connection, ":1.5", Some(":1.5")).await {
            Ok((unique_name, object_path)) => {
                assert_eq!(unique_name.as_str(), ":1.5");
                assert_eq!(object_path.as_str(), DEFAULT_ITEM_OBJECT_PATH);
            }
            Err(err) => panic!("a claimed unique name equal to the real sender must be accepted: {err}"),
        }
    }

    #[tokio::test]
    async fn resolve_registration_rejects_a_fabricated_unique_name() {
        let (connection, _peer) = p2p_pair().await;
        // The real sender is :1.5, but `service` claims a completely different, never-connected
        // unique name -- exactly the `RegisterStatusNotifierItem(":999.1")` attack the Correctness
        // review describes.
        match resolve_registration(&connection, ":999.1", Some(":1.5")).await {
            Err(RegistrationError::UniqueNameMismatch { claimed, sender }) => {
                assert_eq!(claimed, ":999.1");
                assert_eq!(sender, ":1.5");
            }
            other => panic!("a fabricated unique name not matching the real sender must be rejected as UniqueNameMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_registration_rejects_a_unique_name_with_no_sender_present() {
        let (connection, _peer) = p2p_pair().await;
        assert!(matches!(resolve_registration(&connection, ":1.5", None).await, Err(RegistrationError::NoSender)));
    }

    /// Builds a real `RegisterStatusNotifierItem` method-call `Message` carrying `sender` in its
    /// own `SENDER` header field -- constructed locally (no live dispatch needed), so the
    /// `Header<'_>` this yields via `.header()` is exactly what `header.sender()` would return for
    /// a real call from that unique name, letting these tests exercise the real interface method
    /// (not just `resolve_registration` in isolation) with a controlled, authenticated sender.
    fn register_call_message(sender: &str) -> zbus::Message {
        zbus::Message::method_call(WATCHER_OBJECT_PATH, "RegisterStatusNotifierItem")
            .expect("valid method-call builder")
            .sender(sender)
            .expect("valid sender unique name")
            .build(&())
            .expect("well-formed method-call message")
    }

    fn test_watcher(connection: zbus::Connection, registry: ItemRegistry) -> StatusNotifierWatcher {
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        StatusNotifierWatcher { connection, registry, host_registered: Arc::new(Mutex::new(false)), events }
    }

    /// Minimal server-side stub answering only the `org.kde.StatusNotifierItem` properties
    /// `fetch_tray_item_base`/`register_item` actually read for a real registration to complete --
    /// a bare p2p connection with nothing exported on the peer's object server never replies to
    /// these at all (no bus daemon to synthesize an `UnknownObject` error on its behalf), so
    /// `register_item`'s real outbound calls would otherwise hang forever instead of erroring or
    /// returning quickly (confirmed live: without this, the "accepts" test below hung until
    /// SIGKILL'd). Mirrors `dbus::bluetooth`'s own p2p test pattern of exporting the real handler
    /// before any call can reach it. `Menu` returns `"/"` so `register_item`'s own
    /// `path.as_str() != "/"` check skips the whole DBusMenu/GetLayout path, keeping this stub to
    /// exactly the properties needed and nothing more.
    struct StubStatusNotifierItem;

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl StubStatusNotifierItem {
        #[zbus(property, name = "Id")]
        fn id(&self) -> String {
            "stub-item".to_string()
        }
        #[zbus(property, name = "Title")]
        fn title(&self) -> String {
            "Stub Item".to_string()
        }
        #[zbus(property, name = "IconName")]
        fn icon_name(&self) -> String {
            String::new()
        }
        #[zbus(property, name = "IconPixmap")]
        fn icon_pixmap(&self) -> Vec<RawIconPixmap> {
            Vec::new()
        }
        #[zbus(property, name = "Status")]
        fn status(&self) -> String {
            "Active".to_string()
        }
        #[zbus(property, name = "ItemIsMenu")]
        fn item_is_menu(&self) -> bool {
            false
        }
        #[zbus(property, name = "ToolTip")]
        fn tool_tip(&self) -> RawToolTip {
            (String::new(), Vec::new(), String::new(), String::new())
        }
        #[zbus(property, name = "Menu")]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/").expect("\"/\" is a valid object path")
        }
    }

    /// Minimal server-side stub answering `org.freedesktop.DBus.NameHasOwner` -- Fix 5's own
    /// pre-insert liveness check calls this against `self.connection`; on a bare p2p connection
    /// nothing else would ever answer it either (same "no bus daemon to fall back on" reasoning as
    /// [`StubStatusNotifierItem`]'s own doc comment).
    struct StubDBusDaemon;

    #[zbus::interface(name = "org.freedesktop.DBus")]
    impl StubDBusDaemon {
        #[zbus(name = "NameHasOwner")]
        fn name_has_owner(&self, _name: String) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn register_status_notifier_item_accepts_a_unique_name_matching_the_real_sender() {
        let (connection, peer) = p2p_pair().await;
        peer.object_server().at(DEFAULT_ITEM_OBJECT_PATH, StubStatusNotifierItem).await.expect("failed to export the stub StatusNotifierItem");
        peer.object_server().at("/org/freedesktop/DBus", StubDBusDaemon).await.expect("failed to export the stub org.freedesktop.DBus");

        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let watcher = test_watcher(connection.clone(), registry.clone());
        let emitter = zbus::object_server::SignalEmitter::new(&connection, WATCHER_OBJECT_PATH).expect("valid signal emitter");

        let message = register_call_message(":1.5");
        let result = watcher.register_status_notifier_item(":1.5".to_string(), message.header(), emitter).await;

        assert!(result.is_ok(), "a claimed unique name equal to the real sender must be accepted: {result:?}");
        assert_eq!(registry.lock().unwrap().len(), 1, "a matching registration must create exactly one registry entry");
    }

    #[tokio::test]
    async fn register_status_notifier_item_rejects_a_fabricated_unique_name_and_creates_no_registry_entry() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let watcher = test_watcher(connection.clone(), registry.clone());
        let emitter = zbus::object_server::SignalEmitter::new(&connection, WATCHER_OBJECT_PATH).expect("valid signal emitter");

        // The real sender is :1.5; the call claims to be the fabricated, never-connected :999.1.
        let message = register_call_message(":1.5");
        let result = watcher.register_status_notifier_item(":999.1".to_string(), message.header(), emitter).await;

        assert!(result.is_err(), "a fabricated unique name not equal to the real sender must be rejected");
        assert!(registry.lock().unwrap().is_empty(), "a rejected registration must not create a registry entry");
    }
}
