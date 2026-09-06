//! Path-trust validator for `<img src>`, `image-path`, action-icon paths, and registered sound
//! files; decodes and spools image-data hints, and safely deletes spooled icons. Split from
//! `dbus::notifications`; see `dbus/notifications/mod.rs`.

use std::path::{Path, PathBuf};

use zbus::zvariant::Value;

use crate::capabilities::shm_icons::{self, PngEncodeError};

use super::markup::parse_markup;
use super::{MAX_APP_ICON_NAME_BYTES, MAX_BODY_BYTES, MAX_IMAGE_DIMENSION, NotificationSpan, truncate_utf8_bytes};

/// ADR-0033's trusted icon roots, with `$HOME` resolved at runtime. Tests inject fixture roots.
pub(super) fn default_trusted_icon_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/usr/share/icons"), PathBuf::from("/usr/share/pixmaps")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }
    roots
}

/// Accepts only an existing regular file under a trusted absolute root (ADR-0033). Canonicalizing
/// both sides rejects `..` traversal and symlinks escaping the root. Relative paths and bare theme
/// names degrade to no icon; theme lookup is the unbuilt `system:find_icon` row.
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

/// Strips `file://` from §1 `image-path`/`app_icon` hints.
pub(super) fn strip_file_uri(path: &str) -> &str {
    path.strip_prefix("file://").unwrap_or(path)
}

/// Truncates and parses `Notify` body markup, then validates every `<img src>` path. Untrusted
/// images are dropped (ADR-0033 closes arbitrary local-file disclosure through body markup).
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

// Freedesktop `image-data`/`icon_data` is `(iiibiiay)`, unlike tray's square-only ARGB32
// `IconPixmap`; decode the unwrapped `zvariant::Value::Structure` by hand, using the same
// technique `dbus::tray::parse_menu_node` uses for a `Value::Structure`.

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

/// Decodes an `image-data`/`icon_data` `(iiibiiay)` structure. Malformed or non-7-field values
/// yield no image, not a panic.
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

/// Validates `docs/oblisk-supervisor-services-dbus.md §1.1`'s image-data bounds and its "ARGB
/// icon rejection": positive dimensions up to [`MAX_IMAGE_DIMENSION`], 8-bit
/// samples only, channels matching alpha (3=RGB, 4=RGBA), no row padding, and exact data length.
/// ponytail: no 16-bit/float support until a real sender needs it.
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

/// Encodes checked [`RawImageData`] to PNG. Unlike `dbus::tray`'s `encode_argb32_to_png`, no
/// channel reorder is needed: freedesktop data is RGB(A) row-major, not ARGB network-byte-order
/// pixmaps.
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

/// Our icon spool root, `$XDG_RUNTIME_DIR/oblisk/notifications` (ADR-0033's `$UID` fix from
/// ADR-0031); [`delete_icon_file`] may delete only here (finding 1).
fn notifications_icon_dir() -> PathBuf {
    shm_icons::icon_dir("notifications")
}

pub(super) fn write_icon_png(id: u32, png_bytes: &[u8]) -> std::io::Result<String> {
    shm_icons::write_png("notifications", &format!("notif-{id}.png"), png_bytes)
}

/// Whether `path` canonicalizes to a real file under canonicalized `spool_root`.
///
/// Finding 1: `Notification.image_path` may be our SHM copy or an externally-owned
/// `image-path`/`app_icon` (for example under `/usr/share/icons`); deletion must distinguish them.
fn path_is_within_spool_root(path: &str, spool_root: &Path) -> bool {
    let Ok(spool_root) = spool_root.canonicalize() else { return false };
    match Path::new(path).canonicalize() {
        Ok(canonical) => canonical.starts_with(&spool_root),
        Err(_) => false,
    }
}

/// Deletes only under our SHM spool root, never a client-supplied external icon (finding 1).
/// Outside it, forget the reference without touching the file.
pub(super) fn delete_icon_file(path: &str) {
    if !path_is_within_spool_root(path, &notifications_icon_dir()) {
        return;
    }
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!("notifications: failed to delete spooled icon {path:?}: {err}");
    }
}

