//! Notifications capability (`obelisk.notifications`, ADR-0033). Hosts
//! `org.freedesktop.Notifications` with a 100-item FIFO, a 20-item newest-first feed view, global
//! DND, and a Lua-configured per-urgency PipeWire sound registry. `sound-file` overrides a tier
//! default for one notification; `suppress-sound` wins; `sound-name` picks a freedesktop theme
//! sound in place of a registered tier default.
//!
//! Like `dbus::tray`, a controller owns writes, degrades to inert without the session bus, and
//! delegates decisions to pure helpers. Queue/DND are global Supervisor state (ADR-0033), not
//! per-generation; there is no `reset_registrations`.
//!
//! The parser replaces the base spec's blanket strip-to-plain-text sanitizer with a wider allowlist
//! grammar: five constructs are allowlisted (`<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`);
//! everything else is rejected. Images, `image-path`, and action icons share
//! [`validate_trusted_path`]: an existing regular file under a canonicalized trusted root,
//! otherwise no icon and no error.

use serde::Serialize;

use crate::capabilities::truncate_utf8_bytes;

pub mod controller;
pub mod icon;
pub mod markup;
pub mod queue;
pub mod sound;

pub use controller::{
    NotificationsController, parse_dismiss_args, parse_hold_expiry_args, parse_invoke_action_args, parse_reply_args,
    parse_set_sound_args,
};
pub use sound::run_sound_player;

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationsAction {
    /// (id: integer) Removes a queued notification.
    Dismiss,
    /// (id: integer, key: string) Invokes an `actions[].key`, or `"default"`.
    InvokeAction,
    /// (id: integer, text: string) Sends reply text to a notification with `has_reply`.
    Reply,
    /// (urgency: "low"|"normal"|"critical", path: string) Registers an Ogg Vorbis sound file for an urgency tier.
    SetSound,
    /// (enabled: boolean) Gates non-critical notification sounds.
    SetDnd,
    /// (enabled: boolean) Gates non-critical sounds like `set_dnd` without changing DND, for a config's own rules.
    SetQuiet,
    /// (seconds: integer) Holds expiry countdowns this long; `0` releases the hold.
    HoldExpiry,
}

/// Dispatch (ADR-0037): signal-emitting `dismiss`/`invoke_action`/`reply` use `tokio::spawn`
/// (ADR-0029); locked state writes run inline (ADR-0033).
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
        NotificationsAction::SetQuiet => match crate::capabilities::parse_bool_arg(&params.arguments) {
            Some(enabled) => controller.set_quiet(enabled),
            None => crate::log_malformed_command(params),
        },
        NotificationsAction::HoldExpiry => match parse_hold_expiry_args(&params.arguments) {
            Some(seconds) => controller.hold_expiry(seconds),
            None => crate::log_malformed_command(params),
        },
    }
}

/// Well-known bus name and object path for `org.freedesktop.Notifications`.
pub const NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_OBJECT_PATH: &str = "/org/freedesktop/Notifications";

/// Property caps (ADR-0033), measured in bytes and truncated at UTF-8 boundaries.
const MAX_APP_NAME_BYTES: usize = 64;
const MAX_SUMMARY_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 512;

/// Action caps (ADR-0090): unprivileged session-bus input is drawn by config. Eight exceeds the
/// reference config's busiest card (three); labels are capped tighter than summaries.
const MAX_ACTIONS: usize = 8;
const MAX_ACTION_LABEL_BYTES: usize = 64;

/// Theme name from `app_icon` (ADR-0091), capped like the short identifier `app_name`.
const MAX_APP_ICON_NAME_BYTES: usize = MAX_APP_NAME_BYTES;

/// `desktop-entry` cap (ADR-0101). Reverse-DNS ids top out around 40 bytes; use the summary cap.
const MAX_DESKTOP_ENTRY_BYTES: usize = MAX_SUMMARY_BYTES;
/// Reply placeholder cap (ADR-0101), matching a drawn action label.
const MAX_REPLY_PLACEHOLDER_BYTES: usize = MAX_ACTION_LABEL_BYTES;

/// Backing FIFO cap and `notifications.feed` view size (ADR-0033).
const NOTIFICATION_QUEUE_CAP: usize = 100;
const NOTIFICATION_FEED_VIEW: usize = 20;

/// Raw image-data dimension cap, shared with `dbus::tray`.
const MAX_IMAGE_DIMENSION: i32 = 128;

/// Server default for `expire_timeout == -1`, matching mako/dunst (ADR-0033).
const DEFAULT_EXPIRE_MS: u64 = 5000;

/// `GetCapabilities`'s exact 10 strings (ADR-0033). Only `icon-multi` is absent because `Notify`
/// has no multi-size wire field. `sound` honors `sound-file`, `sound-name` and the tier default via
/// [`should_play_sound`].
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

// Wire-facing types (ADR-0033).

