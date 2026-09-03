//! Notifications capability (`oblisk.notifications`, docs/oblisk-supervisor-services-dbus.md §1;
//! docs/oblisk-idl-api-specs.md §2.7/§3.2; ADR-0033). Hosts `org.freedesktop.Notifications`
//! on its own session-bus connection, backed by a 100-item FIFO queue (`notifications.feed` is a
//! most-recent-20 view over it), a Supervisor-global do-not-disturb toggle, and a Lua-configured
//! per-urgency sound registry played back through a dedicated PipeWire playback thread. A
//! client's own `hints["sound-file"]` overrides the Lua tier default for that one notification,
//! and `hints["suppress-sound"]` forces silence unconditionally ([`resolve_sound_path`]);
//! `hints["sound-name"]` (an XDG sound-theme name) is not honored -- no theme-resolution
//! capability exists.
//!
//! Mirrors `dbus::tray`'s shapes: a `*Controller` struct holding everything write actions need,
//! degrade-to-inert on a missing/lost session bus, and pure, unit-testable helpers doing every
//! real decision, with thin D-Bus glue calling into them. Unlike tray, notifications is not
//! per-generation scoped (ADR-0033: the queue and DND state are global Supervisor state) -- no
//! `reset_registrations` analog here.
//!
//! Five allowlisted body-markup constructs (`<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`) replace
//! the base spec's blanket strip-to-plain-text sanitizer with a wider allowlist grammar
//! ([`parse_markup`]) -- everything else is still rejected. `<img src>`, `image-path`, and
//! action-icon references all resolve through one path-trust validator
//! ([`validate_trusted_path`]): an absolute path under a small trusted-directory allowlist,
//! confirmed via `canonicalize()` to really exist as a regular file inside one of them --
//! anything else degrades to no icon, never an error.

use serde::Serialize;

pub mod controller;
pub mod icon;
pub mod markup;
pub mod queue;
pub mod sound;

pub use controller::{
    NotificationsController, parse_dismiss_args, parse_invoke_action_args, parse_reply_args, parse_set_sound_args,
};
pub use sound::run_sound_player;

/// Every action `oblisk.notifications:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationsAction {
    Dismiss,
    InvokeAction,
    Reply,
    SetSound,
    SetDnd,
}

