//! [`NotificationsController`]: the D-Bus interface + write-action dispatcher and state owner
//! (deliberately fused into one type -- see the module-level doc for why). Split from
//! `dbus::notifications` -- see `dbus/notifications/mod.rs` for the module-level doc.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::Value;

use super::icon::{
    IconInput, RawImageData, decode_raw_image_data, default_trusted_icon_roots, delete_icon_file, encode_image_data_to_png, image_data_is_valid,
    resolve_icon_input, sanitize_body, strip_file_uri, validate_trusted_path, value_as_bool, value_as_str, value_as_u8, write_icon_png,
};
use super::queue::{ExpiryPolicy, QueueCleanup, feed_view, find_expiring_entry, next_incarnation, remove_by_id, replace_or_push, resolve_expiry, resolve_notification_id, resolve_sound_path, should_play_sound};
use super::sound::SoundSender;
use super::{
    MAX_APP_NAME_BYTES, MAX_SUMMARY_BYTES, NOTIFICATIONS_BUS_NAME, NOTIFICATIONS_CAPABILITIES, NOTIFICATIONS_OBJECT_PATH, Notification, NotificationsSignal, NotificationsState,
    Urgency, truncate_utf8_bytes, urgency_from_hint_byte, parse_urgency_str,
};

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
/// used to pass, matching this capability's own enum-heavy style elsewhere (`Urgency`,
/// `ExpiryPolicy`, `IconInput`).
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
    /// no-ops -- ADR-0033/this capability's own "no cancellation-handle system" choice), emits
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

    // ---- CloseReason (finding 5) ----

    #[test]
    fn close_reason_maps_to_the_documented_wire_values() {
        assert_eq!(u32::from(CloseReason::Expired), 1);
        assert_eq!(u32::from(CloseReason::Dismissed), 2);
        assert_eq!(u32::from(CloseReason::ClosedByMethod), 3);
        assert_eq!(u32::from(CloseReason::Evicted), 4);
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

}
