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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use pipewire as pw;
use regex::Regex;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::Value;

use super::shm_icons::{self, PngEncodeError};

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

// -------------------------------------------------------------------------------------------
// Markup allowlist parser (TDD seam 2): five constructs, everything else stripped.
// -------------------------------------------------------------------------------------------

/// Matches one HTML-ish tag (`<name ...>`, `</name>`, or a self-closing `<name .../>`), double-
/// quoted attribute values only -- matching every example in the spec docs and ADR-0033's own
/// grammar. `regex`'s guaranteed-linear-time matching keeps this "non-backtracking", the same
/// property §1.1's superseded flat-text sanitizer named explicitly.
static TAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"</?[a-zA-Z][a-zA-Z0-9]*(?:\s+[a-zA-Z_:][a-zA-Z0-9_:-]*\s*=\s*"[^"]*")*\s*/?>"#)
        .expect("TAG_PATTERN is a valid, hand-checked regex literal")
});

/// Extracts `key="value"` attribute pairs from a tag's own inner text (double-quoted only).
static ATTR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([a-zA-Z_:][a-zA-Z0-9_:-]*)\s*=\s*"([^"]*)""#).expect("ATTR_PATTERN is a valid, hand-checked regex literal"));

/// One recognized (or explicitly rejected) tag construct -- [`classify_tag`]'s output.
#[derive(Debug, Clone, PartialEq)]
enum ClassifiedTag {
    OpenBold,
    CloseBold,
    OpenItalic,
    CloseItalic,
    OpenUnderline,
    CloseUnderline,
    /// `<a href="URL">` -- an anchor with no `href` attribute is [`ClassifiedTag::Ignored`]
    /// instead, since it isn't a usable anchor construct.
    OpenAnchor(String),
    CloseAnchor,
    /// `<img src="PATH">` (self-closing or not; `alt`, if present, is parsed but discarded --
    /// nothing in this round's scope reads it). No `src` is [`ClassifiedTag::Ignored`].
    Image(String),
    /// `<script>`/`<style>` -- the opening half of an opaque block whose entire content (including
    /// any markup inside it) is discarded up to its matching close tag.
    OpaqueOpen(String),
    /// Anything else: an unrecognized element, a malformed construct, or an allowed tag missing a
    /// required attribute. The tag markup itself is stripped; unlike `OpaqueOpen`, its surrounding
    /// text is not touched -- only script/style content is executable/non-visual enough to drop
    /// outright (§1.1's original "strips out all executable scripts, style tags" carried forward).
    Ignored,
}

fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    ATTR_PATTERN.captures_iter(attrs).find_map(|caps| if caps[1].eq_ignore_ascii_case(key) { Some(caps[2].to_string()) } else { None })
}

/// Classifies one `TAG_PATTERN` match (including its surrounding `<`/`>`) into a
/// [`ClassifiedTag`]. Never panics on malformed input -- everything not recognized falls to
/// [`ClassifiedTag::Ignored`].
fn classify_tag(raw: &str) -> ClassifiedTag {
    let inner = &raw[1..raw.len() - 1];
    let is_closing = inner.starts_with('/');
    let body = if is_closing { &inner[1..] } else { inner };
    let body = body.trim_end_matches('/').trim();

    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let attrs = &body[name_end..];

    if is_closing {
        return match name.as_str() {
            "b" => ClassifiedTag::CloseBold,
            "i" => ClassifiedTag::CloseItalic,
            "u" => ClassifiedTag::CloseUnderline,
            "a" => ClassifiedTag::CloseAnchor,
            _ => ClassifiedTag::Ignored,
        };
    }

    match name.as_str() {
        "b" => ClassifiedTag::OpenBold,
        "i" => ClassifiedTag::OpenItalic,
        "u" => ClassifiedTag::OpenUnderline,
        "a" => extract_attr(attrs, "href").map(ClassifiedTag::OpenAnchor).unwrap_or(ClassifiedTag::Ignored),
        "img" => extract_attr(attrs, "src").map(ClassifiedTag::Image).unwrap_or(ClassifiedTag::Ignored),
        "script" | "style" => ClassifiedTag::OpaqueOpen(name),
        _ => ClassifiedTag::Ignored,
    }
}

/// Whether `raw` (a `TAG_PATTERN` match) is the closing tag matching `opaque_name`.
fn is_closing_tag_named(raw: &str, opaque_name: &str) -> bool {
    let inner = raw.trim_start_matches('<').trim_end_matches('>');
    inner.strip_prefix('/').is_some_and(|name| name.trim().eq_ignore_ascii_case(opaque_name))
}

/// Flushes `current` into a new [`NotificationSpan::Text`] carrying the currently-active style,
/// if it's non-empty. A no-op otherwise -- callers flush unconditionally on every style change and
/// at end-of-input, so most calls see an already-empty `current`.
fn flush_text(spans: &mut Vec<NotificationSpan>, current: &mut String, bold: u32, italic: u32, underline: u32, href: Option<String>) {
    if current.is_empty() {
        return;
    }
    spans.push(NotificationSpan::Text { text: std::mem::take(current), bold: bold > 0, italic: italic > 0, underline: underline > 0, href });
}

/// Parses `input` into [`NotificationSpan`]s, accepting exactly `<b>`, `<i>`, `<u>`,
/// `<a href="URL">`, `<img src="PATH" alt="ALT">` (self-closing or not) and rejecting/stripping
/// everything else (ADR-0033). Pure grammar only -- an `<img>`'s `src` is carried through
/// unvalidated; the real filesystem/path-trust check is a separate step ([`validate_trusted_path`],
/// applied by [`sanitize_body`]) so this function stays testable with no filesystem I/O.
///
/// Style depth counters (not a generic stack) mean nesting composes naturally
/// (`<b><i>x</i></b>` is both bold and italic) and an *unclosed* allowed tag simply applies its
/// style through to end-of-input instead of erroring -- the same lenient convention real
/// notification daemons (mako) use for malformed markup. `href` uses a real stack since nested
/// anchors with different targets are meaningful; the innermost one wins.
fn parse_markup(input: &str) -> Vec<NotificationSpan> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut bold = 0u32;
    let mut italic = 0u32;
    let mut underline = 0u32;
    let mut href_stack: Vec<String> = Vec::new();
    let mut opaque: Option<String> = None;
    let mut last_end = 0;

    for m in TAG_PATTERN.find_iter(input) {
        let literal = &input[last_end..m.start()];
        last_end = m.end();

        if let Some(opaque_name) = &opaque {
            if is_closing_tag_named(m.as_str(), opaque_name) {
                opaque = None;
            }
            continue;
        }

        current.push_str(literal);
        match classify_tag(m.as_str()) {
            ClassifiedTag::OpenBold => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                bold += 1;
            }
            ClassifiedTag::CloseBold => {
                if bold > 0 {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    bold -= 1;
                }
            }
            ClassifiedTag::OpenItalic => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                italic += 1;
            }
            ClassifiedTag::CloseItalic => {
                if italic > 0 {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    italic -= 1;
                }
            }
            ClassifiedTag::OpenUnderline => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                underline += 1;
            }
            ClassifiedTag::CloseUnderline => {
                if underline > 0 {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    underline -= 1;
                }
            }
            ClassifiedTag::OpenAnchor(href) => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                href_stack.push(href);
            }
            ClassifiedTag::CloseAnchor => {
                if !href_stack.is_empty() {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    href_stack.pop();
                }
            }
            ClassifiedTag::Image(src) => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                spans.push(NotificationSpan::Image { image_path: src });
            }
            ClassifiedTag::OpaqueOpen(name) => opaque = Some(name),
            ClassifiedTag::Ignored => {}
        }
    }

    if opaque.is_none() {
        current.push_str(&input[last_end..]);
    }
    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
    spans
}

// -------------------------------------------------------------------------------------------
// Path-trust validator (TDD seam 3): shared by `<img src>`, `image-path`, action-icon names, and
// registered sound files.
// -------------------------------------------------------------------------------------------

