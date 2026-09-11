//! Pure `Metadata` (`a{sv}`) parsing, album-art trust checks, and track identity comparison
//! (ADR-0036). Split from `dbus::mpris`; see `dbus/mpris/mod.rs`.

use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use zbus::zvariant::{OwnedValue, Value};

fn value_as_str<'a>(value: &'a Value<'_>) -> Option<&'a str> {
    match value {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// `xesam:artist` is `as` on every checked player; accept a bare string too for malformed metadata.
fn value_as_str_or_joined_array(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::Array(array) => {
            let parts: Vec<&str> = array.iter().filter_map(value_as_str).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

fn value_as_i64(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::I64(i) => Some(*i),
        Value::U32(u) => Some(i64::from(*u)),
        Value::I32(i) => Some(i64::from(*i)),
        _ => None,
    }
}

/// `mpris:trackid` is an object path (`o`) on checked players (Zen, mpv-mpris), but Quickshell's
/// `player.cpp:266-274` accepts a bare string for type-wrong players, so accept that too.
fn value_as_trackid(value: &Value<'_>) -> Option<String> {
    match value {
        Value::ObjectPath(path) => Some(path.as_str().to_string()),
        _ => value_as_str(value).map(str::to_string),
    }
}

/// One `Metadata` dict reduced to `obelisk.mpris`'s fields; album, disc/track number, genre, and
/// other keys outside the IDL player shape are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ParsedMetadata {
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) art_url: Option<String>,
    pub(super) length_us: Option<i64>,
    pub(super) trackid: Option<String>,
    pub(super) url: Option<String>,
}

pub(super) fn parse_metadata(metadata: &std::collections::HashMap<String, OwnedValue>) -> ParsedMetadata {
    let get = |key: &str| metadata.get(key).map(|v| Value::from(v.clone()));
    ParsedMetadata {
        title: get("xesam:title").as_ref().and_then(value_as_str).unwrap_or_default().to_string(),
        artist: get("xesam:artist").as_ref().and_then(value_as_str_or_joined_array).unwrap_or_default(),
        art_url: get("mpris:artUrl").as_ref().and_then(value_as_str).map(str::to_string),
        length_us: get("mpris:length").as_ref().and_then(value_as_i64),
        trackid: get("mpris:trackid").as_ref().and_then(value_as_trackid),
        url: get("xesam:url").as_ref().and_then(value_as_str).map(str::to_string),
    }
}

/// [`ParsedMetadata::track_identity`]'s composite key (ADR-0036, CONTEXT.md "Track identity"):
/// require `trackid`/`url`/`title` all to match. Real players leave any one unchanged across a
/// genuine track change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TrackIdentity {
    trackid: Option<String>,
    url: Option<String>,
    title: String,
}

impl ParsedMetadata {
    pub(super) fn track_identity(&self) -> TrackIdentity {
        TrackIdentity { trackid: self.trackid.clone(), url: self.url.clone(), title: self.title.clone() }
    }
}

/// `art_url` must be `file://` and canonicalize to an existing regular file (ADR-0036). No
/// directory allowlist: players use varied locations, confirmed by Zen's
/// `~/.config/zen/...` cache; the player already runs with the user's privileges. Other schemes,
/// remote `http(s)://`, and missing/dangling paths become empty; no HTTP-fetch dependency exists.
pub(super) fn resolve_album_art_path(art_url: Option<&str>) -> String {
    let Some(path) = art_url.and_then(file_url_to_path) else { return String::new() };
    let Ok(canonical) = path.canonicalize() else { return String::new() };
    if canonical.is_file() { canonical.to_string_lossy().into_owned() } else { String::new() }
}

