//! Path-trust validator and icon-data handling: validates `<img src>`/`image-path`/action-icon
//! paths against a trusted-directory allowlist, decodes/bounds-checks/spools raw image-data
//! hints, and deletes spooled icon files safely. Split from `dbus::notifications` -- see
//! `dbus/notifications/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};

use zbus::zvariant::Value;

use crate::dbus::shm_icons::{self, PngEncodeError};

use super::markup::parse_markup;
use super::{MAX_BODY_BYTES, MAX_IMAGE_DIMENSION, NotificationSpan, truncate_utf8_bytes};

// -------------------------------------------------------------------------------------------
// Path-trust validator (TDD seam 3): shared by `<img src>`, `image-path`, action-icon names, and
// registered sound files.
// -------------------------------------------------------------------------------------------

/// The trusted icon directories ADR-0033 names, resolved against the real `$HOME`. Production
/// callers use this; tests inject their own roots (real `tempfile` fixtures) directly into
/// [`validate_trusted_path`] instead, so this function itself needs no test coverage of its own.
pub(super) fn default_trusted_icon_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/usr/share/icons"), PathBuf::from("/usr/share/pixmaps")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }
    roots
}

/// Accepts `path` only as an absolute path that really exists as a regular file under one of
/// `trusted_roots` (ADR-0033). `canonicalize()` resolves both `..` traversal and symlinks before
/// the trusted-root check runs, so a symlink planted inside a trusted directory that points
/// outside it is rejected just like an out-of-tree path would be -- the check compares fully
/// resolved paths on both sides (`canonical.starts_with(canonicalized_root)`), not raw strings.
/// A relative path or a bare theme name (no `/`) is rejected outright by the `is_absolute` check,
/// degrading to "no icon" rather than attempting any theme-name resolution (`system:find_icon` is
/// separate, unbuilt IDL row -- ADR-0033).
pub(super) fn validate_trusted_path(path: &str, trusted_roots: &[PathBuf]) -> Option<PathBuf> {
    let candidate = Path::new(path);
    if !candidate.is_absolute() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.is_file() {
        return None;
    }
    let is_trusted =
        trusted_roots.iter().any(|root| root.canonicalize().map(|root| canonical.starts_with(&root)).unwrap_or(false));
    is_trusted.then_some(canonical)
}

/// Strips a `file://` URI scheme prefix, if present, leaving a raw filesystem path either way
/// (§1's `image-path`/`app_icon` hints can arrive as either form).
pub(super) fn strip_file_uri(path: &str) -> &str {
    path.strip_prefix("file://").unwrap_or(path)
}

