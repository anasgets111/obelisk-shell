//! [`NotificationsController`], the D-Bus interface, write dispatcher, and state owner. Split from
//! `dbus::notifications`, see `dbus/notifications/mod.rs` for the module-level doc.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
// Use tokio's clock: deadlines move with `tokio::time::pause`, so countdowns test without sleeping.
use tokio::time::Instant;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::Value;

use super::icon::{
    ImageInput, RawImageData, decode_raw_image_data, default_trusted_icon_roots, delete_icon_file,
    encode_image_data_to_png, image_data_is_valid, resolve_app_icon, resolve_image_input, sanitize_body,
    split_image_path_hint, strip_file_uri, validate_trusted_path, value_as_bool, value_as_str, value_as_u8,
    write_icon_png,
};
use super::queue::{
    Expiry, QueueCleanup, expire_entry, feed_view, next_incarnation, remove_by_id, replace_or_push, resolve_expiry,
    resolve_notification_id, resolve_sound_path, should_play_sound,
};
use super::sound::{SoundSender, default_trusted_sound_roots, resolve_sound_name};
use super::{
    MAX_ACTION_LABEL_BYTES, MAX_ACTIONS, MAX_APP_NAME_BYTES, MAX_SUMMARY_BYTES, NOTIFICATIONS_BUS_NAME,
    NOTIFICATIONS_CAPABILITIES, NOTIFICATIONS_OBJECT_PATH, Notification, NotificationAction, NotificationsSignal,
    NotificationsState, Urgency, desktop_entry_from_hint, parse_urgency_str, reply_placeholder_from_hint,
    urgency_from_hint_byte,
};
use crate::capabilities::system::controller::epoch_seconds;
use crate::capabilities::truncate_utf8_bytes;

/// Results of one pass over `Notify`'s flat action array, returned together rather than as three
/// passes that each re-decide what a key means (ADR-0090).
#[derive(Debug, Default, PartialEq)]
pub(super) struct ParsedActions {
    pub actions: Vec<NotificationAction>,
    /// A `"default"` key was present; the notification is activatable.
    pub has_default: bool,
    /// An `"inline-reply"` key was present, per the `x-kde-reply` convention.
    pub has_reply: bool,
}

/// Splits `Notify` actions into drawable buttons and the two non-button keys (ADR-0090).
///
/// Odd length means a key has no label. The base spec disallows it, but the notification survives:
/// the key becomes its label.
///
/// With `action_icons`, keys may also be theme icon names. Keep an action only if it has a label or
/// drawable icon; `["", ""]` produces no unlabelled, unpredictable button.
pub(super) fn parse_actions(actions: &[String], action_icons: bool) -> ParsedActions {
    let mut parsed = ParsedActions::default();
    for pair in actions.chunks(2) {
        let Some(key) = pair.first().filter(|key| !key.is_empty()) else { continue };
        match key.as_str() {
            "default" => {
                parsed.has_default = true;
                continue;
            }
            "inline-reply" => {
                parsed.has_reply = true;
                continue;
            }
            _ => {}
        }
        if parsed.actions.len() >= MAX_ACTIONS {
            continue;
        }
        // Theme name, never path: `icon` also accepts absolute paths, so this blocks file access.
        let icon_name = (action_icons && !key.contains('/')).then(|| key.clone());
        let label = pair.get(1).map(String::as_str).unwrap_or_default().trim();
        let label = if label.is_empty() && icon_name.is_none() { key.as_str() } else { label };
        if label.is_empty() && icon_name.is_none() {
            continue;
        }
        parsed.actions.push(NotificationAction {
            key: key.clone(),
            label: truncate_utf8_bytes(label, MAX_ACTION_LABEL_BYTES),
            icon_name,
        });
    }
    parsed
}

struct NotificationsQueueState {
    queue: VecDeque<Notification>,
    next_id: u32,
    next_incarnation: u64,
    dnd: bool,
    quiet: bool,
    sound_registry: HashMap<Urgency, PathBuf>,
    muted_apps: HashSet<String>,
}

impl NotificationsQueueState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            next_id: 1,
            next_incarnation: 1,
            dnd: false,
            quiet: false,
            sound_registry: HashMap::new(),
            muted_apps: HashSet::new(),
        }
    }
}

/// `NotificationClosed.reason`: the freedesktop Notifications spec's values plus ADR-0033's
/// reserved `4` for FIFO eviction.
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

