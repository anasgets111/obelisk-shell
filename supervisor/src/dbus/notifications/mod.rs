//! Notifications capability (`oblisk.notifications`, docs/oblisk-supervisor-services-dbus.md §1;
//! docs/oblisk-idl-api-specs.md §2.7/§3.2; docs/adr/0033). Hosts `org.freedesktop.Notifications`
//! on the session bus (its own dedicated connection -- ADR-0033, independent of `dbus::tray`'s),
//! backed by a 100-item FIFO queue (`notifications.feed` is a most-recent-20 view over it), a
//! Supervisor-global do-not-disturb toggle, and a Lua-configured per-urgency sound registry played
//! back through a dedicated PipeWire playback thread. A client's own `hints["sound-file"]` overrides
//! the Lua tier default for that one notification, and `hints["suppress-sound"]` forces silence
//! unconditionally regardless of DND/urgency/anything else registered ([`resolve_sound_path`]);
//! `hints["sound-name"]` (an XDG sound-theme name) is not honored, the same YAGNI call this file
//! already makes for bare icon theme names -- no theme-resolution capability exists.
//!
//! Mirrors `dbus::tray`'s shapes throughout: a `*Controller` struct holding everything write
//! actions need, degrade-to-inert on a missing/lost session bus, and pure, unit-testable helper
//! functions doing every real decision -- the D-Bus glue (`#[zbus::interface]` methods) stays thin,
//! calling into them. Unlike tray, notifications is not per-generation scoped (ADR-0033: "the queue
//! and DND state are global Supervisor state, same tier as tray's item registry") -- there is no
//! `reset_registrations` analog here.
//!
//! Five allowlisted body-markup constructs (`<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`) replace
//! the base spec doc's blanket strip-to-plain-text sanitizer with a wider allowlist grammar
//! ([`parse_markup`]) -- everything else (script/style tags, any other element, malformed/unclosed
//! constructs) is still rejected exactly as before. `<img src>`, `image-path`, and action-icon
//! references all resolve through one path-trust validator ([`validate_trusted_path`]): an absolute
//! path under a small trusted-directory allowlist, confirmed via `canonicalize()` to really exist as
//! a regular file inside one of them (closes off path traversal and symlink escape) -- anything else
//! degrades to no icon, never an error.

use serde::Serialize;

pub mod controller;
pub mod icon;
pub mod markup;
pub mod queue;
pub mod sound;

pub use controller::{NotificationsController, parse_dismiss_args, parse_reply_args, parse_set_sound_args};
pub use sound::run_sound_player;

/// `oblisk.notifications`'s action dispatch (ADR-0037): `dismiss`/`reply` emit D-Bus signals and
/// get `tokio::spawn`ed (ADR-0029); `set_sound`/`set_dnd` only write Supervisor-held state under
/// its lock (ADR-0033), so they run inline.
pub fn dispatch(controller: &NotificationsController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "dismiss" => match parse_dismiss_args(&params.arguments) {
            Some(id) => {
                let controller = controller.clone();
                tokio::spawn(async move { controller.dismiss(id).await; });
            }
            None => crate::log_malformed_command(params),
        },
        "reply" => match parse_reply_args(&params.arguments) {
            Some((id, text)) => {
                let controller = controller.clone();
                tokio::spawn(async move { controller.reply(id, text).await; });
            }
            None => crate::log_malformed_command(params),
        },
        "set_sound" => match parse_set_sound_args(&params.arguments) {
            Some((urgency, path)) => controller.set_sound(urgency, &path),
            None => crate::log_malformed_command(params),
        },
        "set_dnd" => match crate::dbus::parse_bool_arg(&params.arguments) {
            Some(enabled) => controller.set_dnd(enabled),
            None => crate::log_malformed_command(params),
        },
        _ => crate::log_unknown_action(params),
    }
}

/// Well-known bus name and object path this controller hosts `org.freedesktop.Notifications` at
/// (the base freedesktop notification spec's own fixed path).
pub const NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_OBJECT_PATH: &str = "/org/freedesktop/Notifications";

/// §1.1's property caps (ADR-0033 keeps these as-is): bytes, not chars -- multi-byte UTF-8 must
/// truncate on a char boundary at-or-before the cap, never splitting a sequence.
const MAX_APP_NAME_BYTES: usize = 64;
const MAX_SUMMARY_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 512;

/// The backing FIFO's hard cap and `notifications.feed`'s truncated view size over it
/// (ADR-0033: "a truncated view over the 100-item backing FIFO, not an independent structure").
const NOTIFICATION_QUEUE_CAP: usize = 100;
const NOTIFICATION_FEED_VIEW: usize = 20;