/// One allowlisted body-markup run (CONTEXT.md, ADR-0033). Text carries styling and link target;
/// images carry only a spooled/validated path. `alt` is parsed but not carried.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind")]
pub enum NotificationSpan {
    #[serde(rename = "text")]
    Text {
        /// Unescaped text; empty runs are omitted.
        text: String,
        /// Whether the run was inside `<b>`.
        bold: bool,
        /// Whether the run was inside `<i>`.
        italic: bool,
        /// Whether the run was inside `<u>`.
        underline: bool,
        /// `<a href>` target, or `nil` when not a link. Config decides whether to open it.
        href: Option<String>,
    },
    #[serde(rename = "image")]
    Image {
        /// Existing absolute path under a trusted root; outside paths are dropped during parsing.
        image_path: String,
    },
}

/// One offered action button (ADR-0090), excluding `default` activation and `inline-reply`, which
/// become [`Notification::has_default_action`] and [`Notification::has_reply`]. The flat array
/// was once read for one bool and discarded, so `GetCapabilities` advertised `actions` and
/// `action-icons` while neither was true; parsed buttons are retained now.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NotificationAction {
    /// Opaque key accepted by `:invoke("invoke_action", id, key)` and returned as
    /// `ActionInvoked.action_key`.
    pub key: String,
    /// Button label, falling back to the key when empty unless the action is icon-only.
    pub label: String,
    /// Theme icon name when `action-icons` is set; never a path or resolved here. Keys containing
    /// `/` are refused to prevent a sender naming arbitrary files (ADR-0054 decision 2).
    pub icon_name: Option<String>,
}

/// `low`/`normal`/`critical` urgency tier, also used as the sound-registry key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub enum Urgency {
    #[serde(rename = "low")]
    Low,
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "critical")]
    Critical,
}

/// Maps raw urgency byte 0/1/2; absent or malformed hints default to `Normal` (ADR-0033).
fn urgency_from_hint_byte(byte: Option<u8>) -> Urgency {
    match byte {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// Carries `desktop-entry` (ADR-0101), or `None` when absent, empty, or containing `/`. Desktop
/// ids use dashes for subdirectories, so a slash is not an id and cannot reach `by_app_id`.
fn desktop_entry_from_hint(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty() && !value.contains('/'))?;
    Some(truncate_utf8_bytes(value, MAX_DESKTOP_ENTRY_BYTES))
}

/// Carries the capped reply placeholder (ADR-0101), or `None` when absent/empty.
fn reply_placeholder_from_hint(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty())?;
    Some(truncate_utf8_bytes(value, MAX_REPLY_PLACEHOLDER_BYTES))
}

/// Parses `notifications:set_sound`'s `low`/`normal`/`critical` urgency string.
fn parse_urgency_str(value: &str) -> Option<Urgency> {
    match value {
        "low" => Some(Urgency::Low),
        "normal" => Some(Urgency::Normal),
        "critical" => Some(Urgency::Critical),
        _ => None,
    }
}

/// Queued `notifications.feed[]` object (ADR-0033, ADR-0090). `expire_timeout` and
/// `replaces_id` affect processing but are not feed data.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Notification {
    /// Server id, starting at `1`; used by dismiss/reply/action and reused by replacement.
    pub id: u32,
    /// Arrival time in Unix epoch seconds, matching `obelisk.system.time`; age is
    /// `system.time - timestamp`. Replacements get fresh timestamps; carried because configs
    /// cannot recover history inside ADR-0021 side-effect-free `computed`s.
    pub timestamp: i64,
    /// Sending application, truncated to 64 bytes at a character boundary.
    pub app_name: String,
    /// Plain-text title, truncated to 128 bytes at a character boundary; markup is parsed out.
    pub summary: String,
    /// Body spans, truncated to 512 bytes before parsing. Text carries bold/italic/underline/href;
    /// images carry trusted paths, so config draws without parsing markup.
    pub body: Vec<NotificationSpan>,
    /// Attached picture (album art/avatar/thumbnail) as an existing absolute path: decoded image
    /// spooled to runtime storage or a trusted sender path. `nil` when absent; never a theme name
    /// (ADR-0091). Formerly shared `icon_path` with the application icon; now separate.
    pub image_path: Option<String>,
    /// Application icon: theme name (for example `"firefox"`) or trusted absolute path; `nil` if
    /// neither was supplied. Feeds `icon { name = ... }` (ADR-0054 decision 2). ADR-0091 fixed
    /// the former bug that sent theme names through absolute-path validation, leaving nearly every
    /// notification with the generic fallback.
    pub app_icon: Option<String>,
    /// `"low"`, `"normal"`, or `"critical"`; missing hint means `"normal"`. Critical bypasses DND
    /// and never expires.
    pub urgency: Urgency,
    /// Whether the timeout expired. Ordinary expiry retires the entry from popups but leaves it in
    /// feed history (ADR-0100), after `NotificationClosed(id, reason=1)`; replacements reset it.
    /// Never true for critical or `expire_timeout = 0` notifications.
    pub expired: bool,
    /// `hints["transient"]`: popup-only. Expired transient entries are removed, not retired,
    /// so history never sees them (ADR-0100).
    pub transient: bool,
    /// `hints["desktop-entry"]` id, e.g. `"org.telegram.desktop"`, used by
    /// `obelisk.applications.by_app_id` instead of the mutable/non-unique `app_name`.
    /// `nil` when absent; slashed values are dropped (ADR-0101).
    pub desktop_entry: Option<String>,
    /// Whether the sender offered inline reply; `:invoke("reply", id, text)` requires it.
    pub has_reply: bool,
    /// `hints["x-kde-reply-placeholder-text"]`: what the sender wants an empty reply field to say,
    /// "Reply to Alice" rather than a generic "Reply"; capped at 64 bytes, `nil` if absent, and
    /// meaningless without [`Notification::has_reply`] (ADR-0101).
    pub reply_placeholder: Option<String>,
    /// Offered buttons in sender order, excluding `default` and `inline-reply`; often empty.
    pub actions: Vec<NotificationAction>,
    /// Whether the card is activatable via `:invoke("invoke_action", id, "default")`; separate
    /// from `actions` because `default` is not a button.
    pub has_default_action: bool,
    /// `hints["resident"]`: keep the notification after an action, as media prev/next needs;
    /// bookkeeping only and omitted from payload (`#[serde(skip)]`).
    #[serde(skip)]
    pub resident: bool,
    /// Bookkeeping only (`#[serde(skip)]`): incremented per `Notify` placement so stale expiry
    /// timers can detect replacement (see [`find_expiring_entry`]).
    #[serde(skip)]
    pub incarnation: u64,
}