/// A `file://` URL as a filesystem path: percent-decoded, with RFC 8089's optional `localhost`
/// authority dropped.
///
/// `strip_prefix("file://")` alone treated the URL as if it were already a path, so artwork named
/// `cover art.png` arrived as `cover%20art.png` and was never found, and `file://localhost/tmp/a`
/// became the relative path `localhost/tmp/a`. Decoding is byte-wise because a path is bytes on
/// Unix, not UTF-8: a filename the shell can open is not necessarily one `String` accepts.
fn file_url_to_path(art_url: &str) -> Option<PathBuf> {
    let rest = art_url.strip_prefix("file://")?;
    let encoded = rest.strip_prefix("localhost").filter(|tail| tail.starts_with('/')).unwrap_or(rest);
    let raw = encoded.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let escape = (raw[index] == b'%' && index + 2 < raw.len())
            .then(|| Some(hex_digit(raw[index + 1])? * 16 + hex_digit(raw[index + 2])?))
            .flatten();
        match escape {
            Some(byte) => {
                bytes.push(byte);
                index += 3;
            }
            None => {
                bytes.push(raw[index]);
                index += 1;
            }
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

/// Absolute target for `mpris:seek`/`seek_relative`, clamped to `[0, length]` before
/// `SetPosition`/`Seek` (ADR-0036). With cached `length_us == -1`, only the lower bound applies.
pub(super) fn clamp_seek_target(target_us: i64, length_us: i64) -> i64 {
    let lower = target_us.max(0);
    if length_us >= 0 { lower.min(length_us) } else { lower }
}

#[cfg(test)]
mod file_url_tests {
    use super::*;

    #[test]
    fn a_percent_escape_becomes_the_byte_it_encodes() {
        // A cover named "cover art.png" arrives as `cover%20art.png` and was looked up literally.
        assert_eq!(file_url_to_path("file:///tmp/cover%20art.png"), Some(PathBuf::from("/tmp/cover art.png")));
    }

    #[test]
    fn a_localhost_authority_is_not_part_of_the_path() {
        assert_eq!(file_url_to_path("file://localhost/tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
    }

    #[test]
    fn an_ordinary_path_is_unchanged() {
        let zen = "file:///home/anas/.config/zen/firefox-mpris/3909426_4.png";
        assert_eq!(file_url_to_path(zen), Some(PathBuf::from("/home/anas/.config/zen/firefox-mpris/3909426_4.png")));
    }

    #[test]
    fn a_stray_percent_is_kept_rather_than_swallowing_the_rest() {
        // `100%` in a filename is not an escape; dropping it would rename the file.
        assert_eq!(file_url_to_path("file:///tmp/100%.png"), Some(PathBuf::from("/tmp/100%.png")));
    }

    #[test]
    fn a_non_file_scheme_has_no_path() {
        assert_eq!(file_url_to_path("https://example.com/a.png"), None);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{Array, ObjectPath, Signature, Str};

    use super::*;

    fn owned(value: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn real_metadata() -> HashMap<String, OwnedValue> {
        let mut artists = Array::new(&Signature::Str);
        artists.append(Value::Str(Str::from("Ahmed Ebrahim"))).unwrap();
        let mut map = HashMap::new();
        map.insert("xesam:title".to_string(), owned(Value::Str(Str::from("Track Title"))));
        map.insert("xesam:artist".to_string(), owned(Value::Array(artists)));
        map.insert("mpris:artUrl".to_string(), owned(Value::Str(Str::from("file:///home/anas/art.png"))));
        map.insert("mpris:length".to_string(), owned(Value::I64(1_302_000_000)));
        map.insert(
            "mpris:trackid".to_string(),
            owned(Value::ObjectPath(ObjectPath::try_from("/org/mpris/MediaPlayer2/firefox").unwrap())),
        );
        map.insert("xesam:url".to_string(), owned(Value::Str(Str::from("https://example.com/watch"))));
        map
    }

    #[test]
    fn parse_metadata_reads_every_real_field_from_a_full_metadata_dict() {
        let parsed = parse_metadata(&real_metadata());
        assert_eq!(parsed.title, "Track Title");
        assert_eq!(parsed.artist, "Ahmed Ebrahim");
        assert_eq!(parsed.art_url.as_deref(), Some("file:///home/anas/art.png"));
        assert_eq!(parsed.length_us, Some(1_302_000_000));
        assert_eq!(parsed.trackid.as_deref(), Some("/org/mpris/MediaPlayer2/firefox"));
        assert_eq!(parsed.url.as_deref(), Some("https://example.com/watch"));
    }

    #[test]
    fn parse_metadata_joins_multiple_artists_with_a_comma() {
        let mut artists = Array::new(&Signature::Str);
        artists.append(Value::Str(Str::from("A"))).unwrap();
        artists.append(Value::Str(Str::from("B"))).unwrap();
        let mut map = HashMap::new();
        map.insert("xesam:artist".to_string(), owned(Value::Array(artists)));
        assert_eq!(parse_metadata(&map).artist, "A, B");
    }

    #[test]
    fn parse_metadata_degrades_gracefully_on_an_empty_dict() {
        let parsed = parse_metadata(&HashMap::new());
        assert_eq!(parsed, ParsedMetadata::default());
    }

    #[test]
    fn parse_metadata_accepts_a_bare_string_trackid_defensively() {
        // Real players sometimes publish the wrong D-Bus type.
        let mut map = HashMap::new();
        map.insert("mpris:trackid".to_string(), owned(Value::Str(Str::from("/0"))));
        assert_eq!(parse_metadata(&map).trackid.as_deref(), Some("/0"));
    }

    #[test]
    fn track_identity_differs_when_only_trackid_changes() {
        let a = ParsedMetadata {
            trackid: Some("/1".into()),
            url: Some("u".into()),
            title: "t".into(),
            ..Default::default()
        };
        let b = ParsedMetadata { trackid: Some("/2".into()), ..a.clone() };
        assert_ne!(a.track_identity(), b.track_identity());
    }

    #[test]
    fn track_identity_differs_when_only_url_changes() {
        let a = ParsedMetadata {
            trackid: Some("/1".into()),
            url: Some("u1".into()),
            title: "t".into(),
            ..Default::default()
        };
        let b = ParsedMetadata { url: Some("u2".into()), ..a.clone() };
        assert_ne!(a.track_identity(), b.track_identity());
    }

    #[test]
    fn track_identity_differs_when_only_title_changes() {
        let a = ParsedMetadata {
            trackid: Some("/1".into()),
            url: Some("u".into()),
            title: "t1".into(),
            ..Default::default()
        };
        let b = ParsedMetadata { title: "t2".into(), ..a.clone() };
        assert_ne!(a.track_identity(), b.track_identity());
    }

    #[test]
    fn track_identity_is_equal_when_nothing_changed() {
        let a = ParsedMetadata {
            trackid: Some("/1".into()),
            url: Some("u".into()),
            title: "t".into(),
            ..Default::default()
        };
        assert_eq!(a.track_identity(), a.clone().track_identity());
    }

    #[test]
    fn resolve_album_art_path_accepts_a_real_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cover.png");
        std::fs::write(&file, b"fake png").unwrap();
        let url = format!("file://{}", file.display());
        assert_eq!(resolve_album_art_path(Some(&url)), file.canonicalize().unwrap().to_string_lossy());
    }

    #[test]
    fn resolve_album_art_path_is_empty_for_a_dangling_file_uri() {
        assert_eq!(resolve_album_art_path(Some("file:///nonexistent/path/art.png")), "");
    }

    #[test]
    fn resolve_album_art_path_is_empty_for_a_non_file_scheme() {
        assert_eq!(resolve_album_art_path(Some("https://example.com/art.png")), "");
    }

    #[test]
    fn resolve_album_art_path_is_empty_when_absent() {
        assert_eq!(resolve_album_art_path(None), "");
    }

    #[test]
    fn clamp_seek_target_clamps_below_zero_up_to_zero() {
        assert_eq!(clamp_seek_target(-500, 10_000), 0);
    }

    #[test]
    fn clamp_seek_target_clamps_above_length_down_to_length() {
        assert_eq!(clamp_seek_target(50_000, 10_000), 10_000);
    }

    #[test]
    fn clamp_seek_target_passes_through_an_in_range_value() {
        assert_eq!(clamp_seek_target(5_000, 10_000), 5_000);
    }

    #[test]
    fn clamp_seek_target_only_applies_the_lower_bound_when_length_is_the_unavailable_sentinel() {
        assert_eq!(clamp_seek_target(-1, -1), 0);
        assert_eq!(clamp_seek_target(999_999_999, -1), 999_999_999);
    }
}
