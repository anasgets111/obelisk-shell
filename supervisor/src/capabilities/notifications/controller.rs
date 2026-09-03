//! [`NotificationsController`]: the D-Bus interface + write-action dispatcher and state owner
//! (deliberately fused into one type -- see the module-level doc for why). Split from
//! `dbus::notifications` -- see `dbus/notifications/mod.rs` for the module-level doc.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
// `tokio`'s clock, not `std`'s: these are timer deadlines, and only this one moves with
// `tokio::time::pause`, which is what makes the countdown testable without sleeping through it.
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
    Expiry, ExpiryPolicy, QueueCleanup, expire_entry, feed_view, next_incarnation, remove_by_id, replace_or_push,
    resolve_expiry, resolve_notification_id, resolve_sound_path, should_play_sound,
};
use super::sound::SoundSender;
use super::{
    MAX_ACTION_LABEL_BYTES, MAX_ACTIONS, MAX_APP_NAME_BYTES, MAX_SUMMARY_BYTES, NOTIFICATIONS_BUS_NAME,
    NOTIFICATIONS_CAPABILITIES, NOTIFICATIONS_OBJECT_PATH, Notification, NotificationAction, NotificationsSignal,
    NotificationsState, Urgency, desktop_entry_from_hint, parse_urgency_str, reply_placeholder_from_hint,
    truncate_utf8_bytes, urgency_from_hint_byte,
};
use crate::capabilities::system::controller::epoch_seconds;

// -------------------------------------------------------------------------------------------
// ActionInvoked reply encoding (TDD seam 4).
// -------------------------------------------------------------------------------------------

/// `ActionInvoked`'s `action_key` for a completed inline reply (ADR-0033: `"inline-reply::<text>"`).
fn format_reply_action_key(text: &str) -> String {
    format!("inline-reply::{text}")
}

/// What `Notify`'s flat `[key1, label1, key2, label2, ...]` array splits into (ADR-0090). Three
/// values that all come off one walk of the same array, returned together rather than as three
/// passes each re-deciding what a key means.
#[derive(Debug, Default, PartialEq)]
pub(super) struct ParsedActions {
    pub actions: Vec<NotificationAction>,
    /// A `"default"` key was present: the notification as a whole is activatable.
    pub has_default: bool,
    /// An `"inline-reply"` key was present -- §1.2's `x-kde-reply` convention rides on exactly
    /// that key.
    pub has_reply: bool,
}

/// Splits `Notify`'s `actions` into the buttons a config draws and the two keys that are not
/// buttons (ADR-0090).
///
/// An odd-length array is a sender that sent a key with no label, which the base spec does not
/// allow and which is not worth rejecting a whole notification over: the label reads as empty and
/// the key stands in for it.
///
/// `action_icons` is the sender's hint that each key doubles as a theme icon name. An action is
/// kept only if it can actually be drawn -- a label, or an icon to draw instead of one -- so a
/// sender that offers `["", ""]` gets nothing rather than an unlabelled button that does something
/// unguessable when pressed.
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
        // A theme name, never a path: `icon` accepts an absolute path too, so without this a
        // sender could name any readable file on the machine and have the shell draw it.
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

// -------------------------------------------------------------------------------------------
// D-Bus interface + controller. `NotificationsController` is both the exported
// `org.freedesktop.Notifications` object and the cheap-clone handle `main.rs` holds for
// write-command dispatch -- unlike `dbus::tray`'s split, one type serves both roles here.
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

/// `NotificationClosed`'s `reason` argument (§1's base spec, plus ADR-0033's repurposing of the
/// spec's undefined/reserved `4` for Oblisk's own FIFO-eviction cap).
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
    /// session-bus connection -- `None` degrades every signal emission to a silent no-op, while
    /// queue/DND/sound state stays fully functional either way.
    connection: Option<zbus::Connection>,
    state: Arc<Mutex<NotificationsQueueState>>,
    events: UnboundedSender<NotificationsSignal>,
    sound_tx: SoundSender,
    trusted_roots: Arc<Vec<PathBuf>>,
    /// The instant every pending expiry countdown is stopped until, or `None` when none is
    /// (ADR-0094). A `watch` rather than a field in [`NotificationsQueueState`] because the
    /// readers are the spawned countdowns, which need to be *woken* when it moves and not merely
    /// to find the new value the next time they happen to look.
    expiry_hold: Arc<watch::Sender<Option<Instant>>>,
}