/// `oblisk.notifications`'s action dispatch (ADR-0037): `dismiss`/`invoke_action`/`reply` emit
/// D-Bus signals and get `tokio::spawn`ed (ADR-0029); `set_sound`/`set_dnd` only write Supervisor-held state under
/// its lock (ADR-0033), so they run inline.
pub fn dispatch(controller: &NotificationsController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<NotificationsAction>(params) else { return };
    match action {
        NotificationsAction::Dismiss => match parse_dismiss_args(&params.arguments) {
            Some(id) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.dismiss(id).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        NotificationsAction::InvokeAction => match parse_invoke_action_args(&params.arguments) {
            Some((id, key)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.invoke_action(id, key).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        NotificationsAction::Reply => match parse_reply_args(&params.arguments) {
            Some((id, text)) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.reply(id, text).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        NotificationsAction::SetSound => match parse_set_sound_args(&params.arguments) {
            Some((urgency, path)) => controller.set_sound(urgency, &path),
            None => crate::log_malformed_command(params),
        },
        NotificationsAction::SetDnd => match crate::capabilities::parse_bool_arg(&params.arguments) {
            Some(enabled) => controller.set_dnd(enabled),
            None => crate::log_malformed_command(params),
        },
    }
}

/// Well-known bus name and object path this controller hosts `org.freedesktop.Notifications` at
/// (the base freedesktop notification spec's own fixed path).
pub const NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_OBJECT_PATH: &str = "/org/freedesktop/Notifications";

/// §1.1's property caps (ADR-0033): bytes, not chars -- multi-byte UTF-8 must truncate on a char
/// boundary at-or-before the cap, never splitting a sequence.
const MAX_APP_NAME_BYTES: usize = 64;
const MAX_SUMMARY_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 512;

/// `actions`' own caps (ADR-0090), on the same reasoning §1.1 caps the text properties: the array
/// arrives from an unprivileged sender over the session bus and a config draws every entry. Eight
/// is past anything a real notification offers -- the reference config's own busiest card has
/// three -- and a label is a button, so it is capped tighter than a summary.
const MAX_ACTIONS: usize = 8;
const MAX_ACTION_LABEL_BYTES: usize = 64;

/// A theme name carried out of `app_icon` (ADR-0091). Same cap as `app_name`, which is the same
/// kind of value from the same untrusted sender: a short identifier, not prose.
const MAX_APP_ICON_NAME_BYTES: usize = MAX_APP_NAME_BYTES;

/// The backing FIFO's hard cap and `notifications.feed`'s truncated view size over it (ADR-0033).
const NOTIFICATION_QUEUE_CAP: usize = 100;
const NOTIFICATION_FEED_VIEW: usize = 20;

/// Raw `image-data`/`icon_data` hint bounds cap. Reuses `dbus::tray`'s own 128px cap rather than
/// inventing a second, undocumented threshold.
const MAX_IMAGE_DIMENSION: i32 = 128;

/// `expire_timeout == -1`'s "a sensible server default" (ADR-0033 picks this to match the
/// mako/dunst convention it cites).
const DEFAULT_EXPIRE_MS: u64 = 5000;

/// `GetCapabilities`'s exact 10 strings (ADR-0033) -- excludes only `icon-multi`: no wire
/// mechanism for multiple icon sizes exists in the base `Notify()` signature. `sound` is
/// genuinely honored: [`resolve_sound_path`] plays a client's own `sound-file` hint or the tier
/// default, gated by [`should_play_sound`]; only `sound-name` stays unhonored.
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
/// spooled/validated path -- `alt` text is parsed for grammar completeness but not carried
/// forward, since nothing in this round's scope reads it.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind")]
pub enum NotificationSpan {
    #[serde(rename = "text")]
    Text {
        /// The run's text, already unescaped. Empty runs are not emitted.
        text: String,
        /// The run sat inside `<b>`.
        bold: bool,
        /// The run sat inside `<i>`.
        italic: bool,
        /// The run sat inside `<u>`.
        underline: bool,
        /// The `<a href>` target this run links to, or `nil` for a run that is not a link. Carried
        /// as text, not opened: launching it is a config's decision.
        href: Option<String>,
    },
    #[serde(rename = "image")]
    Image {
        /// An absolute path to an image that exists under a trusted root. A path outside one is
        /// dropped during parsing rather than carried and refused later.
        image_path: String,
    },
}

/// One action button a sender offered (ADR-0090). `Notify` carries these as a flat
/// `[key1, label1, key2, label2, ...]` array, which was read for one bool and thrown away until
/// now -- so `GetCapabilities` advertised `actions` and `action-icons` and neither was true.
///
/// The two keys with meanings of their own are not in here: `"default"` is the whole
/// notification's activation and becomes [`Notification::has_default_action`], and
/// `"inline-reply"` becomes [`Notification::has_reply`]. Both would otherwise draw as buttons
/// beside the ones a sender actually meant as buttons.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NotificationAction {
    /// What `notifications:invoke_action(id, key)` takes, and what travels back to the sender as
    /// `ActionInvoked`'s `action_key`. Opaque: it means something to the application and nothing
    /// here.
    pub key: String,
    /// What to draw on the button. The sender's own label, or the key when it sent an empty one
    /// and the action is not icon-only.
    pub label: String,
    /// A *theme icon name*, present only when the sender set the `action-icons` hint, in which
    /// case the base spec says the key is that name. Not a path and never resolved here: `icon`
    /// takes a theme name directly (ADR-0054 decision 2), so there is nothing to spool.
    ///
    /// A key holding a path separator is refused as an icon rather than carried, because `icon`
    /// also accepts an absolute path -- without that check, a sender could name any file on this
    /// machine and have the shell draw it.
    pub icon_name: Option<String>,
}

/// The `low`/`normal`/`critical` tier (CONTEXT.md's "Notification urgency"). `Hash`/`Eq` so it can
/// key the sound registry directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, schemars::JsonSchema)]
pub enum Urgency {
    #[serde(rename = "low")]
    Low,
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "critical")]
    Critical,
}

/// `hints["urgency"]`'s raw byte (0/1/2), defaulting to `Normal` for anything absent or
/// malformed (ADR-0033/base-spec convention: an unset/invalid hint is normal, not an error).
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
/// are new fields; ADR-0090 adds `actions` and `has_default_action`). Trimmed to what the feed
/// shape and the write commands need: `expire_timeout` and `replaces_id` are acted on and not
/// carried, since neither is a thing a config draws or decides.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Notification {
    /// The server-assigned id, counting up from `1`. What `notifications:dismiss`, `:reply` and
    /// `:invoke_action` take. Reused when an application replaces its own notification in place.
    pub id: u32,
    /// The sending application's name, truncated to 64 bytes on a character boundary.
    pub app_name: String,
    /// The title, truncated to 128 bytes on a character boundary. Plain text: any markup the
    /// application sent is parsed out, not rendered.
    pub summary: String,
    /// The message as a run of spans rather than one string, because the freedesktop body is
    /// markup. Truncated to 512 bytes before parsing. Each span is either text carrying its own
    /// bold/italic/underline/href, or an image whose path passed the trusted-root check, so a
    /// config draws the list in order and never has to parse markup itself.
    pub body: Vec<NotificationSpan>,
    /// The picture the sender attached -- album art, an avatar, a screenshot thumbnail -- as an
    /// absolute path to a file that exists: either a decoded, bounds-checked image spooled to
    /// `/dev/shm`, or a path it sent that passed the trusted-root check. `nil` when it attached
    /// none. Never a theme name (ADR-0091).
    ///
    /// Was called `icon_path` and held this *and* the sending application's icon, whichever
    /// arrived first. They are two different pictures with two different jobs, so they are now two
    /// fields; see [`Notification::app_icon`].
    pub image_path: Option<String>,
    /// The sending application's own icon: a theme name like `"firefox"`, or an absolute path when
    /// it sent one that passed the trusted-root check. `nil` when it identified itself with
    /// neither. Feeds `icon { name = ... }`, which takes either form (ADR-0054 decision 2).
    ///
    /// A theme name is the overwhelmingly common case and used to be dropped on the floor: this
    /// value ran through the same absolute-path validator the attached picture does, and
    /// `"firefox"` is not an absolute path, so nearly every real notification arrived with no icon
    /// at all (ADR-0091).
    pub app_icon: Option<String>,
    /// `"low"`, `"normal"` or `"critical"`. `"normal"` for a sender that set no urgency hint.
    /// Critical is the one that outlives do-not-disturb and never expires on its own.
    pub urgency: Urgency,
    /// The sender offered an inline reply action, so `notifications:reply(id, text)` will be
    /// accepted. Calling it on a notification without one is refused, which is why this is
    /// carried rather than guessed.
    pub has_reply: bool,
    /// The buttons the sender offered, in the order it listed them, minus the two keys that mean
    /// something other than a button. Empty for the great majority of notifications.
    pub actions: Vec<NotificationAction>,
    /// The sender offered a `"default"` action: the whole card is activatable, and clicking it
    /// should call `notifications:invoke_action(id, "default")`. Its own field rather than an
    /// entry in `actions`, because it is not a button and drawing it as one is wrong.
    pub has_default_action: bool,
    /// `hints["resident"]`: the sender wants the notification to survive an action being invoked,
    /// which is what a media notification with prev/next buttons needs. Bookkeeping only,
    /// `#[serde(skip)]` -- it decides what [`NotificationsController::invoke_action`] does next
    /// and a config has no use for it.
    #[serde(skip)]
    pub resident: bool,
    /// Bookkeeping only, `#[serde(skip)]`. Bumped every time `Notify` places new content at this
    /// id. Lets a stale expiry timer spawned for an earlier incarnation tell it's been
    /// superseded before removing content it no longer describes (see [`find_expiring_entry`]).
    #[serde(skip)]
    pub incarnation: u64,
}