// `NotificationsController` is both the exported `org.freedesktop.Notifications` object and the
// cheap-clone handle `main.rs` holds for write dispatch. Unlike `dbus::tray`'s split, one type
// serves both roles here.
#[derive(Clone)]
pub struct NotificationsController {
    /// Live connection only when this process owns `org.freedesktop.Notifications`; `None` makes
    /// signals no-ops while queue/DND/sound state remains functional.
    connection: Option<zbus::Connection>,
    state: Arc<Mutex<NotificationsQueueState>>,
    events: UnboundedSender<NotificationsSignal>,
    sound_tx: SoundSender,
    trusted_roots: Arc<Vec<PathBuf>>,
    sound_roots: Arc<Vec<PathBuf>>,
    /// Deadline stopping all expiry countdowns, or `None` (ADR-0094). A `watch` wakes spawned
    /// countdowns when it changes; a queue field would only be seen on their next check.
    expiry_hold: watch::Sender<Option<Instant>>,
}

impl NotificationsController {
    /// Requests the bus name with `DoNotQueue`: an existing `mako`/`dunst` owner degrades this
    /// daemon to inert rather than queueing behind it (ADR-0033).
    pub async fn new(
        connection: zbus::Connection,
        events: UnboundedSender<NotificationsSignal>,
        sound_tx: SoundSender,
    ) -> Self {
        let state = Arc::new(Mutex::new(NotificationsQueueState::new()));
        let trusted_roots = Arc::new(default_trusted_icon_roots());

        let live_connection = match connection
            .request_name_with_flags(NOTIFICATIONS_BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await
        {
            Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => Some(connection.clone()),
            Ok(other) => {
                eprintln!(
                    "notifications: RequestName({NOTIFICATIONS_BUS_NAME}) -> {other:?}; another notification daemon already owns this name, \
                     disabling the D-Bus server for this run"
                );
                None
            }
            Err(err) => {
                eprintln!(
                    "notifications: RequestName({NOTIFICATIONS_BUS_NAME}) failed: {err}; disabling the D-Bus server for this run"
                );
                None
            }
        };

        let controller = Self {
            connection: live_connection.clone(),
            state,
            events,
            sound_tx,
            trusted_roots,
            sound_roots: Arc::new(default_trusted_sound_roots()),
            expiry_hold: watch::Sender::new(None),
        };

        if let Some(live_connection) = &live_connection
            && let Err(err) = live_connection.object_server().at(NOTIFICATIONS_OBJECT_PATH, controller.clone()).await
        {
            eprintln!(
                "notifications: failed to export org.freedesktop.Notifications at {NOTIFICATIONS_OBJECT_PATH}: {err}"
            );
        }

        controller
    }

    /// No D-Bus connection but working queue/DND/sound state, used when the session bus is absent.
    /// Writes still work; only signals and D-Bus `Notify` are unavailable.
    pub fn inert(events: UnboundedSender<NotificationsSignal>, sound_tx: SoundSender) -> Self {
        Self {
            connection: None,
            state: Arc::new(Mutex::new(NotificationsQueueState::new())),
            events,
            sound_tx,
            trusted_roots: Arc::new(default_trusted_icon_roots()),
            sound_roots: Arc::new(default_trusted_sound_roots()),
            expiry_hold: watch::Sender::new(None),
        }
    }

    async fn emit_notification_closed(&self, id: u32, reason: CloseReason) {
        let Some(connection) = &self.connection else { return };
        match zbus::object_server::SignalEmitter::new(connection, NOTIFICATIONS_OBJECT_PATH) {
            Ok(emitter) => {
                let _ = Self::notification_closed(&emitter, id, reason.into()).await;
            }
            Err(err) => eprintln!(
                "notifications: failed to build a signal emitter for NotificationClosed({id}, {reason:?}): {err}"
            ),
        }
    }

    async fn emit_action_invoked(&self, id: u32, action_key: String) {
        let Some(connection) = &self.connection else { return };
        match zbus::object_server::SignalEmitter::new(connection, NOTIFICATIONS_OBJECT_PATH) {
            Ok(emitter) => {
                let _ = Self::action_invoked(&emitter, id, action_key).await;
            }
            Err(err) => eprintln!(
                "notifications: failed to build a signal emitter for ActionInvoked({id}, {action_key:?}): {err}"
            ),
        }
    }

    /// The reply half of [`NotificationsController::emit_action_invoked`]. The text stays out of
    /// the log: it is the user's message to someone, and the id is enough to place a failure.
    async fn emit_notification_replied(&self, id: u32, text: String) {
        let Some(connection) = &self.connection else { return };
        match zbus::object_server::SignalEmitter::new(connection, NOTIFICATIONS_OBJECT_PATH) {
            Ok(emitter) => {
                let _ = Self::notification_replied(&emitter, id, text).await;
            }
            Err(err) => {
                eprintln!("notifications: failed to build a signal emitter for NotificationReplied({id}): {err}")
            }
        }
    }

    /// Resolves `Notify`'s attached-picture precedence ([`resolve_image_input`]) into a final path.
    /// Raw image data is checked, PNG encoded, and spooled to SHM; `image-path` uses the body-image
    /// trust boundary. The positional application icon is separate and handled by
    /// [`resolve_app_icon`] (ADR-0091).
    async fn resolve_and_spool_image(
        &self,
        id: u32,
        image_data: Option<RawImageData>,
        image_path: Option<String>,
        icon_data: Option<RawImageData>,
    ) -> Option<String> {
        match resolve_image_input(image_data, image_path, icon_data) {
            ImageInput::ImageData(raw) | ImageInput::IconData(raw) => spool_raw_image(id, &raw),
            ImageInput::ImagePath(path) => validate_trusted_path(strip_file_uri(&path), &self.trusted_roots)
                .map(|p| p.to_string_lossy().into_owned()),
            ImageInput::None => None,
        }
    }

    /// Fires after a scheduled duration. `id` plus `incarnation` identifies the `Notify` content;
    /// [`expire_entry`] silently rejects stale replacement timers. Ordinary expiry retires the
    /// entry for history (ADR-0100), emits `NotificationClosed(..., reason=1)`, and leaves
    /// `dismiss` to remove it. The sender gets the same close signal as before, no reliable
    /// `ActionInvoked`, and should not update the entry in place; `transient` entries are removed.
    async fn expire(&self, id: u32, incarnation: u64) {
        let outcome = {
            let mut state = self.state.lock().unwrap();
            expire_entry(&mut state.queue, id, incarnation)
        };
        let Some(outcome) = outcome else { return };
        if let Expiry::Removed { image_path: Some(path) } = outcome {
            delete_icon_file(&path);
        }
        self.emit_notification_closed(id, CloseReason::Expired).await;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:dismiss(id)` removes the entry, lets its expiry task no-op on
    /// recheck, emits `NotificationClosed(..., Dismissed)`, and pushes state. Unknown ids no-op.
    pub async fn dismiss(&self, id: u32) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            remove_by_id(&mut state.queue, id)
        };
        let Some(removed) = removed else {
            eprintln!("notifications: dismiss({id}) ignored: no notification with that id is currently queued");
            return;
        };
        if let Some(path) = removed.image_path {
            delete_icon_file(&path);
        }
        self.emit_notification_closed(id, CloseReason::Dismissed).await;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:reply(id, text)` requires `has_reply`, emits `NotificationReplied(id, text)`,
    /// then removes unless `resident` is set, as [`NotificationsController::invoke_action`] does
    /// for a button. Unknown/no-reply ids log/no-op.
    ///
    /// The text rides its own signal rather than an encoded `ActionInvoked` key; ADR-0033's
    /// amendment says why. `Dismissed` rather than `ClosedByMethod`, because a sent reply is the
    /// user finishing with the notification.
    pub async fn reply(&self, id: u32, text: String) {
        let outcome = {
            let mut state = self.state.lock().unwrap();
            match state.queue.iter().position(|n| n.id == id) {
                Some(index) if !state.queue[index].has_reply => {
                    eprintln!(
                        "notifications: reply({id}, ...) ignored: that notification does not accept an inline reply"
                    );
                    None
                }
                Some(index) if state.queue[index].resident => Some(None),
                Some(_) => Some(remove_by_id(&mut state.queue, id)),
                None => {
                    eprintln!(
                        "notifications: reply({id}, ...) ignored: no notification with that id is currently queued"
                    );
                    None
                }
            }
        };
        let Some(removed) = outcome else { return };
        self.emit_notification_replied(id, text).await;
        if let Some(removed) = removed {
            if let Some(path) = removed.image_path {
                delete_icon_file(&path);
            }
            self.emit_notification_closed(id, CloseReason::Dismissed).await;
        }
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:invoke_action(id, key)` (ADR-0090) validates that the sender declared
    /// `key`, emits `ActionInvoked`, then removes unless `resident` is set. It checks rather than
    /// forwards unknown keys because a key the sender never offered means nothing to it. Unknown
    /// keys log/no-op; non-resident removal also emits `NotificationClosed`.
    pub async fn invoke_action(&self, id: u32, key: String) {
        let outcome = {
            let mut state = self.state.lock().unwrap();
            match state.queue.iter().position(|n| n.id == id) {
                Some(index) if !declares_action(&state.queue[index], &key) => {
                    eprintln!(
                        "notifications: invoke_action({id}, {key:?}) ignored: that notification declares no such action"
                    );
                    None
                }
                Some(index) if state.queue[index].resident => Some(None),
                Some(_) => Some(remove_by_id(&mut state.queue, id)),
                None => {
                    eprintln!(
                        "notifications: invoke_action({id}, {key:?}) ignored: no notification with that id is currently queued"
                    );
                    None
                }
            }
        };
        let Some(removed) = outcome else { return };
        self.emit_action_invoked(id, key).await;
        if let Some(removed) = removed {
            if let Some(path) = removed.image_path {
                delete_icon_file(&path);
            }
            self.emit_notification_closed(id, CloseReason::ClosedByMethod).await;
        }
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:set_sound(urgency, path)` (ADR-0033) registers an existing file under the
    /// sound roots; invalid/untrusted paths log.
    pub fn set_sound(&self, urgency: Urgency, path: &str) {
        match validate_trusted_path(path, &self.sound_roots) {
            Some(validated) => {
                self.state.lock().unwrap().sound_registry.insert(urgency, validated);
            }
            None => eprintln!(
                "notifications: set_sound({urgency:?}, {path:?}) ignored: not a trusted, existing sound file path"
            ),
        }
    }

    /// `notifications:set_dnd(enabled)` (ADR-0033) flips the global sound gate; feed is unchanged.
    pub fn set_dnd(&self, enabled: bool) {
        self.state.lock().unwrap().dnd = enabled;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:set_quiet(enabled)` gates sound like DND, for a config's own rules (locked,
    /// displays off); not in the snapshot, so it never shows as the user's DND.
    pub fn set_quiet(&self, enabled: bool) {
        self.state.lock().unwrap().quiet = enabled;
    }

    /// `notifications:set_app_muted(app, muted)` silences an app that plays its own sound (ADR-0033).
    pub fn set_app_muted(&self, app: String, muted: bool) {
        let mut state = self.state.lock().unwrap();
        if muted {
            state.muted_apps.insert(app);
        } else {
            state.muted_apps.remove(&app);
        }
    }

    /// `notifications:hold_expiry(seconds)` (ADR-0094) pauses all pending countdowns; `0` releases
    /// them. A deadline avoids a paused flag: a config that reloads, crashes, or misses an edge
    /// could otherwise pin the feed for the rest of the session. It self-releases, bounding the
    /// missed edge by [`MAX_EXPIRY_HOLD_SECS`]; configs can refresh a short hold from repeating
    /// events such as reply-field `on_change`.
    /// Not in [`NotificationsState`]: the config is the only writer and already knows its request.
    pub fn hold_expiry(&self, seconds: u64) {
        let until = (seconds > 0).then(|| Instant::now() + Duration::from_secs(seconds.min(MAX_EXPIRY_HOLD_SECS)));
        // Distinguish an intentional hold from a broken timer in logs.
        match until {
            Some(_) => eprintln!("notifications: expiry held for {}s", seconds.min(MAX_EXPIRY_HOLD_SECS)),
            None => eprintln!("notifications: expiry hold released"),
        }
        self.expiry_hold.send_replace(until);
    }

    /// Full re-derivation of `notifications.feed`/`notifications.dnd` from current state --
    /// synchronous, no D-Bus round trip needed.
    pub fn build_state(&self) -> NotificationsState {
        let state = self.state.lock().unwrap();
        NotificationsState { feed: feed_view(&state.queue), dnd: state.dnd }
    }
}

/// Maximum single hold. A hold means somebody is interacting now; five minutes of continuous
/// interaction with one notification is past anything real. It bounds a config asking for a week.
pub const MAX_EXPIRY_HOLD_SECS: u64 = 300;

/// Sleeps `remaining`, pausing for an ADR-0094 hold. Each expiring notification already has one
/// task, so pausing that sleep needs no queue deadline, shared state, re-arming, or second place
/// agreeing with [`expire_entry`]. Served time is banked: a 5s countdown held at 2s has 3s left,
/// not 0 or a restarted 5s.
async fn sleep_past_holds(mut holds: watch::Receiver<Option<Instant>>, mut remaining: Duration) {
    loop {
        let held_until = (*holds.borrow_and_update()).filter(|until| *until > Instant::now());
        if let Some(until) = held_until {
            // Wake on lapse, placement, or release, then re-evaluate.
            let _ = tokio::time::timeout_at(until, holds.changed()).await;
            continue;
        }
        let started = Instant::now();
        match tokio::time::timeout(remaining, holds.changed()).await {
            // No hold change before timeout: duration is served.
            Err(_elapsed) => return,
            Ok(moved) => {
                remaining = remaining.saturating_sub(started.elapsed());
                // Controller gone: no future holds, so finish in one sleep instead of spinning.
                if moved.is_err() {
                    tokio::time::sleep(remaining).await;
                    return;
                }
            }
        }
    }
}

/// Whether `notification` offered `key`, including the non-button `"default"` activation.
fn declares_action(notification: &Notification, key: &str) -> bool {
    (key == "default" && notification.has_default_action) || notification.actions.iter().any(|a| a.key == key)
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
        let action_icons = hints.get("action-icons").and_then(value_as_bool).unwrap_or(false);
        let parsed_actions = parse_actions(&actions, action_icons);
        let resident = hints.get("resident").and_then(value_as_bool).unwrap_or(false);
        let transient = hints.get("transient").and_then(value_as_bool).unwrap_or(false);
        let desktop_entry = desktop_entry_from_hint(hints.get("desktop-entry").and_then(value_as_str));
        let reply_placeholder =
            reply_placeholder_from_hint(hints.get("x-kde-reply-placeholder-text").and_then(value_as_str));

        let image_data = hints.get("image-data").or_else(|| hints.get("image_data")).and_then(decode_raw_image_data);
        let image_path_hint =
            hints.get("image-path").or_else(|| hints.get("image_path")).and_then(value_as_str).map(str::to_string);
        let (image_path, image_name) = split_image_path_hint(image_path_hint);
        // The positional argument wins where a sender set both: it says "this application's icon"
        // and nothing else, where the hint is a fallback from a field that meant a picture.
        let app_icon = (!app_icon.is_empty()).then_some(app_icon).or(image_name);
        let icon_data = hints.get("icon_data").and_then(decode_raw_image_data);
        let app_muted = {
            let state = self.state.lock().unwrap();
            state.muted_apps.contains(&app_name)
                || desktop_entry.as_ref().is_some_and(|entry| state.muted_apps.contains(entry))
        };
        let suppress_sound = app_muted || hints.get("suppress-sound").and_then(value_as_bool).unwrap_or(false);
        let sound_file = hints.get("sound-file").and_then(value_as_str).map(str::to_string);
        let sound_name = hints.get("sound-name").and_then(value_as_str).map(str::to_string);

        let (id, incarnation) = {
            let mut state = self.state.lock().unwrap();
            let id = resolve_notification_id(replaces_id, &mut state.next_id);
            let incarnation = next_incarnation(&mut state.next_incarnation);
            (id, incarnation)
        };
        let image_path = self.resolve_and_spool_image(id, image_data, image_path, icon_data).await;
        let app_icon = resolve_app_icon(app_icon, &self.trusted_roots);

        let notification = Notification {
            id,
            timestamp: epoch_seconds(SystemTime::now()),
            app_name,
            summary,
            body: body_spans,
            image_path,
            app_icon,
            urgency,
            expired: false,
            transient,
            desktop_entry,
            has_reply: parsed_actions.has_reply,
            reply_placeholder,
            actions: parsed_actions.actions,
            has_default_action: parsed_actions.has_default,
            resident,
            incarnation,
        };

        let cleanup = {
            let mut state = self.state.lock().unwrap();
            replace_or_push(&mut state.queue, notification)
        };
        match cleanup {
            Some(QueueCleanup::ReplacedImage(path)) => delete_icon_file(&path),
            Some(QueueCleanup::Evicted { id: evicted_id, image_path }) => {
                if let Some(path) = image_path {
                    delete_icon_file(&path);
                }
                // A FIFO eviction past NOTIFICATION_QUEUE_CAP is a real close, not just an
                // icon-file cleanup -- the evicted id is gone from the queue for good.
                self.emit_notification_closed(evicted_id, CloseReason::Evicted).await;
            }
            None => {}
        }
        let _ = self.events.send(NotificationsSignal::Changed);

        if let Some(duration) = resolve_expiry(urgency, expire_timeout) {
            // ponytail: replacing or dismissing a notification leaves this task sleeping rather
            // than aborting it; the `(id, incarnation)` recheck in `expire_entry` is what makes
            // that correct, and the queue stays the only authority on what is live. Ceiling: an
            // obsolete task holds a controller `Arc` and a hold subscription until its own
            // captured duration elapses, which `expire_timeout` can set to 24.9 days and
            // `hold_expiry` can extend further. Upgrade path is to re-check `find_expiring_entry`
            // on an interval inside `sleep_past_holds`, not a second registry of abort handles
            // that has to agree with the incarnation check forever.
            let controller = self.clone();
            let holds = self.expiry_hold.subscribe();
            tokio::spawn(async move {
                sleep_past_holds(holds, duration).await;
                controller.expire(id, incarnation).await;
            });
        }

        let (silenced, tier_default_sound) = {
            let state = self.state.lock().unwrap();
            (state.dnd || state.quiet, state.sound_registry.get(&urgency).cloned())
        };
        let client_sound_file =
            sound_file.and_then(|path| validate_trusted_path(strip_file_uri(&path), &self.sound_roots));
        let named_sound = sound_name.and_then(|name| resolve_sound_name(&name, &self.sound_roots));
        let sound_path = resolve_sound_path(suppress_sound, client_sound_file, named_sound, tier_default_sound);
        if should_play_sound(silenced, urgency)
            && let Some(sound_path) = sound_path
        {
            let _ = self.sound_tx.try_send(sound_path);
        }

        id
    }

    /// `CloseNotification(id)` removes the entry (if present) and emits `NotificationClosed(id,
    /// reason=ClosedByMethod)`. A `dismiss()` write command emits the same signal with
    /// `reason=Dismissed` instead -- distinct wire callers of the same removal primitive
    /// ([`remove_by_id`]).
    #[zbus(name = "CloseNotification")]
    async fn close_notification(&self, id: u32) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            remove_by_id(&mut state.queue, id)
        };
        if let Some(removed) = removed {
            if let Some(path) = removed.image_path {
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
        ("obelisk".to_string(), "obelisk".to_string(), "0.1.0".to_string(), "1.2".to_string())
    }

    /// `reason` is [`CloseReason`] as its raw wire `u32` (the signal's D-Bus signature is fixed by
    /// the base spec): 1 = expired, 2 = dismissed via `dismiss()`, 3 = `CloseNotification`,
    /// 4 = FIFO eviction -- a base-spec "undefined/reserved" value repurposed for Obelisk's hard
    /// 100-cap (ADR-0033).
    #[zbus(signal, name = "NotificationClosed")]
    async fn notification_closed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "ActionInvoked")]
    async fn action_invoked(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;

    /// The KDE inline-reply extension's signal, which is what advertising `"inline-reply"`
    /// promises. Outside the base spec, so only a client that sent `x-kde-reply` is listening.
    #[zbus(signal, name = "NotificationReplied")]
    async fn notification_replied(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        text: String,
    ) -> zbus::Result<()>;
}

// Write-command argument parsers (ADR-0033); set_dnd calls parse_bool_arg directly.

/// Parses `notifications:dismiss(id)`'s `[id]`.
pub fn parse_dismiss_args(arguments: &[serde_json::Value]) -> Option<u32> {
    arguments.first()?.as_u64().and_then(|v| u32::try_from(v).ok())
}

/// Parses `notifications:reply(id, text)`'s `[id, text]`.
pub fn parse_reply_args(arguments: &[serde_json::Value]) -> Option<(u32, String)> {
    let id = u32::try_from(arguments.first()?.as_u64()?).ok()?;
    let text = arguments.get(1)?.as_str()?.to_string();
    Some((id, text))
}

/// Parses whole-number `[seconds]`. Reject negative/fractional values: elsewhere `-1` means
/// "never", and coercing it to `0` would release a requested hold.
pub fn parse_hold_expiry_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// Parses `notifications:invoke_action(id, key)`'s `[id, key]`.
pub fn parse_invoke_action_args(arguments: &[serde_json::Value]) -> Option<(u32, String)> {
    let id = u32::try_from(arguments.first()?.as_u64()?).ok()?;
    let key = arguments.get(1)?.as_str().filter(|key| !key.is_empty())?.to_string();
    Some((id, key))
}

/// Parses `notifications:set_sound(urgency, path)`'s `[urgency, path]`.
pub fn parse_set_sound_args(arguments: &[serde_json::Value]) -> Option<(Urgency, String)> {
    let urgency = parse_urgency_str(arguments.first()?.as_str()?)?;
    let path = arguments.get(1)?.as_str()?.to_string();
    Some((urgency, path))
}

/// Parses `notifications:set_app_muted(app, muted)`'s `[app, muted]`.
pub fn parse_set_app_muted_args(arguments: &[serde_json::Value]) -> Option<(String, bool)> {
    Some((arguments.first()?.as_str()?.to_string(), arguments.get(1)?.as_bool()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(actions: &[String], action_icons: bool) -> Vec<String> {
        parse_actions(actions, action_icons).actions.into_iter().map(|a| a.key).collect()
    }

    fn flat(pairs: &[(&str, &str)]) -> Vec<String> {
        pairs.iter().flat_map(|(key, label)| [key.to_string(), label.to_string()]).collect()
    }

    /// `default` activates the card and `inline-reply` is a text field, so neither is a button.
    #[test]
    fn default_and_inline_reply_become_flags_rather_than_buttons() {
        let parsed =
            parse_actions(&flat(&[("default", "Open"), ("inline-reply", "Reply"), ("archive", "Archive")]), false);
        assert!(parsed.has_default);
        assert!(parsed.has_reply);
        assert_eq!(parsed.actions.len(), 1);
        assert_eq!(parsed.actions[0].key, "archive");
        assert_eq!(parsed.actions[0].label, "Archive");
    }

    #[test]
    fn an_array_with_neither_key_sets_neither_flag() {
        let parsed = parse_actions(&flat(&[("archive", "Archive")]), false);
        assert!(!parsed.has_default);
        assert!(!parsed.has_reply);
        assert_eq!(parse_actions(&[], false), ParsedActions::default());
    }

    /// Senders omit labels; the usually-readable key becomes the button label.
    #[test]
    fn an_empty_label_falls_back_to_the_key() {
        let parsed = parse_actions(&flat(&[("archive", "")]), false);
        assert_eq!(parsed.actions[0].label, "archive");
    }

    /// Preserve the base spec's odd key-without-label case rather than rejecting the notification.
    #[test]
    fn an_odd_length_array_still_yields_its_last_action() {
        assert_eq!(keys(&["archive".to_string()], false), ["archive"]);
    }

    /// Under `action-icons`, carry a key as a theme name unless it contains `/`; `icon` also takes
    /// paths, so the separator check blocks arbitrary file access.
    #[test]
    fn action_icons_carries_the_key_as_an_icon_name_but_never_as_a_path() {
        let parsed = parse_actions(&flat(&[("mail-archive", "")]), true);
        assert_eq!(parsed.actions[0].icon_name.as_deref(), Some("mail-archive"));

        let parsed = parse_actions(&flat(&[("/home/anas/.ssh/id_ed25519", "Archive")]), true);
        assert_eq!(parsed.actions[0].icon_name, None, "a path must not be carried as an icon name");

        let parsed = parse_actions(&flat(&[("mail-archive", "")]), false);
        assert_eq!(parsed.actions[0].icon_name, None, "without the hint the key is not an icon");
    }

    /// Drop actions with neither a label nor an icon instead of making blank buttons.
    #[test]
    fn an_action_with_nothing_to_draw_is_dropped() {
        let parsed = parse_actions(&flat(&[("", ""), ("", "Orphan label")]), true);
        assert!(parsed.actions.is_empty(), "got {:?}", parsed.actions);
    }

    #[test]
    fn the_action_count_and_label_length_are_both_capped() {
        let many: Vec<(String, String)> = (0..20).map(|i| (format!("k{i}"), format!("l{i}"))).collect();
        let flattened: Vec<String> = many.iter().flat_map(|(k, l)| [k.clone(), l.clone()]).collect();
        assert_eq!(parse_actions(&flattened, false).actions.len(), MAX_ACTIONS);

        let long = "x".repeat(1000);
        let parsed = parse_actions(&["k".to_string(), long], false);
        assert_eq!(parsed.actions[0].label.len(), MAX_ACTION_LABEL_BYTES);
    }

    /// Guards `ActionInvoked` to sender-declared keys; `"default"` counts outside the button list.
    #[test]
    fn declares_action_covers_the_buttons_and_the_default_activation() {
        let mut notification = Notification {
            id: 1,
            timestamp: 0,
            app_name: "app".to_string(),
            summary: "s".to_string(),
            body: Vec::new(),
            image_path: None,
            app_icon: None,
            urgency: Urgency::Normal,
            expired: false,
            transient: false,
            desktop_entry: None,
            has_reply: false,
            reply_placeholder: None,
            actions: vec![NotificationAction {
                key: "archive".to_string(),
                label: "Archive".to_string(),
                icon_name: None,
            }],
            has_default_action: false,
            resident: false,
            incarnation: 0,
        };
        assert!(declares_action(&notification, "archive"));
        assert!(!declares_action(&notification, "delete"));
        assert!(!declares_action(&notification, "default"));

        notification.has_default_action = true;
        assert!(declares_action(&notification, "default"));
    }

    #[test]
    fn parse_invoke_action_args_needs_an_id_and_a_nonempty_key() {
        use serde_json::json;
        assert_eq!(parse_invoke_action_args(&[json!(7), json!("archive")]), Some((7, "archive".to_string())));
        assert_eq!(parse_invoke_action_args(&[json!(7), json!("")]), None);
        assert_eq!(parse_invoke_action_args(&[json!(7)]), None);
        assert_eq!(parse_invoke_action_args(&[json!("seven"), json!("archive")]), None);
    }

    #[test]
    fn close_reason_maps_to_the_documented_wire_values() {
        assert_eq!(u32::from(CloseReason::Expired), 1);
        assert_eq!(u32::from(CloseReason::Dismissed), 2);
        assert_eq!(u32::from(CloseReason::ClosedByMethod), 3);
        assert_eq!(u32::from(CloseReason::Evicted), 4);
    }

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
        assert_eq!(
            parse_reply_args(&[serde_json::json!(7), serde_json::json!("sounds good")]),
            Some((7, "sounds good".to_string()))
        );
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

    #[test]
    fn parse_hold_expiry_args_reads_a_whole_number_of_seconds_and_nothing_else() {
        assert_eq!(parse_hold_expiry_args(&[serde_json::json!(10)]), Some(10));
        assert_eq!(parse_hold_expiry_args(&[serde_json::json!(0)]), Some(0), "0 is the release, not a malformed hold");
        assert_eq!(parse_hold_expiry_args(&[serde_json::json!(-1)]), None, "not silently a release");
        assert_eq!(parse_hold_expiry_args(&[serde_json::json!(1.5)]), None);
        assert_eq!(parse_hold_expiry_args(&[]), None);
    }

    /// Paused tokio time makes this exact and instant: an unheld countdown serves its duration.
    #[tokio::test(start_paused = true)]
    async fn an_unheld_countdown_runs_for_exactly_its_duration() {
        let holds = watch::Sender::new(None);
        let started = Instant::now();
        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    /// A hold at 2s of 5 leaves 3s to serve, so the card outlives the pointer resting on it.
    #[tokio::test(start_paused = true)]
    async fn a_hold_stops_the_clock_and_the_time_already_served_is_banked() {
        let holds = Arc::new(watch::Sender::new(None));
        let started = Instant::now();

        let placer = Arc::clone(&holds);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            placer.send_replace(Some(Instant::now() + Duration::from_secs(60)));
        });

        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        // 2 served, 60 held, 3 left.
        assert_eq!(started.elapsed(), Duration::from_secs(65));
    }

    /// `hold_expiry(0)` releases early rather than waiting out the original deadline.
    #[tokio::test(start_paused = true)]
    async fn releasing_a_hold_resumes_the_countdown_at_once() {
        let holds = Arc::new(watch::Sender::new(Some(Instant::now() + Duration::from_secs(600))));
        let started = Instant::now();

        let releaser = Arc::clone(&holds);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            releaser.send_replace(None);
        });

        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(9), "4 held, then the full 5 -- none was served first");
    }

    /// Repeated events such as reply `on_change` extend the deadline; the later hold must win.
    #[tokio::test(start_paused = true)]
    async fn a_second_hold_extends_the_first() {
        let holds = Arc::new(watch::Sender::new(Some(Instant::now() + Duration::from_secs(10))));
        let started = Instant::now();

        let extender = Arc::clone(&holds);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            extender.send_replace(Some(Instant::now() + Duration::from_secs(10)));
        });

        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(23), "8 + 10 held, then the full 5");
    }

    /// An already-lapsed deadline is not a hold; without this check stale values would stop time.
    #[tokio::test(start_paused = true)]
    async fn a_lapsed_hold_does_not_stop_the_clock() {
        let holds = watch::Sender::new(Some(Instant::now() + Duration::from_millis(1)));
        tokio::time::sleep(Duration::from_secs(1)).await;
        let started = Instant::now();
        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    /// An absurd hold clamps to five minutes, not the rest of the session.
    #[tokio::test(start_paused = true)]
    async fn a_hold_longer_than_the_cap_is_clamped_to_it() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (sound_tx, _sound_rx) = std::sync::mpsc::sync_channel(1);
        let controller = NotificationsController::inert(events, sound_tx);

        let before = Instant::now();
        controller.hold_expiry(u64::MAX);
        let held = controller.expiry_hold.borrow().expect("a hold was placed");
        assert_eq!(held, before + Duration::from_secs(MAX_EXPIRY_HOLD_SECS), "u64::MAX seconds is five minutes");

        controller.hold_expiry(0);
        assert_eq!(*controller.expiry_hold.borrow(), None, "0 releases");
    }

    #[tokio::test]
    async fn a_muted_app_is_silent_by_desktop_entry_or_app_name() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (sound_tx, sound_rx) = std::sync::mpsc::sync_channel(1);
        let controller = &NotificationsController::inert(events, sound_tx);
        controller.set_sound(Urgency::Normal, "/usr/share/sounds/freedesktop/stereo/message.oga");
        controller.set_app_muted("vesktop".into(), true);
        let notify = move |app_name: &str, desktop_entry: Option<&'static str>| {
            let hints =
                desktop_entry.map(|entry| ("desktop-entry".to_string(), Value::from(entry))).into_iter().collect();
            controller.notify(app_name.into(), 0, String::new(), "s".into(), String::new(), vec![], hints, -1)
        };

        notify("Vesktop", Some("vesktop")).await;
        notify("vesktop", None).await;
        assert!(sound_rx.try_recv().is_err(), "muted by desktop-entry and by app name");
        controller.set_app_muted("vesktop".into(), false);
        notify("vesktop", None).await;
        assert!(sound_rx.try_recv().is_ok(), "unmuting plays the tier sound again");
    }
}