/// Raw `image-data`/`icon_data` hint bounds cap. Reuses `dbus::tray`'s own 128px cap rather than
/// inventing a second, undocumented threshold: nothing checked (ADR-0033's own research pass, or
/// this round's own scope) shows notification images legitimately needing to be larger than a tray
/// icon, and a single documented cap is easier to audit than two.
const MAX_IMAGE_DIMENSION: i32 = 128;

/// `expire_timeout == -1`'s "a sensible server default" (docs/oblisk-supervisor-services-dbus.md
/// §1 is silent on the exact value; ADR-0033 picks this to match the mako/dunst convention it
/// cites).
const DEFAULT_EXPIRE_MS: u64 = 5000;

/// `GetCapabilities`'s exact 10 strings (ADR-0033) -- excludes only `icon-multi`: no wire mechanism
/// for multiple icon sizes exists in the base `Notify()` signature, so the capability has nothing
/// to attach to. `sound` is included and genuinely honored: [`resolve_sound_path`] plays a client's
/// own `sound-file` hint (overriding the Lua tier default) or the tier default itself, gated by
/// [`should_play_sound`]; only `sound-name` (an XDG sound-theme name) stays unhonored.
const NOTIFICATIONS_CAPABILITIES: [&str; 10] = [
    "action-icons",
    "actions",
    "body",
    "body-hyperlinks",
    "body-images",
    "body-markup",
    "icon-static",
    "persistence",
    "sound",
    "inline-reply",
];

// -------------------------------------------------------------------------------------------
// Wire-facing types (docs/oblisk-idl-api-specs.md §2.7, corrected by ADR-0033).
// -------------------------------------------------------------------------------------------

/// One allowlisted body-markup run (CONTEXT.md's "Notification body span"; ADR-0033). A text run
/// carries its own styling and, for a `<a href>`, the link target; an image run carries only a
/// spooled/validated path -- `alt` text is parsed (for grammar completeness) but not carried
/// forward, since nothing in this round's scope reads it (no renderer-side consumer exists yet,
/// build-steps.md Phase 16's "Notifications body-span rendering" follow-up).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum NotificationSpan {
    #[serde(rename = "text")]
    Text { text: String, bold: bool, italic: bool, underline: bool, href: Option<String> },
    #[serde(rename = "image")]
    Image { image_path: String },
}

/// The `low`/`normal`/`critical` tier (CONTEXT.md's "Notification urgency"). `Hash`/`Eq` so it can
/// key the sound registry directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
pub enum Urgency {
    #[serde(rename = "low")]
    Low,
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "critical")]
    Critical,
}

/// `hints["urgency"]`'s raw byte (0/1/2), defaulting to `Normal` for anything absent or malformed
/// (docs/oblisk-supervisor-services-dbus.md §1 doesn't specify a default; ADR-0033/base-spec
/// convention treats an unset/invalid urgency hint as normal, not an error).
fn urgency_from_hint_byte(byte: Option<u8>) -> Urgency {
    match byte {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// `notifications:set_sound(urgency, path)`'s `urgency` argument, one of the three wire strings.
fn parse_urgency_str(value: &str) -> Option<Urgency> {
    match value {
        "low" => Some(Urgency::Low),
        "normal" => Some(Urgency::Normal),
        "critical" => Some(Urgency::Critical),
        _ => None,
    }
}

/// One queued notification -- `notifications.feed[]`'s object shape (docs/oblisk-idl-api-specs.md
/// §2.7, ADR-0033's corrections: `body` is a span array not a flat string, `urgency`/`has_reply`
/// are new fields). Trimmed to exactly what the feed shape and the write commands below need --
/// the raw `actions` array itself is never stored, only the `has_reply` bool it's collapsed into
/// (see [`actions_have_reply`]), since no write command exists yet to invoke an arbitrary action.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: Vec<NotificationSpan>,
    pub icon_path: Option<String>,
    pub urgency: Urgency,
    pub has_reply: bool,
    /// Bookkeeping only -- never part of the documented wire shape, hence `#[serde(skip)]`.
    /// Bumped fresh every time `Notify` places new content at this id (a brand-new arrival or a
    /// `replaces_id` update alike -- see `notify()`'s own `next_incarnation` allocation). Lets a
    /// stale expiry timer spawned for an earlier incarnation tell it's been superseded before it
    /// removes content it no longer describes (finding 2 -- see [`find_expiring_entry`]).
    #[serde(skip)]
    pub incarnation: u64,
}

/// `notifications.feed`/`notifications.dnd`'s `StateSnapshot` payload shape (ADR-0033: "reuses
/// `StateSnapshot`, no new `SupervisorFrame` variant").
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NotificationsState {
    pub feed: Vec<Notification>,
    pub dnd: bool,
}

/// The channel `NotificationsController` fans a queue/DND mutation out through -- mirrors
/// `dbus::tray::TraySignal`'s single-variant shape exactly (every mutation here, like every tray
/// registry change, collapses to "go rebuild and push a fresh `StateSnapshot`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationsSignal {
    Changed,
}