/// Attached-picture precedence from §1: `image-data`/`image_data` > `image-path`/`image_path` >
/// `icon_data`.
///
/// The three spellings accumulated across spec versions. `app_icon` no longer competes with this
/// picture chain (ADR-0091); [`resolve_app_icon`] owns the application's icon.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ImageInput {
    ImageData(RawImageData),
    ImagePath(String),
    IconData(RawImageData),
    None,
}

pub(super) fn resolve_image_input(
    image_data: Option<RawImageData>,
    image_path: Option<String>,
    icon_data: Option<RawImageData>,
) -> ImageInput {
    if let Some(data) = image_data {
        return ImageInput::ImageData(data);
    }
    if let Some(path) = image_path.filter(|p| !p.is_empty()) {
        return ImageInput::ImagePath(path);
    }
    if let Some(data) = icon_data {
        return ImageInput::IconData(data);
    }
    ImageInput::None
}

/// Splits `image-path`/`image_path` into `(picture, theme name)` (ADR-0096).
///
/// §1.2 allows a `file://` URI or a freedesktop theme name; `file://` is the only URI schema
/// supported right now. Paths use [`validate_trusted_path`];
/// names have no path to validate. Leaving names in the picture chain caused ADR-0091's
/// `app_icon` bug, so they move to the application-icon path.
///
/// Split on `/`: using `is_absolute` would misclassify `"../../etc/passwd"` as a theme name and
/// pass it to renderer lookup.
pub(super) fn split_image_path_hint(hint: Option<String>) -> (Option<String>, Option<String>) {
    match hint.filter(|hint| !hint.is_empty()) {
        Some(hint) if !strip_file_uri(&hint).contains('/') => (None, Some(hint)),
        hint => (hint, None),
    }
}