/// `notifications.feed`/`notifications.dnd` `StateSnapshot` payload (ADR-0033).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NotificationsState {
    /// Newest 20 first, including unread retired entries until dismissed (ADR-0100); `expired`
    /// distinguishes them. This is a view of the 100-entry queue, so older entries remain
    /// dismissable by id after leaving the list (ADR-0033).
    pub feed: Vec<Notification>,
    /// DND from `:invoke("set_dnd", enabled)`; gates only non-critical sounds. Notifications remain
    /// accepted, queued, and in `feed`; popup suppression is config policy.
    pub dnd: bool,
}

/// Channel carrying queue/DND changes, matching `dbus::tray::TraySignal`'s single variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationsSignal {
    Changed,
}

/// Shared by submodule tests.
#[cfg(test)]
mod test_support {
    use super::NotificationSpan;

    /// Builds a text span from plain arguments.
    pub(super) fn text(text: &str, bold: bool, italic: bool, underline: bool, href: Option<&str>) -> NotificationSpan {
        NotificationSpan::Text { text: text.to_string(), bold, italic, underline, href: href.map(str::to_string) }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::text;
    use super::*;

    #[test]
    fn app_name_summary_body_caps_match_the_spec() {
        let long = "x".repeat(1000);
        assert_eq!(truncate_utf8_bytes(&long, MAX_APP_NAME_BYTES).len(), MAX_APP_NAME_BYTES);
        assert_eq!(truncate_utf8_bytes(&long, MAX_SUMMARY_BYTES).len(), MAX_SUMMARY_BYTES);
        assert_eq!(truncate_utf8_bytes(&long, MAX_BODY_BYTES).len(), MAX_BODY_BYTES);
    }

    #[test]
    fn urgency_from_hint_byte_maps_the_three_defined_values() {
        assert_eq!(urgency_from_hint_byte(Some(0)), Urgency::Low);
        assert_eq!(urgency_from_hint_byte(Some(1)), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(2)), Urgency::Critical);
    }

    #[test]
    fn a_desktop_entry_hint_is_carried_unless_it_is_empty_or_looks_like_a_path() {
        assert_eq!(desktop_entry_from_hint(Some("org.telegram.desktop")), Some("org.telegram.desktop".to_string()));
        assert_eq!(desktop_entry_from_hint(Some("")), None);
        assert_eq!(desktop_entry_from_hint(Some("../../etc/passwd")), None);
        assert_eq!(desktop_entry_from_hint(Some("/usr/share/applications/x.desktop")), None);
        assert_eq!(desktop_entry_from_hint(None), None);
        assert_eq!(desktop_entry_from_hint(Some(&"a".repeat(500))).unwrap().len(), MAX_DESKTOP_ENTRY_BYTES);
    }

    #[test]
    fn a_reply_placeholder_hint_is_carried_capped_and_never_empty() {
        assert_eq!(reply_placeholder_from_hint(Some("Reply to Alice")), Some("Reply to Alice".to_string()));
        assert_eq!(reply_placeholder_from_hint(Some("")), None);
        assert_eq!(reply_placeholder_from_hint(None), None);
        assert_eq!(reply_placeholder_from_hint(Some(&"é".repeat(100))).unwrap().len(), MAX_REPLY_PLACEHOLDER_BYTES);
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