/// Runs `Notify`'s raw `body` through the full sanitize pipeline: byte-cap truncation
/// ([`truncate_utf8_bytes`]), the allowlist grammar ([`parse_markup`]), then the path-trust
/// validator against every `<img src>` -- an image whose path isn't a real, trusted file is
/// dropped from the body entirely (ADR-0033: "closes off arbitrary local-file disclosure through
/// body markup").
pub(super) fn sanitize_body(raw_body: &str, trusted_roots: &[PathBuf]) -> Vec<NotificationSpan> {
    let truncated = truncate_utf8_bytes(raw_body, MAX_BODY_BYTES);
    parse_markup(&truncated)
        .into_iter()
        .filter_map(|span| match span {
            NotificationSpan::Image { image_path } => validate_trusted_path(&image_path, trusted_roots)
                .map(|validated| NotificationSpan::Image { image_path: validated.to_string_lossy().into_owned() }),
            text_span => Some(text_span),
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// Image hint decoding: the real freedesktop `image-data`/`icon_data` struct shape
// `(iiibiiay)` -- width, height, rowstride, has_alpha, bits_per_sample, channels, data -- decoded
// by hand from the already-unwrapped `zvariant::Value`, same technique `dbus::tray::parse_menu_node`
// already uses for a `Value::Structure`. Not tray's `IconPixmap` shape (square-only ARGB32); this
// is a different, real struct with its own field layout.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RawImageData {
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
    bits_per_sample: i32,
    channels: i32,
    data: Vec<u8>,
}

fn value_as_i32(value: &Value<'_>) -> Option<i32> {
    match value {
        Value::I32(i) => Some(*i),
        _ => None,
    }
}

pub(super) fn value_as_bool(value: &Value<'_>) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

pub(super) fn value_as_u8(value: &Value<'_>) -> Option<u8> {
    match value {
        Value::U8(b) => Some(*b),
        _ => None,
    }
}

pub(super) fn value_as_str<'a>(value: &'a Value<'_>) -> Option<&'a str> {
    match value {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn value_as_bytes(value: &Value<'_>) -> Option<Vec<u8>> {
    match value {
        Value::Array(array) => {
            Some(array.iter().filter_map(|v| if let Value::U8(b) = v { Some(*b) } else { None }).collect())
        }
        _ => None,
    }
}

/// Decodes an `image-data`/`icon_data` hint's `(iiibiiay)` structure. `None` for anything that
/// isn't a 7-field structure with exactly this field-type layout -- a malformed hint degrades to
/// "no image from this source", never a panic.
pub(super) fn decode_raw_image_data(value: &Value<'_>) -> Option<RawImageData> {
    let Value::Structure(structure) = value else { return None };
    let fields = structure.fields();
    if fields.len() != 7 {
        return None;
    }
    Some(RawImageData {
        width: value_as_i32(&fields[0])?,
        height: value_as_i32(&fields[1])?,
        rowstride: value_as_i32(&fields[2])?,
        has_alpha: value_as_bool(&fields[3])?,
        bits_per_sample: value_as_i32(&fields[4])?,
        channels: value_as_i32(&fields[5])?,
        data: value_as_bytes(&fields[6])?,
    })
}

/// Bounds-checks a decoded `image-data`/`icon_data` hint (docs/oblisk-supervisor-services-dbus.md
/// §1.1's "ARGB icon rejection" extended to the real struct shape): positive, capped at
/// [`MAX_IMAGE_DIMENSION`], 8-bit samples only (ponytail: no 16-bit/float sample support --
/// nothing checked shows a real sender using anything else), a channel count matching `has_alpha`
/// exactly (3 = RGB, 4 = RGBA), no row padding (`rowstride == width * channels` exactly), and the
/// data buffer's real length matching `rowstride * height` exactly.
pub(super) fn image_data_is_valid(image: &RawImageData) -> bool {
    image.width > 0
        && image.height > 0
        && image.width <= MAX_IMAGE_DIMENSION
        && image.height <= MAX_IMAGE_DIMENSION
        && image.bits_per_sample == 8
        && image.channels == if image.has_alpha { 4 } else { 3 }
        && image.rowstride == image.width * image.channels
        && image.data.len() == (image.rowstride as usize) * (image.height as usize)
}

/// Encodes an already-bounds-checked [`RawImageData`] to PNG. Unlike `dbus::tray`'s
/// `encode_argb32_to_png`, no channel reordering is needed -- the freedesktop `image-data` hint is
/// already RGB(A) row-major, not ARGB network byte order.
pub(super) fn encode_image_data_to_png(image: &RawImageData) -> Result<Vec<u8>, PngEncodeError> {
    let mut buffer = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buffer, image.width as u32, image.height as u32);
        encoder.set_color(if image.has_alpha { png::ColorType::Rgba } else { png::ColorType::Rgb });
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(PngEncodeError::Png)?;
        writer.write_image_data(&image.data).map_err(PngEncodeError::Png)?;
    }
    Ok(buffer)
}

/// `/dev/shm/oblisk-$UID/notifications` (ADR-0033: "gets the `$UID` fix ADR-0031 already
/// established for tray"). Our own icon spool root -- the only directory [`delete_icon_file`] is
/// ever allowed to delete from (finding 1).
fn notifications_icon_dir() -> PathBuf {
    shm_icons::icon_dir("notifications")
}

pub(super) fn write_icon_png(id: u32, png_bytes: &[u8]) -> std::io::Result<String> {
    shm_icons::write_png("notifications", &format!("notif-{id}.png"), png_bytes)
}

/// Whether `path` is safe for [`delete_icon_file`] to actually delete: it must canonicalize to a
/// real file living under `spool_root` (also canonicalized) -- the same canonicalize-both-sides
/// care [`validate_trusted_path`] already takes. `false` for anything that fails to canonicalize
/// (already gone, or never existed) or resolves outside `spool_root`.
///
/// Finding 1: `Notification.icon_path` is set identically whether it's our own SHM spool copy or a
/// client-supplied `image-path`/`app_icon` hint that [`validate_trusted_path`] resolved to a real,
/// externally-owned file under a trusted theme directory (`/usr/share/icons`, `~/.local/share/icons`,
/// etc.) -- deleting on dismiss/expiry/eviction must never touch the latter.
fn path_is_within_spool_root(path: &str, spool_root: &Path) -> bool {
    let Ok(spool_root) = spool_root.canonicalize() else { return false };
    match Path::new(path).canonicalize() {
        Ok(canonical) => canonical.starts_with(&spool_root),
        Err(_) => false,
    }
}

/// Deletes `path` only if it lives under our own SHM spool root ([`notifications_icon_dir`]) --
/// never a client-supplied icon hint resolved to a real, externally-owned file (finding 1). A path
/// outside our spool root is a silent no-op: we simply forget the reference, we never delete or
/// even touch a file we don't own.
pub(super) fn delete_icon_file(path: &str) {
    if !path_is_within_spool_root(path, &notifications_icon_dir()) {
        return;
    }
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!("notifications: failed to delete spooled icon {path:?}: {err}");
    }
}

/// `Notify`'s image hint precedence (docs/oblisk-supervisor-services-dbus.md §1;
/// ADR-0033/base-spec convention): `image-data`/`image_data` > `image-path`/`image_path` >
/// deprecated positional `app_icon` > deprecated `icon_data`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum IconInput {
    ImageData(RawImageData),
    ImagePath(String),
    AppIcon(String),
    IconData(RawImageData),
    None,
}

pub(super) fn resolve_icon_input(
    image_data: Option<RawImageData>,
    image_path: Option<String>,
    app_icon: Option<String>,
    icon_data: Option<RawImageData>,
) -> IconInput {
    if let Some(data) = image_data {
        return IconInput::ImageData(data);
    }
    if let Some(path) = image_path.filter(|p| !p.is_empty()) {
        return IconInput::ImagePath(path);
    }
    if let Some(icon) = app_icon.filter(|a| !a.is_empty()) {
        return IconInput::AppIcon(icon);
    }
    if let Some(data) = icon_data {
        return IconInput::IconData(data);
    }
    IconInput::None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- validate_trusted_path (TDD seam 3) ----

    #[test]
    fn validate_trusted_path_accepts_a_real_file_under_a_trusted_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("icon.png");
        std::fs::write(&file, b"fake png bytes").unwrap();

        let result = validate_trusted_path(file.to_str().unwrap(), &[dir.path().to_path_buf()]);
        assert_eq!(result, Some(file.canonicalize().unwrap()));
    }

    #[test]
    fn validate_trusted_path_rejects_a_relative_path() {
        assert_eq!(validate_trusted_path("relative/icon.png", &[PathBuf::from("/tmp")]), None);
    }

    #[test]
    fn validate_trusted_path_rejects_a_bare_theme_name() {
        assert_eq!(validate_trusted_path("battery-full", &[PathBuf::from("/tmp")]), None);
    }

    #[test]
    fn validate_trusted_path_rejects_a_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.png");
        assert_eq!(validate_trusted_path(missing.to_str().unwrap(), &[dir.path().to_path_buf()]), None);
    }

    #[test]
    fn validate_trusted_path_rejects_a_path_outside_every_trusted_root() {
        let trusted_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("icon.png");
        std::fs::write(&outside_file, b"x").unwrap();

        assert_eq!(validate_trusted_path(outside_file.to_str().unwrap(), &[trusted_dir.path().to_path_buf()]), None);
    }

    #[test]
    fn validate_trusted_path_rejects_a_directory_traversal_escape() {
        let trusted_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("secret.png");
        std::fs::write(&outside_file, b"x").unwrap();

        // "<trusted_dir>/../<outside_dir's own name>/secret.png" only actually escapes if the two
        // temp dirs share a parent -- construct the traversal against the trusted dir's own parent
        // directly instead of assuming that, so this test is robust to however the OS lays out
        // temp directories.
        let escape_path =
            trusted_dir.path().join("..").join(outside_dir.path().file_name().unwrap()).join("secret.png");
        assert_eq!(validate_trusted_path(escape_path.to_str().unwrap(), &[trusted_dir.path().to_path_buf()]), None);
    }

    #[test]
    fn validate_trusted_path_rejects_a_symlink_escaping_the_trusted_root() {
        let trusted_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("real.png");
        std::fs::write(&outside_file, b"x").unwrap();

        let symlink_path = trusted_dir.path().join("escape.png");
        std::os::unix::fs::symlink(&outside_file, &symlink_path).unwrap();

        assert_eq!(validate_trusted_path(symlink_path.to_str().unwrap(), &[trusted_dir.path().to_path_buf()]), None);
    }

    #[test]
    fn validate_trusted_path_rejects_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(validate_trusted_path(dir.path().to_str().unwrap(), &[dir.path().to_path_buf()]), None);
    }

    #[test]
    fn strip_file_uri_strips_the_scheme_when_present() {
        assert_eq!(strip_file_uri("file:///usr/share/icons/x.png"), "/usr/share/icons/x.png");
    }

    #[test]
    fn strip_file_uri_is_a_no_op_without_the_scheme() {
        assert_eq!(strip_file_uri("/usr/share/icons/x.png"), "/usr/share/icons/x.png");
    }

    // ---- path_is_within_spool_root (finding 1: never delete a file we don't own) ----

    #[test]
    fn path_is_within_spool_root_accepts_a_real_file_under_the_spool_root() {
        let spool_root = tempfile::tempdir().unwrap();
        let file = spool_root.path().join("notif-1.png");
        std::fs::write(&file, b"fake png bytes").unwrap();

        assert!(path_is_within_spool_root(file.to_str().unwrap(), spool_root.path()));
    }

    #[test]
    fn path_is_within_spool_root_rejects_a_file_outside_the_spool_root() {
        // A real, existing file living somewhere else entirely -- exactly the shape of a
        // client-supplied image-path/app_icon hint resolved to a real theme icon under
        // /usr/share/icons or ~/.local/share/icons, which delete_icon_file must never touch.
        let spool_root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let external_file = elsewhere.path().join("theme-icon.png");
        std::fs::write(&external_file, b"a real, externally-owned icon").unwrap();

        assert!(!path_is_within_spool_root(external_file.to_str().unwrap(), spool_root.path()));
    }

    #[test]
    fn path_is_within_spool_root_rejects_a_nonexistent_path() {
        let spool_root = tempfile::tempdir().unwrap();
        let missing = spool_root.path().join("never-written.png");
        assert!(!path_is_within_spool_root(missing.to_str().unwrap(), spool_root.path()));
    }

    #[test]
    fn path_is_within_spool_root_rejects_a_symlink_escaping_the_spool_root() {
        let spool_root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let external_file = elsewhere.path().join("real.png");
        std::fs::write(&external_file, b"x").unwrap();

        let symlink_path = spool_root.path().join("escape.png");
        std::os::unix::fs::symlink(&external_file, &symlink_path).unwrap();

        assert!(!path_is_within_spool_root(symlink_path.to_str().unwrap(), spool_root.path()));
    }

    // ---- image-data decoding + bounds checks ----

    fn valid_rgba_image(width: i32, height: i32) -> RawImageData {
        let channels = 4;
        let rowstride = width * channels;
        RawImageData {
            width,
            height,
            rowstride,
            has_alpha: true,
            bits_per_sample: 8,
            channels,
            data: vec![0u8; (rowstride * height) as usize],
        }
    }

    #[test]
    fn image_data_is_valid_accepts_a_well_formed_rgba_image() {
        assert!(image_data_is_valid(&valid_rgba_image(4, 4)));
    }

    #[test]
    fn image_data_is_valid_rejects_oversized_dimensions() {
        assert!(!image_data_is_valid(&valid_rgba_image(MAX_IMAGE_DIMENSION + 1, 4)));
        assert!(image_data_is_valid(&valid_rgba_image(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION)));
    }

    #[test]
    fn image_data_is_valid_rejects_a_channel_count_mismatched_with_has_alpha() {
        let mut image = valid_rgba_image(4, 4);
        image.has_alpha = false;
        assert!(!image_data_is_valid(&image), "has_alpha=false but channels=4 must be rejected");
    }

    #[test]
    fn image_data_is_valid_rejects_a_rowstride_mismatch() {
        let mut image = valid_rgba_image(4, 4);
        image.rowstride += 4;
        assert!(!image_data_is_valid(&image));
    }

    #[test]
    fn image_data_is_valid_rejects_a_data_length_mismatch() {
        let mut image = valid_rgba_image(4, 4);
        image.data.pop();
        assert!(!image_data_is_valid(&image));
    }

    #[test]
    fn image_data_is_valid_rejects_non_positive_dimensions() {
        assert!(!image_data_is_valid(&valid_rgba_image(0, 4)));
        assert!(!image_data_is_valid(&valid_rgba_image(4, 0)));
    }

    #[test]
    fn image_data_is_valid_rejects_a_non_8_bit_sample() {
        let mut image = valid_rgba_image(4, 4);
        image.bits_per_sample = 16;
        assert!(!image_data_is_valid(&image));
    }

    #[test]
    fn encode_image_data_to_png_round_trips_a_known_pixel() {
        // One 1x1 RGBA pixel: R=0x11, G=0x22, B=0x33, A=0x44 (already RGBA row-major, unlike
        // tray's ARGB network-byte-order pixmaps -- no reordering needed).
        let image = RawImageData {
            width: 1,
            height: 1,
            rowstride: 4,
            has_alpha: true,
            bits_per_sample: 8,
            channels: 4,
            data: vec![0x11, 0x22, 0x33, 0x44],
        };
        let png_bytes = encode_image_data_to_png(&image).expect("encoding must succeed");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("valid PNG frame");
        assert_eq!(&buf[..info.buffer_size()], &[0x11, 0x22, 0x33, 0x44]);
    }

    fn structure_value(fields: Vec<Value<'static>>) -> Value<'static> {
        let mut builder = zbus::zvariant::StructureBuilder::new();
        for field in fields {
            builder = builder.append_field(field);
        }
        Value::Structure(builder.build().expect("well-formed test structure"))
    }

    #[test]
    fn decode_raw_image_data_parses_a_well_formed_structure() {
        let data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut array = zbus::zvariant::Array::new(&zbus::zvariant::Signature::U8);
        for byte in &data {
            array.append(Value::U8(*byte)).unwrap();
        }
        let value = structure_value(vec![
            Value::I32(2),
            Value::I32(1),
            Value::I32(8),
            Value::Bool(true),
            Value::I32(8),
            Value::I32(4),
            Value::Array(array),
        ]);

        let decoded = decode_raw_image_data(&value).expect("must decode a well-formed image-data structure");
        assert_eq!(
            decoded,
            RawImageData { width: 2, height: 1, rowstride: 8, has_alpha: true, bits_per_sample: 8, channels: 4, data }
        );
    }

    #[test]
    fn decode_raw_image_data_rejects_a_non_structure_value() {
        assert_eq!(decode_raw_image_data(&Value::I32(1)), None);
    }

    #[test]
    fn decode_raw_image_data_rejects_a_wrong_field_count() {
        let value = structure_value(vec![Value::I32(1), Value::I32(1)]);
        assert_eq!(decode_raw_image_data(&value), None);
    }

    // ---- resolve_icon_input ----

    fn tiny_image() -> RawImageData {
        valid_rgba_image(1, 1)
    }

    #[test]
    fn resolve_icon_input_prefers_image_data_over_everything() {
        let resolved = resolve_icon_input(
            Some(tiny_image()),
            Some("/path".to_string()),
            Some("app-icon".to_string()),
            Some(tiny_image()),
        );
        assert_eq!(resolved, IconInput::ImageData(tiny_image()));
    }

    #[test]
    fn resolve_icon_input_prefers_image_path_over_app_icon_and_icon_data() {
        let resolved =
            resolve_icon_input(None, Some("/path".to_string()), Some("app-icon".to_string()), Some(tiny_image()));
        assert_eq!(resolved, IconInput::ImagePath("/path".to_string()));
    }

    #[test]
    fn resolve_icon_input_prefers_app_icon_over_icon_data() {
        let resolved = resolve_icon_input(None, None, Some("app-icon".to_string()), Some(tiny_image()));
        assert_eq!(resolved, IconInput::AppIcon("app-icon".to_string()));
    }

    #[test]
    fn resolve_icon_input_falls_back_to_icon_data() {
        let resolved = resolve_icon_input(None, None, None, Some(tiny_image()));
        assert_eq!(resolved, IconInput::IconData(tiny_image()));
    }

    #[test]
    fn resolve_icon_input_is_none_when_nothing_is_supplied() {
        assert_eq!(resolve_icon_input(None, None, None, None), IconInput::None);
    }

    #[test]
    fn resolve_icon_input_treats_an_empty_image_path_as_absent() {
        let resolved = resolve_icon_input(None, Some(String::new()), Some("app-icon".to_string()), None);
        assert_eq!(resolved, IconInput::AppIcon("app-icon".to_string()));
    }
}