/// Resolves `Notify`'s positional `app_icon` into something a config can draw (ADR-0091).
///
/// Theme names (`"firefox"`, `"org.telegram.desktop"`) pass through for renderer resolution
/// (`icon { name = ... }`, ADR-0054 decision 2); absolute paths and `file://` URIs use the trusted
/// root check.
///
/// A separator check, not `is_absolute`, keeps `../../etc/passwd` from becoming a theme name.
///
/// Before ADR-0091, validating the whole value rejected bare names, so nearly every notification
/// drew the same generic fallback.
pub(super) fn resolve_app_icon(app_icon: Option<String>, trusted_roots: &[PathBuf]) -> Option<String> {
    let app_icon = app_icon.filter(|icon| !icon.is_empty())?;
    let stripped = strip_file_uri(&app_icon);
    if !stripped.contains('/') {
        return Some(truncate_utf8_bytes(stripped, MAX_APP_ICON_NAME_BYTES));
    }
    validate_trusted_path(stripped, trusted_roots).map(|path| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Build traversal through the trusted dir's parent instead of assuming temp-dir layout.
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

    #[test]
    fn path_is_within_spool_root_accepts_a_real_file_under_the_spool_root() {
        let spool_root = tempfile::tempdir().unwrap();
        let file = spool_root.path().join("notif-1.png");
        std::fs::write(&file, b"fake png bytes").unwrap();

        assert!(path_is_within_spool_root(file.to_str().unwrap(), spool_root.path()));
    }

    #[test]
    fn path_is_within_spool_root_rejects_a_file_outside_the_spool_root() {
        // An external resolved theme icon is exactly what delete_icon_file must never touch.
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
        // One RGBA row-major pixel, unlike tray's ARGB network-byte-order pixmaps.
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

    fn tiny_image() -> RawImageData {
        valid_rgba_image(1, 1)
    }

    /// The `notify-send -i firefox` case, which is how the overwhelming majority of notifications
    /// on a desktop name their icon: the positional argument is empty and the theme name rides in
    /// the hint that otherwise means a picture.
    #[test]
    fn a_bare_name_in_the_image_path_hint_is_a_theme_name_not_a_picture() {
        assert_eq!(
            split_image_path_hint(Some("firefox".to_string())),
            (None, Some("firefox".to_string())),
            "goes to `app_icon`, not into the picture chain that would validate it away"
        );
    }

    #[test]
    fn a_path_in_the_image_path_hint_is_still_a_picture() {
        assert_eq!(split_image_path_hint(Some("/tmp/art.png".to_string())), (Some("/tmp/art.png".to_string()), None));
        assert_eq!(
            split_image_path_hint(Some("file:///tmp/art.png".to_string())),
            (Some("file:///tmp/art.png".to_string()), None),
            "the URI is stripped when the picture is resolved, not here"
        );
    }

    /// A relative path is neither, and must not be passed off as a theme name for the renderer's
    /// icon lookup to open -- `resolve_app_icon` refuses it on the same grounds.
    #[test]
    fn a_relative_path_in_the_image_path_hint_stays_in_the_picture_chain_to_be_refused() {
        assert_eq!(
            split_image_path_hint(Some("../../etc/passwd".to_string())),
            (Some("../../etc/passwd".to_string()), None)
        );
    }

    #[test]
    fn an_absent_or_empty_image_path_hint_is_neither() {
        assert_eq!(split_image_path_hint(None), (None, None));
        assert_eq!(split_image_path_hint(Some(String::new())), (None, None));
    }

    #[test]
    fn resolve_image_input_prefers_image_data_over_everything() {
        let resolved = resolve_image_input(Some(tiny_image()), Some("/path".to_string()), Some(tiny_image()));
        assert_eq!(resolved, ImageInput::ImageData(tiny_image()));
    }

    #[test]
    fn resolve_image_input_prefers_image_path_over_icon_data() {
        let resolved = resolve_image_input(None, Some("/path".to_string()), Some(tiny_image()));
        assert_eq!(resolved, ImageInput::ImagePath("/path".to_string()));
    }

    #[test]
    fn resolve_image_input_falls_back_to_icon_data() {
        assert_eq!(resolve_image_input(None, None, Some(tiny_image())), ImageInput::IconData(tiny_image()));
    }

    #[test]
    fn resolve_image_input_is_none_when_nothing_is_supplied() {
        assert_eq!(resolve_image_input(None, None, None), ImageInput::None);
    }

    #[test]
    fn resolve_image_input_treats_an_empty_image_path_as_absent() {
        assert_eq!(resolve_image_input(None, Some(String::new()), None), ImageInput::None);
    }

    /// The case that was silently broken: a bare theme name is what the base spec asks senders for
    /// and what nearly all of them send, and it used to resolve to nothing.
    #[test]
    fn resolve_app_icon_carries_a_theme_name_through_untouched() {
        assert_eq!(resolve_app_icon(Some("firefox".to_string()), &[]), Some("firefox".to_string()));
        assert_eq!(
            resolve_app_icon(Some("org.telegram.desktop".to_string()), &[]),
            Some("org.telegram.desktop".to_string())
        );
    }

    #[test]
    fn resolve_app_icon_is_none_for_absent_or_empty() {
        assert_eq!(resolve_app_icon(None, &[]), None);
        assert_eq!(resolve_app_icon(Some(String::new()), &[]), None);
    }

    /// A path still has to earn it, by the same rule every other client-supplied path follows.
    #[test]
    fn resolve_app_icon_validates_a_path_and_accepts_a_file_uri() {
        let root = tempfile::tempdir().expect("tempdir");
        let icon = root.path().join("app.png");
        std::fs::write(&icon, b"not really a png").expect("write");
        let roots = vec![root.path().to_path_buf()];

        let canonical = icon.canonicalize().expect("canonicalize").to_string_lossy().into_owned();
        assert_eq!(resolve_app_icon(Some(icon.to_string_lossy().into_owned()), &roots), Some(canonical.clone()));
        assert_eq!(resolve_app_icon(Some(format!("file://{}", icon.display())), &roots), Some(canonical));

        assert_eq!(resolve_app_icon(Some("/etc/passwd".to_string()), &roots), None, "outside every trusted root");
    }

    /// A relative path is neither a theme name nor a validatable path, and must not slip through
    /// as the former just because it is not absolute.
    #[test]
    fn resolve_app_icon_refuses_a_relative_path_rather_than_calling_it_a_name() {
        assert_eq!(resolve_app_icon(Some("../../etc/passwd".to_string()), &[]), None);
        assert_eq!(resolve_app_icon(Some("icons/app.png".to_string()), &[]), None);
    }
}