impl NotificationsController {
    /// Requests `org.freedesktop.Notifications` with `DoNotQueue` set: a real desktop might
    /// already have `mako`/`dunst` running and owning this name, a genuine "someone else already
    /// provides this" outcome to degrade to inert for, not a race to queue behind (ADR-0033).
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
            expiry_hold: Arc::new(watch::Sender::new(None)),
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

    /// Fully inert controller: no D-Bus connection, but a real, working queue/DND/sound-registry
    /// state -- used when the session bus itself couldn't be reached at all. Every write command
    /// still behaves sensibly; only signal emission and `Notify` arriving over D-Bus never happen.
    pub fn inert(events: UnboundedSender<NotificationsSignal>, sound_tx: SoundSender) -> Self {
        Self {
            connection: None,
            state: Arc::new(Mutex::new(NotificationsQueueState::new())),
            events,
            sound_tx,
            trusted_roots: Arc::new(default_trusted_icon_roots()),
            expiry_hold: Arc::new(watch::Sender::new(None)),
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

    /// Resolves `Notify`'s attached-picture precedence ([`resolve_image_input`]) into a final,
    /// spooled/validated `image_path`. `image-data`/`icon_data` get bounds-checked and
    /// PNG-encoded/spooled to SHM; `image-path` runs through the same [`validate_trusted_path`]
    /// boundary body-markup images use.
    ///
    /// The positional `app_icon` argument is not in here any more: it is the application's own
    /// icon rather than the picture it attached, and [`resolve_app_icon`] handles it (ADR-0091).
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

    /// Fires once, at the end of the `Duration` a `notify()` call scheduled it for -- `id` and
    /// `incarnation` together identify which `Notify` call's content this timer is for.
    /// [`expire_entry`] is the recheck: if a `replaces_id` update already landed new content at
    /// `id`, `incarnation` no longer matches and this is a silent no-op.
    ///
    /// Retires rather than removes (ADR-0100): the entry stays in the queue with `expired` set,
    /// so the history keeps showing what happened after the popup has gone, and `dismiss` is what
    /// removes it. The sender is told the same thing it always was -- `NotificationClosed(id,
    /// reason=1)` -- because from its side the notification *has* closed: it will get no
    /// `ActionInvoked` it can rely on and should not try to update the entry in place. A
    /// `transient` entry is the exception and is removed the way every expiry used to be.
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

    /// `notifications:dismiss(id)` (§3.2): removes the entry, cancels nothing explicitly (the
    /// pending expiry task's own recheck-before-acting sees it's gone and no-ops), emits
    /// `NotificationClosed(id, reason=Dismissed)`, and signals a fresh `StateSnapshot` push. A
    /// logged no-op for an unknown id.
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

    /// `notifications:reply(id, text)` (ADR-0033): confirms the notification `has_reply`, emits
    /// `ActionInvoked(id, format_reply_action_key(text))`, and removes it -- a replied-to
    /// notification is done. Logged no-ops for an unknown id or one with no reply.
    pub async fn reply(&self, id: u32, text: String) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            match state.queue.iter().position(|n| n.id == id) {
                Some(index) if state.queue[index].has_reply => remove_by_id(&mut state.queue, id),
                Some(_) => {
                    eprintln!(
                        "notifications: reply({id}, ...) ignored: that notification does not accept an inline reply"
                    );
                    None
                }
                None => {
                    eprintln!(
                        "notifications: reply({id}, ...) ignored: no notification with that id is currently queued"
                    );
                    None
                }
            }
        };
        let Some(removed) = removed else { return };
        if let Some(path) = removed.image_path {
            delete_icon_file(&path);
        }
        self.emit_action_invoked(id, format_reply_action_key(&text)).await;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:invoke_action(id, key)` (ADR-0090): confirms the notification actually
    /// declared `key`, emits `ActionInvoked(id, key)`, and then removes it unless the sender set
    /// `hints["resident"]`.
    ///
    /// The key is checked rather than forwarded, for the same reason `reply` checks `has_reply`:
    /// a key the sender never offered means nothing to it, and the round trip that discovers that
    /// is a signal the application has to field and ignore. Logged no-ops for an unknown id or an
    /// undeclared key.
    ///
    /// Removing afterwards is the base spec's default and `resident` is its own exception to it --
    /// a media notification whose prev/next buttons closed the card on first press would be
    /// useless. `NotificationClosed` is emitted alongside, because an action-invoked removal is a
    /// close like any other and a sender tracking its own ids needs to hear about it.
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

    /// `notifications:set_sound(urgency, path)` (ADR-0033): registers `path` for `urgency`'s tier
    /// if it passes the same path-trust validator as icon references. An invalid/untrusted path
    /// is a logged no-op.
    pub fn set_sound(&self, urgency: Urgency, path: &str) {
        match validate_trusted_path(path, &self.trusted_roots) {
            Some(validated) => {
                self.state.lock().unwrap().sound_registry.insert(urgency, validated);
            }
            None => eprintln!(
                "notifications: set_sound({urgency:?}, {path:?}) ignored: not a trusted, existing sound file path"
            ),
        }
    }

    /// `notifications:set_dnd(enabled)` (ADR-0033): flips the Supervisor-global toggle. Gates
    /// sound only -- `notifications.feed` keeps receiving everything regardless.
    pub fn set_dnd(&self, enabled: bool) {
        self.state.lock().unwrap().dnd = enabled;
        let _ = self.events.send(NotificationsSignal::Changed);
    }

    /// `notifications:hold_expiry(seconds)` (ADR-0094): stops every pending expiry countdown for
    /// the next `seconds`, so a notification cannot vanish out from under someone reading it or
    /// part-way through typing a reply into it. `0` releases the hold immediately.
    ///
    /// A deadline rather than a paused/resumed flag, and that is the whole point: a config that
    /// pauses and never resumes -- because it reloaded, crashed, or simply missed an edge --
    /// pins the feed for the rest of the session, and nothing in the Supervisor could tell that
    /// state from a legitimately long one. A deadline lapses on its own, so the worst a bug can
    /// do is bounded by [`MAX_EXPIRY_HOLD_SECS`], and the natural way to use it is to keep
    /// re-placing a short one from an event that is already repeating (a reply field's
    /// `on_change` fires per keystroke).
    ///
    /// Not in [`NotificationsState`]: the config is the only writer and already knows what it
    /// asked for, and a readable one invites a second reader to make decisions from a value that
    /// is stale the moment it is pushed.
    pub fn hold_expiry(&self, seconds: u64) {
        let until = (seconds > 0).then(|| Instant::now() + Duration::from_secs(seconds.min(MAX_EXPIRY_HOLD_SECS)));
        // Worth a line each way. A feed that has stopped expiring looks identical to a broken
        // timer from the outside, and this is the one thing that tells them apart.
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

/// The longest a single [`NotificationsController::hold_expiry`] call can stop the countdowns for.
/// A hold asserts "somebody is interacting with this right now", and five minutes of continuous
/// interaction with one notification is past anything real; the cap is what keeps a config that
/// asks for a week from pinning the feed for the session.
pub const MAX_EXPIRY_HOLD_SECS: u64 = 300;

/// Sleeps out `remaining`, with the clock stopped for as long as a `hold_expiry` deadline is in
/// force (ADR-0094).
///
/// The countdown lives here rather than as a deadline on the queue entry because that is all it
/// is -- one task per expiring notification already existed, and giving it a pausable sleep costs
/// no shared state, no re-arming on release, and no second place that has to agree with
/// [`expire_entry`] about what is still pending.
///
/// Time already served is banked across a hold: a notification held at 2s of 5 has 3s left when
/// the hold lapses, not 0. Restarting it would be wrong and expiring it immediately would be
/// worse -- the card would vanish at the instant the pointer left it, which reads as the pointer
/// having dismissed it.
async fn sleep_past_holds(mut holds: watch::Receiver<Option<Instant>>, mut remaining: Duration) {
    loop {
        let held_until = (*holds.borrow_and_update()).filter(|until| *until > Instant::now());
        if let Some(until) = held_until {
            // Stopped. Wake when the hold lapses, or sooner if one is placed or released, and
            // decide again from the top rather than assuming which of the two happened.
            let _ = tokio::time::timeout_at(until, holds.changed()).await;
            continue;
        }
        let started = Instant::now();
        match tokio::time::timeout(remaining, holds.changed()).await {
            // The countdown ran out with nothing interrupting it: the time is served.
            Err(_elapsed) => return,
            Ok(moved) => {
                remaining = remaining.saturating_sub(started.elapsed());
                // The sender is gone, which in a live Supervisor means the controller itself is,
                // so there will never be another hold: serve the rest in one sleep instead of
                // spinning on a channel that can only keep erroring.
                if moved.is_err() {
                    tokio::time::sleep(remaining).await;
                    return;
                }
            }
        }
    }
}

/// Whether `notification` offered `key`, counting the `"default"` activation that
/// [`parse_actions`] deliberately keeps out of the button list.
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
        let suppress_sound = hints.get("suppress-sound").and_then(value_as_bool).unwrap_or(false);
        let sound_file = hints.get("sound-file").and_then(value_as_str).map(str::to_string);

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

        if let ExpiryPolicy::After(duration) = resolve_expiry(urgency, expire_timeout) {
            let controller = self.clone();
            let holds = self.expiry_hold.subscribe();
            tokio::spawn(async move {
                sleep_past_holds(holds, duration).await;
                controller.expire(id, incarnation).await;
            });
        }

        let (dnd, tier_default_sound) = {
            let state = self.state.lock().unwrap();
            (state.dnd, state.sound_registry.get(&urgency).cloned())
        };
        let client_sound_file =
            sound_file.and_then(|path| validate_trusted_path(strip_file_uri(&path), &self.trusted_roots));
        let sound_path = resolve_sound_path(suppress_sound, client_sound_file, tier_default_sound);
        if should_play_sound(dnd, urgency, sound_path.is_some())
            && let Some(sound_path) = sound_path
        {
            let _ = self.sound_tx.send(sound_path);
        }

        id
    }

    /// `CloseNotification(id)` (§1): removes the entry (if present) and emits
    /// `NotificationClosed(id, reason=ClosedByMethod)`. A `dismiss()` write command emits the
    /// same signal with `reason=Dismissed` instead -- distinct wire callers of the same removal
    /// primitive ([`remove_by_id`]).
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
        ("oblisk".to_string(), "oblisk".to_string(), "0.1.0".to_string(), "1.2".to_string())
    }

    /// `reason` is [`CloseReason`] as its raw wire `u32` (the signal's D-Bus signature is fixed by
    /// the base spec): 1 = expired, 2 = dismissed via `dismiss()`, 3 = `CloseNotification`,
    /// 4 = FIFO eviction -- a base-spec "undefined/reserved" value repurposed for Oblisk's hard
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
}

// -------------------------------------------------------------------------------------------
// Write-command argument parsers (§3.2, ADR-0033's added rows). set_dnd reuses
// dbus::parse_bool_arg directly at the call site rather than a redundant wrapper here.
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

/// `notifications:hold_expiry(seconds)`'s `arguments: [seconds]`. A negative or fractional number
/// is malformed rather than rounded: `-1` in an expiry argument means "never" everywhere else in
/// this spec, and quietly reading it as `0` would release a hold a config meant to place.
pub fn parse_hold_expiry_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `notifications:invoke_action(id, key)`'s `arguments: [id, key]`.
pub fn parse_invoke_action_args(arguments: &[serde_json::Value]) -> Option<(u32, String)> {
    let id = u32::try_from(arguments.first()?.as_u64()?).ok()?;
    let key = arguments.get(1)?.as_str().filter(|key| !key.is_empty())?.to_string();
    Some((id, key))
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

    // ---- parse_actions (ADR-0090) ----

    fn keys(actions: &[String], action_icons: bool) -> Vec<String> {
        parse_actions(actions, action_icons).actions.into_iter().map(|a| a.key).collect()
    }

    fn flat(pairs: &[(&str, &str)]) -> Vec<String> {
        pairs.iter().flat_map(|(key, label)| [key.to_string(), label.to_string()]).collect()
    }

    /// The two keys that are not buttons come out as their own flags and out of the list, because
    /// drawing either as a button is wrong: `"default"` is the whole card, and `"inline-reply"`
    /// is a text field.
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

    /// Senders do send a key with no label. The key is what the button says rather than the
    /// button vanishing, since the key is usually a word like "archive".
    #[test]
    fn an_empty_label_falls_back_to_the_key() {
        let parsed = parse_actions(&flat(&[("archive", "")]), false);
        assert_eq!(parsed.actions[0].label, "archive");
    }

    /// The base spec's odd case: a key with no label at all, which it does not allow and which is
    /// not worth refusing the whole notification over.
    #[test]
    fn an_odd_length_array_still_yields_its_last_action() {
        assert_eq!(keys(&["archive".to_string()], false), ["archive"]);
    }

    /// Under `action-icons` the key names a theme icon, so it is carried as one -- unless it holds
    /// a path separator, because `icon` also takes absolute paths and a sender must not be able to
    /// point the shell at an arbitrary file.
    #[test]
    fn action_icons_carries_the_key_as_an_icon_name_but_never_as_a_path() {
        let parsed = parse_actions(&flat(&[("mail-archive", "")]), true);
        assert_eq!(parsed.actions[0].icon_name.as_deref(), Some("mail-archive"));

        let parsed = parse_actions(&flat(&[("/home/anas/.ssh/id_ed25519", "Archive")]), true);
        assert_eq!(parsed.actions[0].icon_name, None, "a path must not be carried as an icon name");

        let parsed = parse_actions(&flat(&[("mail-archive", "")]), false);
        assert_eq!(parsed.actions[0].icon_name, None, "without the hint the key is not an icon");
    }

    /// An action that can be drawn neither as a label nor as an icon does nothing a user could
    /// predict, so it is dropped rather than becoming a blank button.
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

    // ---- declares_action ----

    /// The guard that keeps a config from emitting an `ActionInvoked` the sending application
    /// cannot interpret. `"default"` counts even though it is not in the button list.
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

    // ---- parse_invoke_action_args ----

    #[test]
    fn parse_invoke_action_args_needs_an_id_and_a_nonempty_key() {
        use serde_json::json;
        assert_eq!(parse_invoke_action_args(&[json!(7), json!("archive")]), Some((7, "archive".to_string())));
        assert_eq!(parse_invoke_action_args(&[json!(7), json!("")]), None);
        assert_eq!(parse_invoke_action_args(&[json!(7)]), None);
        assert_eq!(parse_invoke_action_args(&[json!("seven"), json!("archive")]), None);
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

    /// The countdown with nothing holding it: it serves its time and no more. `tokio::time::pause`
    /// makes the clock jump to each timer rather than sleeping, so these run instantly and are
    /// exact rather than timing-tolerant.
    #[tokio::test(start_paused = true)]
    async fn an_unheld_countdown_runs_for_exactly_its_duration() {
        let holds = watch::Sender::new(None);
        let started = Instant::now();
        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    /// The reason this is a stopped clock and not a restart or an immediate expiry: a hold placed
    /// at 2s of 5 must leave 3s to serve, so the card outlives the pointer that was resting on it.
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
        // 2 served, 60 held, 3 left to serve.
        assert_eq!(started.elapsed(), Duration::from_secs(65));
    }

    /// A hold released early ends the stop early: `hold_expiry(0)` is the config saying it is done,
    /// and waiting out the original deadline anyway would make a release do nothing.
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

    /// Re-placing a hold while one is already in force is the shape a repeating event uses (a
    /// reply field's `on_change`, once per keystroke), so the later deadline has to win rather
    /// than the countdown resuming when the first one lapses.
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

    /// A hold already lapsed by the time a countdown reads it is not a hold. Without the deadline
    /// check the countdown would stop for a value nobody meant to still be in force.
    #[tokio::test(start_paused = true)]
    async fn a_lapsed_hold_does_not_stop_the_clock() {
        let holds = watch::Sender::new(Some(Instant::now() + Duration::from_millis(1)));
        tokio::time::sleep(Duration::from_secs(1)).await;
        let started = Instant::now();
        sleep_past_holds(holds.subscribe(), Duration::from_secs(5)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    /// The cap is what bounds a config bug: an absurd hold is clamped rather than honoured, so the
    /// worst case is five minutes of a pinned feed and not the rest of the session.
    #[tokio::test(start_paused = true)]
    async fn a_hold_longer_than_the_cap_is_clamped_to_it() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (sound_tx, _sound_rx) = std::sync::mpsc::channel();
        let controller = NotificationsController::inert(events, sound_tx);

        let before = Instant::now();
        controller.hold_expiry(u64::MAX);
        let held = controller.expiry_hold.borrow().expect("a hold was placed");
        assert_eq!(held, before + Duration::from_secs(MAX_EXPIRY_HOLD_SECS), "u64::MAX seconds is five minutes");

        controller.hold_expiry(0);
        assert_eq!(*controller.expiry_hold.borrow(), None, "0 releases");
    }
}