/// `notifications.feed`/`notifications.dnd`'s `StateSnapshot` payload shape (ADR-0033).
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NotificationsState {
    /// The newest 20 live notifications, most recent first. A truncated view of a 100-deep queue
    /// (ADR-0033), so a notification can leave this list while still being live and still
    /// dismissable by id.
    pub feed: Vec<Notification>,
    /// Do-not-disturb, flipped by `notifications:set_dnd`. It gates exactly one thing in the
    /// Supervisor: a non-critical notification's sound does not play. Notifications are still
    /// accepted, still queued, and still appear in [`NotificationsState::feed`], so not drawing the
    /// popup is the config's decision, and `"critical"` is the urgency worth letting through.
    pub dnd: bool,
}

/// The channel `NotificationsController` fans a queue/DND mutation out through -- mirrors
/// `dbus::tray::TraySignal`'s single-variant shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationsSignal {
    Changed,
}

// -------------------------------------------------------------------------------------------
// Property truncation (TDD seam 1): byte-capped, char-boundary-safe.
// -------------------------------------------------------------------------------------------

/// Truncates `input` to at most `max_bytes` UTF-8 bytes, backing off to the nearest char boundary
/// at-or-before that cap rather than splitting a multi-byte sequence (§1.1: "bytes, not chars").
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

/// Shared by every submodule's own `#[cfg(test)]`.
#[cfg(test)]
mod test_support {
    use super::NotificationSpan;

    /// A single unstyled-or-styled [`NotificationSpan::Text`] literal, built from plain args
    /// instead of the verbose struct-literal form.
    pub(super) fn text(text: &str, bold: bool, italic: bool, underline: bool, href: Option<&str>) -> NotificationSpan {
        NotificationSpan::Text { text: text.to_string(), bold, italic, underline, href: href.map(str::to_string) }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::text;
    use super::*;

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
        assert_eq!(
            json,
            serde_json::json!({ "kind": "text", "text": "hi", "bold": true, "italic": false, "underline": false, "href": "url" })
        );
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