// -------------------------------------------------------------------------------------------
// Property truncation (TDD seam 1): byte-capped, char-boundary-safe.
// -------------------------------------------------------------------------------------------

/// Truncates `input` to at most `max_bytes` UTF-8 bytes, backing off to the nearest char boundary
/// at-or-before that cap rather than ever splitting a multi-byte sequence (§1.1's byte caps taken
/// literally: "bytes, not chars").
fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}


/// Shared by every submodule's own `#[cfg(test)]` -- see [`test_support::text`] for why this
/// lives here instead of being copied into each one.
#[cfg(test)]
mod test_support {
    use super::NotificationSpan;

    /// A single unstyled-or-styled [`NotificationSpan::Text`] literal, built from plain args
    /// instead of the verbose struct-literal form every test that touches spans would otherwise
    /// repeat.
    pub(super) fn text(text: &str, bold: bool, italic: bool, underline: bool, href: Option<&str>) -> NotificationSpan {
        NotificationSpan::Text { text: text.to_string(), bold, italic, underline, href: href.map(str::to_string) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::text;

    // ---- truncate_utf8_bytes (TDD seam 1) ----

    #[test]
    fn truncate_utf8_bytes_is_a_no_op_under_the_cap() {
        assert_eq!(truncate_utf8_bytes("hello", 64), "hello");
    }

    #[test]
    fn truncate_utf8_bytes_truncates_ascii_at_the_exact_cap() {
        assert_eq!(truncate_utf8_bytes("hello world", 5), "hello");
    }

    #[test]
    fn truncate_utf8_bytes_never_splits_a_multibyte_char() {
        // "héllo" -- 'é' is 2 bytes (0xc3 0xa9); a byte cap landing mid-character must back off.
        let input = "héllo";
        assert_eq!(input.len(), 6);
        // Cap of 2 bytes lands right in the middle of 'é' (byte 1 is not a char boundary).
        let truncated = truncate_utf8_bytes(input, 2);
        assert_eq!(truncated, "h");
        assert!(truncated.len() <= 2);
    }

    #[test]
    fn truncate_utf8_bytes_handles_a_cap_of_zero() {
        assert_eq!(truncate_utf8_bytes("hello", 0), "");
    }

    #[test]
    fn app_name_summary_body_caps_match_the_spec() {
        let long = "x".repeat(1000);
        assert_eq!(truncate_utf8_bytes(&long, MAX_APP_NAME_BYTES).len(), MAX_APP_NAME_BYTES);
        assert_eq!(truncate_utf8_bytes(&long, MAX_SUMMARY_BYTES).len(), MAX_SUMMARY_BYTES);
        assert_eq!(truncate_utf8_bytes(&long, MAX_BODY_BYTES).len(), MAX_BODY_BYTES);
    }

    // ---- urgency_from_hint_byte / parse_urgency_str ----

    #[test]
    fn urgency_from_hint_byte_maps_the_three_defined_values() {
        assert_eq!(urgency_from_hint_byte(Some(0)), Urgency::Low);
        assert_eq!(urgency_from_hint_byte(Some(1)), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(2)), Urgency::Critical);
    }

    #[test]
    fn urgency_from_hint_byte_defaults_to_normal_when_absent_or_malformed() {
        assert_eq!(urgency_from_hint_byte(None), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(99)), Urgency::Normal);
    }

    #[test]
    fn parse_urgency_str_matches_the_three_wire_strings() {
        assert_eq!(parse_urgency_str("low"), Some(Urgency::Low));
        assert_eq!(parse_urgency_str("normal"), Some(Urgency::Normal));
        assert_eq!(parse_urgency_str("critical"), Some(Urgency::Critical));
        assert_eq!(parse_urgency_str("urgent"), None);
    }

    // ---- serde wire shape ----

    #[test]
    fn notification_span_text_serializes_with_a_kind_tag() {
        let span = text("hi", true, false, false, Some("url"));
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "text", "text": "hi", "bold": true, "italic": false, "underline": false, "href": "url" }));
    }

    #[test]
    fn notification_span_image_serializes_with_a_kind_tag() {
        let span = NotificationSpan::Image { image_path: "/tmp/x.png".to_string() };
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "image", "image_path": "/tmp/x.png" }));
    }

    #[test]
    fn urgency_serializes_as_lowercase_strings() {
        assert_eq!(serde_json::to_value(Urgency::Low).unwrap(), serde_json::json!("low"));
        assert_eq!(serde_json::to_value(Urgency::Normal).unwrap(), serde_json::json!("normal"));
        assert_eq!(serde_json::to_value(Urgency::Critical).unwrap(), serde_json::json!("critical"));
    }
}