/// The trusted icon directories ADR-0033 names, resolved against the real `$HOME`. Production
/// callers use this; tests inject their own roots (real `tempfile` fixtures) directly into
/// [`validate_trusted_path`] instead, so this function itself needs no test coverage of its own.
fn default_trusted_icon_roots() -> Vec<PathBuf> {
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
fn validate_trusted_path(path: &str, trusted_roots: &[PathBuf]) -> Option<PathBuf> {
    let candidate = Path::new(path);
    if !candidate.is_absolute() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.is_file() {
        return None;
    }
    let is_trusted = trusted_roots.iter().any(|root| root.canonicalize().map(|root| canonical.starts_with(&root)).unwrap_or(false));
    is_trusted.then_some(canonical)
}

/// Strips a `file://` URI scheme prefix, if present, leaving a raw filesystem path either way
/// (§1's `image-path`/`app_icon` hints can arrive as either form).
fn strip_file_uri(path: &str) -> &str {
    path.strip_prefix("file://").unwrap_or(path)
}

/// Runs `Notify`'s raw `body` through the full sanitize pipeline: byte-cap truncation
/// ([`truncate_utf8_bytes`]), the allowlist grammar ([`parse_markup`]), then the path-trust
/// validator against every `<img src>` -- an image whose path isn't a real, trusted file is
/// dropped from the body entirely (ADR-0033: "closes off arbitrary local-file disclosure through
/// body markup").
fn sanitize_body(raw_body: &str, trusted_roots: &[PathBuf]) -> Vec<NotificationSpan> {
    let truncated = truncate_utf8_bytes(raw_body, MAX_BODY_BYTES);
    parse_markup(&truncated)
        .into_iter()
        .filter_map(|span| match span {
            NotificationSpan::Image { image_path } => {
                validate_trusted_path(&image_path, trusted_roots).map(|validated| NotificationSpan::Image { image_path: validated.to_string_lossy().into_owned() })
            }
            text_span => Some(text_span),
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// ActionInvoked reply encoding (TDD seam 4).
// -------------------------------------------------------------------------------------------

/// `ActionInvoked`'s `action_key` for a completed inline reply (ADR-0033/Noctalia convention:
/// `"inline-reply::<text>"`).
fn format_reply_action_key(text: &str) -> String {
    format!("inline-reply::{text}")
}

/// Whether `Notify`'s `actions` array (flat `[key1, label1, key2, label2, ...]` pairs) declares
/// the KDE inline-reply extension (§1.2's `x-kde-reply` convention rides on an `"inline-reply"`
/// action key being present).
fn actions_have_reply(actions: &[String]) -> bool {
    actions.chunks(2).any(|pair| pair.first().is_some_and(|key| key == "inline-reply"))
}

// -------------------------------------------------------------------------------------------
// Urgency / critical-bypass expiry logic (TDD seam 5).
// -------------------------------------------------------------------------------------------

/// Whether, and after how long, a notification auto-expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryPolicy {
    Never,
    After(Duration),
}

/// `expire_timeout`'s resolution (docs/oblisk-supervisor-services-dbus.md §1; ADR-0033's
/// "Critical urgency ignores `expire_timeout`" policy fix): critical never expires regardless of
/// what the sender requested; otherwise `0` means never, a negative value means "use the server
/// default" ([`DEFAULT_EXPIRE_MS`], matching mako/dunst), and a positive value is that many
/// milliseconds.
fn resolve_expiry(urgency: Urgency, expire_timeout: i32) -> ExpiryPolicy {
    if urgency == Urgency::Critical {
        return ExpiryPolicy::Never;
    }
    match expire_timeout {
        0 => ExpiryPolicy::Never,
        t if t < 0 => ExpiryPolicy::After(Duration::from_millis(DEFAULT_EXPIRE_MS)),
        t => ExpiryPolicy::After(Duration::from_millis(t as u64)),
    }
}

// -------------------------------------------------------------------------------------------
// DND sound-gating logic (TDD seam 6).
// -------------------------------------------------------------------------------------------

/// Whether a notification's sound should play (ADR-0033): never without a registered sound for
/// its tier; never while do-not-disturb is on unless the notification is critical (critical
/// bypasses DND, mirroring the same bypass [`resolve_expiry`] already applies to `expire_timeout`);
/// otherwise yes.
fn should_play_sound(dnd: bool, urgency: Urgency, sound_registered: bool) -> bool {
    if !sound_registered {
        return false;
    }
    if dnd && urgency != Urgency::Critical {
        return false;
    }
    true
}

/// Resolves *which* sound file (if any) a single `Notify` call plays -- the source-selection half
/// that [`should_play_sound`]'s DND/urgency gate doesn't need to know about; that function only
/// ever sees the result's `is_some()`. Resolution order (ADR-0033: "Sound: Lua sets the per-urgency
/// default, a client's own `sound-file` hint overrides it for that one notification,
/// `suppress-sound` always wins to silence"): `suppress` forces `None` unconditionally, regardless
/// of what either argument carries; else `client_sound_file` (already validated through the same
/// path-trust boundary as `image-path` -- [`validate_trusted_path`]) plays instead of the tier
/// default; else `tier_default` (the urgency's `set_sound` registration, if any) plays; else nothing
/// plays. `hints["sound-name"]` never reaches this function at all -- it isn't honored (ADR-0033),
/// same YAGNI call already made for bare icon theme names.
fn resolve_sound_path(suppress: bool, client_sound_file: Option<PathBuf>, tier_default: Option<PathBuf>) -> Option<PathBuf> {
    if suppress {
        return None;
    }
    client_sound_file.or(tier_default)
}

// -------------------------------------------------------------------------------------------
// Queue mutation (TDD seam 7): FIFO eviction and replace/icon lifecycle, pure `VecDeque`
// mutations -- no filesystem I/O of their own, just reporting which icon file (if any) is now
// orphaned so the caller deletes it in the same step.
// -------------------------------------------------------------------------------------------

/// Allocates the next monotonic incarnation stamp for a piece of content being placed at some id
/// (finding 2). Every `Notify` call bumps this and stamps its [`Notification`] with the result --
/// a fresh arrival and a `replaces_id` update alike -- so a spawned expiry timer can capture "the
/// incarnation I was scheduled for" and later tell whether a newer `Notify` call already
/// superseded it ([`find_expiring_entry`]). No wraparound guard analogous to
/// [`resolve_notification_id`]'s: a `u64` exhausting within one process's lifetime isn't a real
/// scenario to guard against.
fn next_incarnation(counter: &mut u64) -> u64 {
    let value = *counter;
    *counter = counter.wrapping_add(1);
    value
}

/// `replaces_id == 0`'s id half: allocates the next id, monotonic and wrap-safe (never lands on
/// `0`, which is reserved to mean "new" on the wire). `replaces_id != 0` passes through unchanged
/// without bumping the allocator (base spec/ADR-0033: id reuse doesn't consume a fresh id).
fn resolve_notification_id(replaces_id: u32, next_id: &mut u32) -> u32 {
    if replaces_id != 0 {
        return replaces_id;
    }
    let id = *next_id;
    *next_id = if *next_id == u32::MAX { 1 } else { *next_id + 1 };
    id
}

/// What [`push_new`]/[`replace_or_push`] report needs cleaning up after a queue mutation --
/// distinguishes a genuine FIFO eviction (a *different* notification fell off the back of the
/// queue past [`NOTIFICATION_QUEUE_CAP`]; finding 3: this needs both an icon-file deletion and a
/// `NotificationClosed(evicted_id, reason=Evicted)` signal, since the evicted id is now gone for
/// good) from a same-id icon replacement (no signal -- the id itself is still very much present in
/// the queue, just its old icon file is now orphaned).
#[derive(Debug, Clone, PartialEq)]
enum QueueCleanup {
    /// Same id, old icon path now orphaned by a `replaces_id` update that changed/cleared the icon.
    ReplacedIcon(String),
    /// A different notification fell off the back of the queue entirely.
    Evicted { id: u32, icon_path: Option<String> },
}

/// Appends `notification` (a genuinely new arrival), evicting the oldest entry past
/// [`NOTIFICATION_QUEUE_CAP`] and reporting it for the caller to clean up (ADR-0033: "a FIFO
/// eviction past the 100-item cap deletes the evicted item's spooled file in the same step"; this
/// round's finding 3 additionally requires the caller to emit `NotificationClosed(evicted_id,
/// reason=Evicted)`, so the evicted id travels alongside its icon path here).
fn push_new(queue: &mut VecDeque<Notification>, notification: Notification) -> Option<QueueCleanup> {
    queue.push_back(notification);
    if queue.len() > NOTIFICATION_QUEUE_CAP {
        queue.pop_front().map(|evicted| QueueCleanup::Evicted { id: evicted.id, icon_path: evicted.icon_path })
    } else {
        None
    }
}

/// A `replaces_id` update with no fresh image resolves to `(None, Some(old_path))` -- clears
/// `icon_path` and marks the old file for deletion, "rather than leaving a stale image attached to
/// new text" (ADR-0033). A fresh image that differs from the old path also marks the old file for
/// deletion; the same path reused (rare, but not impossible) deletes nothing.
fn resolve_replacement_icon(previous_icon_path: Option<String>, fresh_icon_path: Option<String>) -> (Option<String>, Option<String>) {
    match (&previous_icon_path, &fresh_icon_path) {
        (Some(old), Some(new)) if old != new => (fresh_icon_path, previous_icon_path),
        (Some(_), Some(_)) => (fresh_icon_path, None),
        (Some(_), None) => (None, previous_icon_path),
        (None, _) => (fresh_icon_path, None),
    }
}

/// Inserts `notification`: replaces the matching queued entry in place (same position -- a replace
/// is not treated as a fresh arrival for ordering) if `notification.id` is still present, otherwise
/// falls back to [`push_new`] (the id was already dismissed/evicted, but `Notify`'s own id-reuse
/// contract still applies regardless -- see [`resolve_notification_id`]). Returns what the caller
/// needs to clean up ([`QueueCleanup`]) -- either the replaced entry's own old icon
/// ([`resolve_replacement_icon`], never a signal) or, on the fallback path, whatever [`push_new`]
/// itself evicted (icon deletion *and* a `NotificationClosed` signal).
fn replace_or_push(queue: &mut VecDeque<Notification>, mut notification: Notification) -> Option<QueueCleanup> {
    let target_id = notification.id;
    if let Some(existing) = queue.iter_mut().find(|entry| entry.id == target_id) {
        let previous_icon = existing.icon_path.clone();
        let (resolved_icon, to_delete) = resolve_replacement_icon(previous_icon, notification.icon_path.take());
        notification.icon_path = resolved_icon;
        *existing = notification;
        to_delete.map(QueueCleanup::ReplacedIcon)
    } else {
        push_new(queue, notification)
    }
}

/// The pure decision half of [`NotificationsController::expire_if_still_present`] (finding 2):
/// the index of the queue entry that a timer captured for `(id, incarnation)` should still expire,
/// or `None` if there isn't one -- either no entry with that id exists at all, or one does but its
/// `incarnation` has moved on, meaning a newer `Notify` call (a `replaces_id` update) already
/// superseded this timer. Both cases collapse to the same "this timer does nothing" outcome; the
/// newer timer that replace spawned handles expiry correctly on its own schedule instead.
fn find_expiring_entry(queue: &VecDeque<Notification>, id: u32, incarnation: u64) -> Option<usize> {
    queue.iter().position(|entry| entry.id == id && entry.incarnation == incarnation)
}

/// `dismiss(id)`/`reply(id, ...)`/`CloseNotification(id)`'s shared removal: pulls the matching
/// entry out of the queue entirely (not just clearing a field), for the caller to delete its icon
/// file (if any) and emit the appropriate `NotificationClosed`/`ActionInvoked` signal.
fn remove_by_id(queue: &mut VecDeque<Notification>, id: u32) -> Option<Notification> {
    let index = queue.iter().position(|entry| entry.id == id)?;
    queue.remove(index)
}

/// `notifications.feed`'s truncated view (ADR-0033): the newest [`NOTIFICATION_FEED_VIEW`] entries,
/// most recent first.
fn feed_view(queue: &VecDeque<Notification>) -> Vec<Notification> {
    queue.iter().rev().take(NOTIFICATION_FEED_VIEW).cloned().collect()
}

// -------------------------------------------------------------------------------------------
// Image hint decoding: the real freedesktop `image-data`/`icon_data` struct shape
// `(iiibiiay)` -- width, height, rowstride, has_alpha, bits_per_sample, channels, data -- decoded
// by hand from the already-unwrapped `zvariant::Value`, same technique `dbus::tray::parse_menu_node`
// already uses for a `Value::Structure`. Not tray's `IconPixmap` shape (square-only ARGB32); this
// is a different, real struct with its own field layout.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct RawImageData {
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

fn value_as_bool(value: &Value<'_>) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn value_as_u8(value: &Value<'_>) -> Option<u8> {
    match value {
        Value::U8(b) => Some(*b),
        _ => None,
    }
}

fn value_as_str<'a>(value: &'a Value<'_>) -> Option<&'a str> {
    match value {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn value_as_bytes(value: &Value<'_>) -> Option<Vec<u8>> {
    match value {
        Value::Array(array) => Some(array.iter().filter_map(|v| if let Value::U8(b) = v { Some(*b) } else { None }).collect()),
        _ => None,
    }
}

/// Decodes an `image-data`/`icon_data` hint's `(iiibiiay)` structure. `None` for anything that
/// isn't a 7-field structure with exactly this field-type layout -- a malformed hint degrades to
/// "no image from this source", never a panic.
fn decode_raw_image_data(value: &Value<'_>) -> Option<RawImageData> {
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
fn image_data_is_valid(image: &RawImageData) -> bool {
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
fn encode_image_data_to_png(image: &RawImageData) -> Result<Vec<u8>, PngEncodeError> {
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

fn write_icon_png(id: u32, png_bytes: &[u8]) -> std::io::Result<String> {
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
fn delete_icon_file(path: &str) {
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
enum IconInput {
    ImageData(RawImageData),
    ImagePath(String),
    AppIcon(String),
    IconData(RawImageData),
    None,
}

fn resolve_icon_input(image_data: Option<RawImageData>, image_path: Option<String>, app_icon: Option<String>, icon_data: Option<RawImageData>) -> IconInput {
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

// -------------------------------------------------------------------------------------------
// Sound playback: WAV decode (testable) + a dedicated one-shot PipeWire playback thread
// (live-test-only, no unit tests of its own -- see the module doc comment and this section's own
// doc comments for why).
// -------------------------------------------------------------------------------------------

pub type SoundSender = std::sync::mpsc::Sender<PathBuf>;

#[derive(Debug, Clone, PartialEq)]
struct DecodedWav {
    channels: u32,
    sample_rate: u32,
    samples: Vec<i16>,
}

#[derive(Debug)]
enum SoundDecodeError {
    Wav(hound::Error),
    UnsupportedFormat { format: hound::SampleFormat, bits: u16 },
}

impl std::fmt::Display for SoundDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wav(err) => write!(f, "{err}"),
            Self::UnsupportedFormat { format, bits } => write!(f, "unsupported WAV sample format {format:?} at {bits} bits per sample"),
        }
    }
}

impl std::error::Error for SoundDecodeError {}

impl From<hound::Error> for SoundDecodeError {
    fn from(err: hound::Error) -> Self {
        Self::Wav(err)
    }
}

/// ponytail: WAV only, and only 16-bit integer or 32-bit float samples -- `hound` is a small,
/// pure-Rust WAV decoder with no transitive bloat, matching what every reference notification
/// daemon checked actually needs (short, simple UI sounds). A general audio-decode dependency
/// (MP3/OGG/FLAC) isn't justified until a real Lua config registers something else; other bit
/// depths fail cleanly via [`SoundDecodeError::UnsupportedFormat`] rather than silently
/// mis-decoding.
fn decode_wav_samples(path: &Path) -> Result<DecodedWav, SoundDecodeError> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let samples: Vec<i16> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => reader.samples::<i16>().collect::<Result<_, _>>()?,
        (hound::SampleFormat::Float, 32) => {
            reader.samples::<f32>().map(|sample| sample.map(|value| (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)).collect::<Result<_, _>>()?
        }
        (format, bits) => return Err(SoundDecodeError::UnsupportedFormat { format, bits }),
    };
    Ok(DecodedWav { channels: u32::from(spec.channels), sample_rate: spec.sample_rate, samples })
}

/// Runs until `requests` closes, decoding and playing each requested sound file's WAV
/// ([`decode_wav_samples`]) through a fresh, one-shot PipeWire playback stream per request
/// (ADR-0033: "a small one-shot player... no cancellation-handle system, no Lua/wire round-trip
/// for the trigger itself"). Blocks the calling thread -- call from a dedicated
/// `std::thread::spawn`, same "pipewire-rs's loop is `!Send`" reasoning `audio::mixer::run`
/// already established (a different pipewire-rs API surface, though: `pw::stream::Stream` writing
/// audio out, not `pw::registry` listening for nodes).
///
/// ponytail: real PipeWire stream I/O against a real audio device is fundamentally live-test-only,
/// same category as idle's raw Wayland dispatch (ADR-0032) -- this function and
/// [`play_one_wav`] have no unit tests of their own. [`decode_wav_samples`] (the WAV-decode
/// boundary) and [`should_play_sound`] (the DND/urgency gate deciding whether this ever gets
/// triggered) are the tested seams either side of it.
pub fn run_sound_player(requests: std::sync::mpsc::Receiver<PathBuf>) {
    while let Ok(path) = requests.recv() {
        if let Err(err) = play_one_wav(&path) {
            eprintln!("notifications: failed to play sound {path:?}: {err}");
        }
    }
}

struct PlaybackState {
    samples: Vec<i16>,
    position: usize,
    channels: u32,
    main_loop: pw::main_loop::MainLoopRc,
}

const CHAN_SIZE: usize = std::mem::size_of::<i16>();

fn play_one_wav(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let decoded = decode_wav_samples(path)?;
    if decoded.samples.is_empty() {
        return Ok(());
    }

    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "oblisk-notification-sound",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Notification",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::AUDIO_CHANNELS => decoded.channels.to_string(),
        },
    )?;

    let playback = PlaybackState { samples: decoded.samples, position: 0, channels: decoded.channels, main_loop: main_loop.clone() };

    let _listener = stream
        .add_local_listener_with_user_data(playback)
        .process(|stream, state| match stream.dequeue_buffer() {
            None => {}
            Some(mut buffer) => {
                let datas = buffer.datas_mut();
                let stride = CHAN_SIZE * state.channels.max(1) as usize;
                let data = &mut datas[0];
                let n_frames = if let Some(slice) = data.data() {
                    let remaining_frames = state.samples.len().saturating_sub(state.position) / state.channels.max(1) as usize;
                    let capacity_frames = slice.len() / stride;
                    let n_frames = remaining_frames.min(capacity_frames);
                    for i in 0..n_frames {
                        for c in 0..state.channels as usize {
                            let sample = state.samples[state.position + i * state.channels as usize + c];
                            let start = i * stride + c * CHAN_SIZE;
                            let end = start + CHAN_SIZE;
                            slice[start..end].copy_from_slice(&i16::to_le_bytes(sample));
                        }
                    }
                    state.position += n_frames * state.channels as usize;
                    n_frames
                } else {
                    0
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
                if state.position >= state.samples.len() {
                    state.main_loop.quit();
                }
            }
        })
        .register()?;

    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(decoded.sample_rate);
    audio_info.set_channels(decoded.channels);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    for (i, slot) in position.iter_mut().take(decoded.channels as usize).enumerate() {
        *slot = match i {
            0 => pw::spa::sys::SPA_AUDIO_CHANNEL_FL,
            1 => pw::spa::sys::SPA_AUDIO_CHANNEL_FR,
            _ => pw::spa::sys::SPA_AUDIO_CHANNEL_UNKNOWN,
        };
    }
    audio_info.set_position(position);

    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object { type_: pw::spa::sys::SPA_TYPE_OBJECT_Format, id: pw::spa::sys::SPA_PARAM_EnumFormat, properties: audio_info.into() }),
    )?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values).ok_or("failed to build the audio format pod")?];

    stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    main_loop.run();
    Ok(())
}

// -------------------------------------------------------------------------------------------
// D-Bus interface + controller. `NotificationsController` is both the exported
// `org.freedesktop.Notifications` object (its `#[zbus::interface]` methods below) and the cheap-
// clone handle `main.rs` holds for write-command dispatch -- unlike `dbus::tray`'s split
// (`StatusNotifierWatcher` vs. `TrayController`), one type serves both roles here since both need
// the same queue/DND/sound state and the same signal-emitting capability.
// -------------------------------------------------------------------------------------------

struct NotificationsQueueState {
    queue: VecDeque<Notification>,
    next_id: u32,
    next_incarnation: u64,
    dnd: bool,
    sound_registry: HashMap<Urgency, PathBuf>,
}

impl NotificationsQueueState {
    fn new() -> Self {
        Self { queue: VecDeque::new(), next_id: 1, next_incarnation: 1, dnd: false, sound_registry: HashMap::new() }
    }
}

/// `NotificationClosed`'s `reason` argument (docs/oblisk-supervisor-services-dbus.md §1's base
/// spec, plus ADR-0033's repurposing of the spec's undefined/reserved `4` for Oblisk's own
/// FIFO-eviction cap -- not a base-spec concept). Replaces the raw `u32` literals every call site
/// used to pass, matching this file's own enum-heavy style elsewhere (`Urgency`, `ExpiryPolicy`,
/// `IconInput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    ClosedByMethod = 3,
    Evicted = 4,
}

impl From<CloseReason> for u32 {
    fn from(reason: CloseReason) -> Self {
        reason as u32
    }
}

#[derive(Clone)]
pub struct NotificationsController {
    /// `Some` only when this process actually owns `org.freedesktop.Notifications` on a live
    /// session-bus connection -- `None` degrades every signal emission to a silent no-op (ADR-0033/
    /// `TrayController::inert`'s precedent), while queue/DND/sound state stays fully functional
    /// either way (they're pure Supervisor state, not dependent on the D-Bus server being live).
    connection: Option<zbus::Connection>,
    state: Arc<Mutex<NotificationsQueueState>>,
    events: UnboundedSender<NotificationsSignal>,
    sound_tx: SoundSender,
    trusted_roots: Arc<Vec<PathBuf>>,
}

impl NotificationsController {
    /// Requests `org.freedesktop.Notifications` with `DoNotQueue` set (unlike tray's dual-role
    /// dance): a real desktop might already have `mako`/`dunst` running and owning this name, and
    /// that's a genuine "someone else already provides this service" outcome to degrade to inert
    /// for, not a race to queue behind (ADR-0033: "don't panic, don't retry-loop").
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<NotificationsSignal>, sound_tx: SoundSender) -> Self {
        let state = Arc::new(Mutex::new(NotificationsQueueState::new()));
        let trusted_roots = Arc::new(default_trusted_icon_roots());

        let live_connection = match connection.request_name_with_flags(NOTIFICATIONS_BUS_NAME, RequestNameFlags::DoNotQueue.into()).await {
            Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => Some(connection.clone()),
            Ok(other) => {
                eprintln!(
                    "notifications: RequestName({NOTIFICATIONS_BUS_NAME}) -> {other:?}; another notification daemon already owns this name, \
                     disabling the D-Bus server for this run"
                );
                None
            }
            Err(err) => {
                eprintln!("notifications: RequestName({NOTIFICATIONS_BUS_NAME}) failed: {err}; disabling the D-Bus server for this run");
                None
            }
        };

        let controller = Self { connection: live_connection.clone(), state, events, sound_tx, trusted_roots };

        if let Some(live_connection) = &live_connection
            && let Err(err) = live_connection.object_server().at(NOTIFICATIONS_OBJECT_PATH, controller.clone()).await
        {
            eprintln!("notifications: failed to export org.freedesktop.Notifications at {NOTIFICATIONS_OBJECT_PATH}: {err}");
        }

        controller
    }

    /// Fully inert controller: no D-Bus connection, but a real, working queue/DND/sound-registry
    /// state -- used when the session bus itself couldn't be reached at all. Every write command
    /// still behaves sensibly (an empty queue for `dismiss`/`reply` to miss against, `set_dnd`/
    /// `set_sound` still mutate real in-memory state), it's only signal emission and `Notify`
    /// arriving over D-Bus that can never happen (`TrayController::inert`'s precedent).
    pub fn inert(events: UnboundedSender<NotificationsSignal>, sound_tx: SoundSender) -> Self {
        Self { connection: None, state: Arc::new(Mutex::new(NotificationsQueueState::new())), events, sound_tx, trusted_roots: Arc::new(default_trusted_icon_roots()) }
    }

    async fn emit_notification_closed(&self, id: u32, reason: CloseReason) {
        let Some(connection) = &self.connection else { return };
        match zbus::object_server::SignalEmitter::new(connection, NOTIFICATIONS_OBJECT_PATH) {
            Ok(emitter) => {
                let _ = Self::notification_closed(&emitter, id, reason.into()).await;
            }
            Err(err) => eprintln!("notifications: failed to build a signal emitter for NotificationClosed({id}, {reason:?}): {err}"),
        }
    }

    async fn emit_action_invoked(&self, id: u32, action_key: String) {
        let Some(connection) = &self.connection else { return };
        match zbus::object_server::SignalEmitter::new(connection, NOTIFICATIONS_OBJECT_PATH) {
            Ok(emitter) => {
                let _ = Self::action_invoked(&emitter, id, action_key).await;
            }
            Err(err) => eprintln!("notifications: failed to build a signal emitter for ActionInvoked({id}, {action_key:?}): {err}"),
        }
    }

    /// Resolves `Notify`'s icon precedence ([`resolve_icon_input`]) into a final, spooled/
    /// validated `icon_path`. `image-data`/`icon_data` get bounds-checked ([`image_data_is_valid`])
    /// and PNG-encoded/spooled to SHM; `image-path`/`app_icon` (after stripping a `file://` scheme,
    /// if present) run through the same [`validate_trusted_path`] boundary body-markup images use.
    async fn resolve_and_spool_icon(&self, id: u32, image_data: Option<RawImageData>, image_path: Option<String>, app_icon: Option<String>, icon_data: Option<RawImageData>) -> Option<String> {
        match resolve_icon_input(image_data, image_path, app_icon, icon_data) {
            IconInput::ImageData(raw) | IconInput::IconData(raw) => spool_raw_image(id, &raw),
            IconInput::ImagePath(path) | IconInput::AppIcon(path) => validate_trusted_path(strip_file_uri(&path), &self.trusted_roots).map(|p| p.to_string_lossy().into_owned()),
            IconInput::None => None,
        }
    }

    /// Fires once, at the end of the [`Duration`] a `notify()` call scheduled it for -- `id` and
    /// `incarnation` together identify exactly which `Notify` call's content this timer is for
    /// (finding 2). [`find_expiring_entry`] is the recheck: if a `replaces_id` update already
    /// landed new content at `id` since this timer was spawned, `incarnation` no longer matches
    /// and this is a silent no-op -- the newer timer that replace itself spawned will expire the
    /// replacement correctly, on its own schedule.
    async fn expire_if_still_present(&self, id: u32, incarnation: u64) {
        let icon_to_delete = {
            let mut state = self.state.lock().unwrap();
            let index = match find_expiring_entry(&state.queue, id, incarnation) {
                Some(index) => index,
                None => return,
            };
            state.queue.remove(index).and_then(|n| n.icon_path)
        };
        if let Some(path) = icon_to_delete {
            delete_icon_file(&path);
        }
        self.emit_notification_closed(id, CloseReason::Expired).await;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:dismiss(id)` (docs/oblisk-idl-api-specs.md §3.2): removes the entry, cancels
    /// nothing explicitly (the pending expiry task's own recheck-before-acting sees it's gone and
    /// no-ops -- ADR-0033/this module's own "no cancellation-handle system" choice), emits
    /// `NotificationClosed(id, reason=Dismissed)`, and signals a fresh `StateSnapshot` push. A
    /// silent, logged no-op for an unknown id.
    pub async fn dismiss(&self, id: u32) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            remove_by_id(&mut state.queue, id)
        };
        let Some(removed) = removed else {
            eprintln!("notifications: dismiss({id}) ignored: no notification with that id is currently queued");
            return;
        };
        if let Some(path) = removed.icon_path {
            delete_icon_file(&path);
        }
        self.emit_notification_closed(id, CloseReason::Dismissed).await;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:reply(id, text)` (ADR-0033): confirms the notification `has_reply`, emits
    /// `ActionInvoked(id, format_reply_action_key(text))`, and removes it from the queue -- "a
    /// replied-to notification is done". Logged no-ops for an unknown id or one that doesn't accept
    /// a reply.
    pub async fn reply(&self, id: u32, text: String) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            match state.queue.iter().position(|n| n.id == id) {
                Some(index) if state.queue[index].has_reply => remove_by_id(&mut state.queue, id),
                Some(_) => {
                    eprintln!("notifications: reply({id}, ...) ignored: that notification does not accept an inline reply");
                    None
                }
                None => {
                    eprintln!("notifications: reply({id}, ...) ignored: no notification with that id is currently queued");
                    None
                }
            }
        };
        let Some(removed) = removed else { return };
        if let Some(path) = removed.icon_path {
            delete_icon_file(&path);
        }
        self.emit_action_invoked(id, format_reply_action_key(&text)).await;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:set_sound(urgency, path)` (ADR-0033): registers `path` for `urgency`'s tier
    /// if it passes the same path-trust validator as icon references. An invalid/untrusted path is
    /// a logged no-op, not a panic.
    pub fn set_sound(&self, urgency: Urgency, path: &str) {
        match validate_trusted_path(path, &self.trusted_roots) {
            Some(validated) => {
                self.state.lock().unwrap().sound_registry.insert(urgency, validated);
            }
            None => eprintln!("notifications: set_sound({urgency:?}, {path:?}) ignored: not a trusted, existing sound file path"),
        }
    }

    /// `notifications:set_dnd(enabled)` (ADR-0033): flips the Supervisor-global toggle and signals
    /// a fresh `StateSnapshot` push (`notifications.dnd`). Gates sound only -- `notifications.feed`
    /// keeps receiving everything regardless.
    pub fn set_dnd(&self, enabled: bool) {
        self.state.lock().unwrap().dnd = enabled;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// Full re-derivation of `notifications.feed`/`notifications.dnd` from current state --
    /// synchronous, no D-Bus round trip needed (mirrors `dbus::tray::TrayController::build_state`).
    pub fn build_state(&self) -> NotificationsState {
        let state = self.state.lock().unwrap();
        NotificationsState { feed: feed_view(&state.queue), dnd: state.dnd }
    }
}

fn spool_raw_image(id: u32, raw: &RawImageData) -> Option<String> {
    if !image_data_is_valid(raw) {
        eprintln!("notifications: rejected an image-data/icon_data hint for notification {id}: fails bounds checks");
        return None;
    }
    match encode_image_data_to_png(raw) {
        Ok(png_bytes) => match write_icon_png(id, &png_bytes) {
            Ok(path) => Some(path),
            Err(err) => {
                eprintln!("notifications: failed to spool icon PNG for notification {id}: {err}");
                None
            }
        },
        Err(err) => {
            eprintln!("notifications: failed to encode image-data for notification {id}: {err}");
            None
        }
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationsController {
    #[zbus(name = "Notify")]
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        let app_name = truncate_utf8_bytes(&app_name, MAX_APP_NAME_BYTES);
        let summary = truncate_utf8_bytes(&summary, MAX_SUMMARY_BYTES);
        let body_spans = sanitize_body(&body, &self.trusted_roots);
        let urgency = urgency_from_hint_byte(hints.get("urgency").and_then(value_as_u8));
        let has_reply = actions_have_reply(&actions);

        let image_data = hints.get("image-data").or_else(|| hints.get("image_data")).and_then(decode_raw_image_data);
        let image_path = hints.get("image-path").or_else(|| hints.get("image_path")).and_then(value_as_str).map(str::to_string);
        let app_icon = (!app_icon.is_empty()).then_some(app_icon);
        let icon_data = hints.get("icon_data").and_then(decode_raw_image_data);
        let suppress_sound = hints.get("suppress-sound").and_then(value_as_bool).unwrap_or(false);
        let sound_file = hints.get("sound-file").and_then(value_as_str).map(str::to_string);

        let (id, incarnation) = {
            let mut state = self.state.lock().unwrap();
            let id = resolve_notification_id(replaces_id, &mut state.next_id);
            let incarnation = next_incarnation(&mut state.next_incarnation);
            (id, incarnation)
        };
        let icon_path = self.resolve_and_spool_icon(id, image_data, image_path, app_icon, icon_data).await;

        let notification = Notification { id, app_name, summary, body: body_spans, icon_path, urgency, has_reply, incarnation };

        let cleanup = {
            let mut state = self.state.lock().unwrap();
            replace_or_push(&mut state.queue, notification)
        };
        match cleanup {
            Some(QueueCleanup::ReplacedIcon(path)) => delete_icon_file(&path),
            Some(QueueCleanup::Evicted { id: evicted_id, icon_path }) => {
                if let Some(path) = icon_path {
                    delete_icon_file(&path);
                }
                // finding 3: a FIFO eviction past NOTIFICATION_QUEUE_CAP is a real close, not just
                // an icon-file cleanup -- the evicted id is gone from the queue for good.
                self.emit_notification_closed(evicted_id, CloseReason::Evicted).await;
            }
            None => {}
        }
        let _ = self.events.send(NotificationsSignal::Changed);

        if let ExpiryPolicy::After(duration) = resolve_expiry(urgency, expire_timeout) {
            let controller = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(duration).await;
                controller.expire_if_still_present(id, incarnation).await;
            });
        }

        let (dnd, tier_default_sound) = {
            let state = self.state.lock().unwrap();
            (state.dnd, state.sound_registry.get(&urgency).cloned())
        };
        let client_sound_file = sound_file.and_then(|path| validate_trusted_path(strip_file_uri(&path), &self.trusted_roots));
        let sound_path = resolve_sound_path(suppress_sound, client_sound_file, tier_default_sound);
        if should_play_sound(dnd, urgency, sound_path.is_some())
            && let Some(sound_path) = sound_path
        {
            let _ = self.sound_tx.send(sound_path);
        }

        id
    }

    /// `CloseNotification(id)` (docs/oblisk-supervisor-services-dbus.md §1): removes the entry (if
    /// present) and emits `NotificationClosed(id, reason=ClosedByMethod)`. A `dismiss()` write
    /// command emits the same signal with `reason=Dismissed` instead -- the two are distinct wire
    /// callers of the same removal primitive ([`remove_by_id`]), so this is not just
    /// [`NotificationsController::dismiss`] under another name.
    #[zbus(name = "CloseNotification")]
    async fn close_notification(&self, id: u32) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            remove_by_id(&mut state.queue, id)
        };
        if let Some(removed) = removed {
            if let Some(path) = removed.icon_path {
                delete_icon_file(&path);
            }
            self.emit_notification_closed(id, CloseReason::ClosedByMethod).await;
            let _ = self.events.send(NotificationsSignal::Changed);
        }
    }

    #[zbus(name = "GetCapabilities")]
    async fn get_capabilities(&self) -> Vec<String> {
        NOTIFICATIONS_CAPABILITIES.iter().map(|s| s.to_string()).collect()
    }

    #[zbus(name = "GetServerInformation")]
    async fn get_server_information(&self) -> (String, String, String, String) {
        ("oblisk".to_string(), "oblisk".to_string(), "0.1.0".to_string(), "1.2".to_string())
    }

    /// `reason` is [`CloseReason`] as its raw wire `u32` (the signal's own D-Bus signature is fixed
    /// by the base spec, so the enum can't appear here directly): 1 = expired, 2 = dismissed via
    /// `dismiss()`, 3 = `CloseNotification`, 4 = FIFO eviction -- a real desktop-notification-spec
    /// "undefined/reserved" value repurposed for a case the base spec never anticipated, since
    /// Oblisk's hard 100-cap is not a base-spec concept (ADR-0033). All four are emitted by this
    /// controller: `notify()`'s own eviction path emits `reason=4` when [`push_new`]/
    /// [`replace_or_push`] report a [`QueueCleanup::Evicted`].
    #[zbus(signal, name = "NotificationClosed")]
    async fn notification_closed(signal_emitter: &zbus::object_server::SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal, name = "ActionInvoked")]
    async fn action_invoked(signal_emitter: &zbus::object_server::SignalEmitter<'_>, id: u32, action_key: String) -> zbus::Result<()>;
}

// -------------------------------------------------------------------------------------------
// Write-command argument parsers (docs/oblisk-idl-api-specs.md §3.2, ADR-0033's added rows).
// `set_dnd` reuses `dbus::parse_bool_arg` directly at the call site (tray/idle/network/bluetooth's
// established convention) rather than a redundant wrapper here.
// -------------------------------------------------------------------------------------------

/// `notifications:dismiss(id)`'s `arguments: [id]`.
pub fn parse_dismiss_args(arguments: &[serde_json::Value]) -> Option<u32> {
    arguments.first()?.as_u64().and_then(|v| u32::try_from(v).ok())
}

/// `notifications:reply(id, text)`'s `arguments: [id, text]`.
pub fn parse_reply_args(arguments: &[serde_json::Value]) -> Option<(u32, String)> {
    let id = u32::try_from(arguments.first()?.as_u64()?).ok()?;
    let text = arguments.get(1)?.as_str()?.to_string();
    Some((id, text))
}

/// `notifications:set_sound(urgency, path)`'s `arguments: [urgency, path]`.
pub fn parse_set_sound_args(arguments: &[serde_json::Value]) -> Option<(Urgency, String)> {
    let urgency = parse_urgency_str(arguments.first()?.as_str()?)?;
    let path = arguments.get(1)?.as_str()?.to_string();
    Some((urgency, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str, bold: bool, italic: bool, underline: bool, href: Option<&str>) -> NotificationSpan {
        NotificationSpan::Text { text: text.to_string(), bold, italic, underline, href: href.map(str::to_string) }
    }

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

    // ---- parse_markup (TDD seam 2) ----

    #[test]
    fn parse_markup_plain_text_is_a_single_unstyled_span() {
        assert_eq!(parse_markup("hello world"), vec![text("hello world", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_empty_input_is_empty() {
        assert_eq!(parse_markup(""), Vec::new());
    }

    #[test]
    fn parse_markup_handles_a_single_allowed_tag() {
        assert_eq!(parse_markup("<b>bold</b>"), vec![text("bold", true, false, false, None)]);
        assert_eq!(parse_markup("<i>italic</i>"), vec![text("italic", false, true, false, None)]);
        assert_eq!(parse_markup("<u>underline</u>"), vec![text("underline", false, false, true, None)]);
    }

    #[test]
    fn parse_markup_nests_allowed_tags() {
        assert_eq!(parse_markup("<b><i>text</i></b>"), vec![text("text", true, true, false, None)]);
    }

    #[test]
    fn parse_markup_handles_an_unclosed_tag_by_applying_style_to_end_of_input() {
        assert_eq!(parse_markup("<b>bold text"), vec![text("bold text", true, false, false, None)]);
    }

    #[test]
    fn parse_markup_strips_a_script_tag_mixed_with_legal_markup() {
        let spans = parse_markup(r#"<b>bold</b><script>alert(1)</script>more text"#);
        assert_eq!(spans, vec![text("bold", true, false, false, None), text("more text", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_strips_a_style_tag_and_its_content() {
        let spans = parse_markup("before<style>.x{color:red}</style>after");
        assert_eq!(spans, vec![text("beforeafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_drops_an_img_with_no_src() {
        let spans = parse_markup(r#"before<img alt="no src">after"#);
        assert_eq!(spans, vec![text("beforeafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_emits_an_image_span_for_a_self_closing_img() {
        let spans = parse_markup(r#"<img src="/usr/share/icons/x.png" alt="x"/>"#);
        assert_eq!(spans, vec![NotificationSpan::Image { image_path: "/usr/share/icons/x.png".to_string() }]);
    }

    #[test]
    fn parse_markup_emits_an_image_span_for_a_non_self_closing_img() {
        let spans = parse_markup(r#"<img src="/usr/share/icons/x.png">"#);
        assert_eq!(spans, vec![NotificationSpan::Image { image_path: "/usr/share/icons/x.png".to_string() }]);
    }

    #[test]
    fn parse_markup_handles_an_anchor_with_href() {
        let spans = parse_markup(r#"<a href="https://example.com">link</a>"#);
        assert_eq!(spans, vec![text("link", false, false, false, Some("https://example.com"))]);
    }

    #[test]
    fn parse_markup_drops_an_anchor_with_no_href() {
        let spans = parse_markup("before<a>no href</a>after");
        assert_eq!(spans, vec![text("beforeno hrefafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_strips_unrecognized_tags_but_keeps_their_surrounding_text() {
        let spans = parse_markup("before<div>middle</div>after");
        assert_eq!(spans, vec![text("beforemiddleafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_mixes_multiple_constructs() {
        let spans = parse_markup(r#"plain <b>bold</b> and <a href="url">link</a>"#);
        assert_eq!(
            spans,
            vec![
                text("plain ", false, false, false, None),
                text("bold", true, false, false, None),
                text(" and ", false, false, false, None),
                text("link", false, false, false, Some("url")),
            ]
        );
    }

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
        let escape_path = trusted_dir.path().join("..").join(outside_dir.path().file_name().unwrap()).join("secret.png");
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

    // ---- format_reply_action_key (TDD seam 4) ----

    #[test]
    fn format_reply_action_key_embeds_the_text() {
        assert_eq!(format_reply_action_key("sounds good"), "inline-reply::sounds good");
    }

    #[test]
    fn actions_have_reply_detects_the_inline_reply_action_key() {
        assert!(actions_have_reply(&["inline-reply".to_string(), "Reply".to_string()]));
    }

    #[test]
    fn actions_have_reply_is_false_without_it() {
        assert!(!actions_have_reply(&["default".to_string(), "Open".to_string()]));
        assert!(!actions_have_reply(&[]));
    }

    // ---- resolve_expiry (TDD seam 5) ----

    #[test]
    fn resolve_expiry_critical_never_expires_regardless_of_timeout() {
        assert_eq!(resolve_expiry(Urgency::Critical, 1000), ExpiryPolicy::Never);
        assert_eq!(resolve_expiry(Urgency::Critical, 0), ExpiryPolicy::Never);
        assert_eq!(resolve_expiry(Urgency::Critical, -1), ExpiryPolicy::Never);
    }

    #[test]
    fn resolve_expiry_zero_means_never_for_non_critical() {
        assert_eq!(resolve_expiry(Urgency::Normal, 0), ExpiryPolicy::Never);
        assert_eq!(resolve_expiry(Urgency::Low, 0), ExpiryPolicy::Never);
    }

    #[test]
    fn resolve_expiry_negative_one_uses_the_server_default() {
        assert_eq!(resolve_expiry(Urgency::Normal, -1), ExpiryPolicy::After(Duration::from_millis(DEFAULT_EXPIRE_MS)));
    }

    #[test]
    fn resolve_expiry_positive_uses_that_many_milliseconds() {
        assert_eq!(resolve_expiry(Urgency::Normal, 2500), ExpiryPolicy::After(Duration::from_millis(2500)));
        assert_eq!(resolve_expiry(Urgency::Low, 100), ExpiryPolicy::After(Duration::from_millis(100)));
    }

    // ---- should_play_sound (TDD seam 6) ----

    #[test]
    fn should_play_sound_is_false_when_nothing_is_registered() {
        assert!(!should_play_sound(false, Urgency::Normal, false));
        assert!(!should_play_sound(false, Urgency::Critical, false));
    }

    #[test]
    fn should_play_sound_is_false_under_dnd_for_non_critical() {
        assert!(!should_play_sound(true, Urgency::Low, true));
        assert!(!should_play_sound(true, Urgency::Normal, true));
    }

    #[test]
    fn should_play_sound_critical_bypasses_dnd() {
        assert!(should_play_sound(true, Urgency::Critical, true));
    }

    #[test]
    fn should_play_sound_is_true_outside_dnd_with_a_registered_sound() {
        assert!(should_play_sound(false, Urgency::Normal, true));
        assert!(should_play_sound(false, Urgency::Low, true));
    }

    // ---- resolve_sound_path (sound-file/suppress-sound hint precedence) ----

    #[test]
    fn resolve_sound_path_suppress_always_wins_to_none() {
        let client = Some(PathBuf::from("/usr/share/sounds/client.wav"));
        let tier = Some(PathBuf::from("/usr/share/sounds/tier.wav"));
        assert_eq!(resolve_sound_path(true, client.clone(), tier.clone()), None, "suppress-sound must force silence even with both a client file and a tier default present");
        assert_eq!(resolve_sound_path(true, client, None), None);
        assert_eq!(resolve_sound_path(true, None, tier), None);
        assert_eq!(resolve_sound_path(true, None, None), None);
    }

    #[test]
    fn resolve_sound_path_prefers_the_client_file_over_the_tier_default() {
        let client = Some(PathBuf::from("/usr/share/sounds/client.wav"));
        let tier = Some(PathBuf::from("/usr/share/sounds/tier.wav"));
        assert_eq!(resolve_sound_path(false, client.clone(), tier), client);
    }

    #[test]
    fn resolve_sound_path_falls_back_to_the_tier_default_without_a_client_file() {
        let tier = Some(PathBuf::from("/usr/share/sounds/tier.wav"));
        assert_eq!(resolve_sound_path(false, None, tier.clone()), tier);
    }

    #[test]
    fn resolve_sound_path_is_none_when_nothing_is_available() {
        assert_eq!(resolve_sound_path(false, None, None), None);
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

    // ---- resolve_notification_id ----

    #[test]
    fn resolve_notification_id_allocates_monotonically_when_replaces_id_is_zero() {
        let mut next_id = 1u32;
        assert_eq!(resolve_notification_id(0, &mut next_id), 1);
        assert_eq!(resolve_notification_id(0, &mut next_id), 2);
        assert_eq!(next_id, 3);
    }

    #[test]
    fn resolve_notification_id_passes_through_a_nonzero_replaces_id_without_bumping_the_allocator() {
        let mut next_id = 5u32;
        assert_eq!(resolve_notification_id(42, &mut next_id), 42);
        assert_eq!(next_id, 5, "the allocator must not move for a replaces_id reuse");
    }

    #[test]
    fn resolve_notification_id_wraps_safely_past_u32_max_skipping_zero() {
        let mut next_id = u32::MAX;
        assert_eq!(resolve_notification_id(0, &mut next_id), u32::MAX);
        assert_eq!(next_id, 1, "must wrap to 1, never 0 -- 0 is reserved to mean \"new\" on the wire");
    }

    // ---- queue mutation: push_new / replace_or_push / remove_by_id / feed_view (TDD seam 7 +
    //      FIFO eviction) ----

    fn sample_notification(id: u32, icon_path: Option<&str>) -> Notification {
        Notification {
            id,
            app_name: "app".to_string(),
            summary: "summary".to_string(),
            body: vec![text("body", false, false, false, None)],
            icon_path: icon_path.map(str::to_string),
            urgency: Urgency::Normal,
            has_reply: false,
            incarnation: 0,
        }
    }

    #[test]
    fn push_new_does_not_evict_under_the_cap() {
        let mut queue = VecDeque::new();
        let evicted = push_new(&mut queue, sample_notification(1, None));
        assert_eq!(evicted, None);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn push_new_evicts_the_oldest_entry_past_the_cap_and_reports_its_id_and_icon() {
        let mut queue = VecDeque::new();
        for i in 0..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        let evicted = push_new(&mut queue, sample_notification(9999, Some("/tmp/evicted.png")));
        // The 101st insert evicts the oldest (id 0, no icon) -- finding 3: the evicted *id* must
        // come back too (not just its icon path), so the caller can emit
        // NotificationClosed(0, reason=Evicted) even though there's no icon file to delete.
        assert_eq!(evicted, Some(QueueCleanup::Evicted { id: 0, icon_path: None }));
        assert_eq!(queue.len(), NOTIFICATION_QUEUE_CAP);
        assert_eq!(queue.front().unwrap().id, 1, "the oldest entry (id 0) must have been evicted");
    }

    #[test]
    fn push_new_returns_the_evicted_entrys_own_id_and_icon_path() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(0, Some("/tmp/oldest.png")));
        for i in 1..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        let evicted = push_new(&mut queue, sample_notification(9999, None));
        assert_eq!(evicted, Some(QueueCleanup::Evicted { id: 0, icon_path: Some("/tmp/oldest.png".to_string()) }));
    }

    #[test]
    fn resolve_replacement_icon_clears_and_deletes_the_old_icon_when_no_fresh_image_supplied() {
        let (resolved, to_delete) = resolve_replacement_icon(Some("/tmp/old.png".to_string()), None);
        assert_eq!(resolved, None);
        assert_eq!(to_delete, Some("/tmp/old.png".to_string()));
    }

    #[test]
    fn resolve_replacement_icon_deletes_the_old_icon_when_a_different_fresh_image_is_supplied() {
        let (resolved, to_delete) = resolve_replacement_icon(Some("/tmp/old.png".to_string()), Some("/tmp/new.png".to_string()));
        assert_eq!(resolved, Some("/tmp/new.png".to_string()));
        assert_eq!(to_delete, Some("/tmp/old.png".to_string()));
    }

    #[test]
    fn resolve_replacement_icon_deletes_nothing_when_the_same_path_is_reused() {
        let (resolved, to_delete) = resolve_replacement_icon(Some("/tmp/same.png".to_string()), Some("/tmp/same.png".to_string()));
        assert_eq!(resolved, Some("/tmp/same.png".to_string()));
        assert_eq!(to_delete, None);
    }

    #[test]
    fn resolve_replacement_icon_no_previous_icon_just_uses_the_fresh_one() {
        let (resolved, to_delete) = resolve_replacement_icon(None, Some("/tmp/new.png".to_string()));
        assert_eq!(resolved, Some("/tmp/new.png".to_string()));
        assert_eq!(to_delete, None);
    }

    #[test]
    fn replace_or_push_replaces_in_place_and_clears_the_old_icon() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, Some("/tmp/old.png")));
        push_new(&mut queue, sample_notification(2, None));

        let mut replacement = sample_notification(1, None);
        replacement.summary = "updated".to_string();
        let cleanup = replace_or_push(&mut queue, replacement);

        assert_eq!(cleanup, Some(QueueCleanup::ReplacedIcon("/tmp/old.png".to_string())), "a same-id replace must never report an Evicted cleanup");
        assert_eq!(queue.len(), 2, "a replace must not grow the queue");
        assert_eq!(queue[0].summary, "updated");
        assert_eq!(queue[0].icon_path, None);
        assert_eq!(queue[1].id, 2, "the replace must not reorder other entries");
    }

    #[test]
    fn replace_or_push_falls_back_to_a_fresh_push_when_the_id_is_not_present() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, None));

        let cleanup = replace_or_push(&mut queue, sample_notification(999, None));

        assert_eq!(cleanup, None);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.back().unwrap().id, 999);
    }

    #[test]
    fn replace_or_push_fallback_path_reports_a_real_eviction_when_the_queue_is_full() {
        let mut queue = VecDeque::new();
        for i in 0..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        // id 9999 isn't queued, so this falls back to push_new -- which, at the cap, evicts the
        // oldest entry (id 0) and must report it the same way push_new itself would.
        let cleanup = replace_or_push(&mut queue, sample_notification(9999, None));
        assert_eq!(cleanup, Some(QueueCleanup::Evicted { id: 0, icon_path: None }));
    }

    #[test]
    fn remove_by_id_removes_and_returns_the_matching_entry() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, None));
        push_new(&mut queue, sample_notification(2, Some("/tmp/two.png")));

        let removed = remove_by_id(&mut queue, 2);

        assert_eq!(removed.map(|n| n.id), Some(2));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().id, 1);
    }

    #[test]
    fn remove_by_id_is_none_for_an_unknown_id() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, None));
        assert_eq!(remove_by_id(&mut queue, 999), None);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn feed_view_returns_the_newest_entries_first() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, None));
        push_new(&mut queue, sample_notification(2, None));
        push_new(&mut queue, sample_notification(3, None));

        let view = feed_view(&queue);
        assert_eq!(view.iter().map(|n| n.id).collect::<Vec<_>>(), vec![3, 2, 1]);
    }

    #[test]
    fn feed_view_caps_at_the_view_size_even_with_a_larger_backing_queue() {
        let mut queue = VecDeque::new();
        for i in 0..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        let view = feed_view(&queue);
        assert_eq!(view.len(), NOTIFICATION_FEED_VIEW);
        assert_eq!(view.first().unwrap().id, NOTIFICATION_QUEUE_CAP as u32 - 1, "the newest entry must be first");
    }

    // ---- next_incarnation / find_expiring_entry (finding 2: stale-expiry-timer race) ----

    #[test]
    fn next_incarnation_is_monotonic() {
        let mut counter = 1u64;
        assert_eq!(next_incarnation(&mut counter), 1);
        assert_eq!(next_incarnation(&mut counter), 2);
        assert_eq!(next_incarnation(&mut counter), 3);
    }

    #[test]
    fn find_expiring_entry_finds_the_entry_matching_both_id_and_incarnation() {
        let mut queue = VecDeque::new();
        let mut notification = sample_notification(7, None);
        notification.incarnation = 5;
        queue.push_back(notification);

        assert_eq!(find_expiring_entry(&queue, 7, 5), Some(0));
    }

    #[test]
    fn find_expiring_entry_is_none_for_an_unknown_id() {
        let mut queue = VecDeque::new();
        let mut notification = sample_notification(7, None);
        notification.incarnation = 5;
        queue.push_back(notification);

        assert_eq!(find_expiring_entry(&queue, 999, 5), None);
    }

    /// The exact race finding 2 describes: id=7 created (incarnation 1, timer A captures
    /// incarnation 1); before timer A fires, `replaces_id=7` lands new content (incarnation 2,
    /// timer B captures incarnation 2). Timer A firing must find nothing to expire -- incarnation 1
    /// no longer describes what's queued at id 7, so it must not remove the replacement 29 seconds
    /// early. Timer B firing later, against the incarnation that's actually still there, must find
    /// and expire it normally.
    #[test]
    fn find_expiring_entry_ignores_a_stale_timer_superseded_by_a_replace() {
        let mut queue = VecDeque::new();
        let mut notification = sample_notification(7, None);
        notification.incarnation = 1;
        queue.push_back(notification);

        // The replace lands: same id, a fresh incarnation, in place (mirrors what notify() does
        // via replace_or_push -- the id stays queued, just its incarnation moves on).
        let mut replacement = sample_notification(7, None);
        replacement.incarnation = 2;
        queue[0] = replacement;

        // Timer A (captured incarnation 1 at spawn time) must find nothing: superseded.
        assert_eq!(find_expiring_entry(&queue, 7, 1), None, "a stale timer for the pre-replace incarnation must be a no-op");
        // Timer B (captured incarnation 2, the replacement's own) must find the real entry.
        assert_eq!(find_expiring_entry(&queue, 7, 2), Some(0), "the current timer for the post-replace incarnation must still fire normally");
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

    // ---- CloseReason (finding 5) ----

    #[test]
    fn close_reason_maps_to_the_documented_wire_values() {
        assert_eq!(u32::from(CloseReason::Expired), 1);
        assert_eq!(u32::from(CloseReason::Dismissed), 2);
        assert_eq!(u32::from(CloseReason::ClosedByMethod), 3);
        assert_eq!(u32::from(CloseReason::Evicted), 4);
    }

    // ---- image-data decoding + bounds checks ----

    fn valid_rgba_image(width: i32, height: i32) -> RawImageData {
        let channels = 4;
        let rowstride = width * channels;
        RawImageData { width, height, rowstride, has_alpha: true, bits_per_sample: 8, channels, data: vec![0u8; (rowstride * height) as usize] }
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
        let image = RawImageData { width: 1, height: 1, rowstride: 4, has_alpha: true, bits_per_sample: 8, channels: 4, data: vec![0x11, 0x22, 0x33, 0x44] };
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
        let value = structure_value(vec![Value::I32(2), Value::I32(1), Value::I32(8), Value::Bool(true), Value::I32(8), Value::I32(4), Value::Array(array)]);

        let decoded = decode_raw_image_data(&value).expect("must decode a well-formed image-data structure");
        assert_eq!(decoded, RawImageData { width: 2, height: 1, rowstride: 8, has_alpha: true, bits_per_sample: 8, channels: 4, data });
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
        let resolved = resolve_icon_input(Some(tiny_image()), Some("/path".to_string()), Some("app-icon".to_string()), Some(tiny_image()));
        assert_eq!(resolved, IconInput::ImageData(tiny_image()));
    }

    #[test]
    fn resolve_icon_input_prefers_image_path_over_app_icon_and_icon_data() {
        let resolved = resolve_icon_input(None, Some("/path".to_string()), Some("app-icon".to_string()), Some(tiny_image()));
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

    // ---- decode_wav_samples (the sound-playback testable boundary) ----

    fn write_test_wav(path: &Path, spec: hound::WavSpec, samples: &[i16]) {
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn decode_wav_samples_round_trips_16_bit_int_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sound.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 44100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let samples = [100i16, -100, 200, -200];
        write_test_wav(&path, spec, &samples);

        let decoded = decode_wav_samples(&path).expect("must decode a real 16-bit WAV file");
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.sample_rate, 44100);
        assert_eq!(decoded.samples, samples);
    }

    #[test]
    fn decode_wav_samples_decodes_32_bit_float_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sound.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 22050, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0.5f32).unwrap();
        writer.write_sample(-0.5f32).unwrap();
        writer.finalize().unwrap();

        let decoded = decode_wav_samples(&path).expect("must decode a real 32-bit float WAV file");
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples.len(), 2);
        assert!(decoded.samples[0] > 16000 && decoded.samples[0] < 17000, "0.5 should map close to i16::MAX/2, got {}", decoded.samples[0]);
    }

    #[test]
    fn decode_wav_samples_rejects_an_unsupported_bit_depth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sound.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 8000, bits_per_sample: 8, sample_format: hound::SampleFormat::Int };
        write_test_wav(&path, spec, &[]);

        assert!(decode_wav_samples(&path).is_err());
    }

    // ---- write-command argument parsers ----

    #[test]
    fn parse_dismiss_args_reads_the_id() {
        assert_eq!(parse_dismiss_args(&[serde_json::json!(42)]), Some(42));
    }

    #[test]
    fn parse_dismiss_args_rejects_a_malformed_shape() {
        assert_eq!(parse_dismiss_args(&[]), None);
        assert_eq!(parse_dismiss_args(&[serde_json::json!("42")]), None);
    }

    #[test]
    fn parse_reply_args_reads_id_and_text() {
        assert_eq!(parse_reply_args(&[serde_json::json!(7), serde_json::json!("sounds good")]), Some((7, "sounds good".to_string())));
    }

    #[test]
    fn parse_reply_args_rejects_a_malformed_shape() {
        assert_eq!(parse_reply_args(&[serde_json::json!(7)]), None, "missing text");
        assert_eq!(parse_reply_args(&[serde_json::json!("7"), serde_json::json!("text")]), None, "id is not a number");
    }

    #[test]
    fn parse_set_sound_args_reads_urgency_and_path() {
        assert_eq!(
            parse_set_sound_args(&[serde_json::json!("critical"), serde_json::json!("/usr/share/sounds/x.wav")]),
            Some((Urgency::Critical, "/usr/share/sounds/x.wav".to_string()))
        );
    }

    #[test]
    fn parse_set_sound_args_rejects_an_invalid_urgency_string() {
        assert_eq!(parse_set_sound_args(&[serde_json::json!("urgent"), serde_json::json!("/path")]), None);
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
