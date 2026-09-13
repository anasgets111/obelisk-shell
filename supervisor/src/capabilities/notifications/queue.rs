//! Urgency expiry, DND sound gating, and FIFO queue/icon lifecycle. Split from
//! `dbus::notifications`, see `dbus/notifications/mod.rs` for the module-level doc.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use super::{DEFAULT_EXPIRE_MS, NOTIFICATION_FEED_VIEW, NOTIFICATION_QUEUE_CAP, Notification, Urgency};

/// Whether a notification auto-expires, and when.
/// Resolves `expire_timeout` (ADR-0033): critical never expires; otherwise `0` means never,
/// negative means [`DEFAULT_EXPIRE_MS`] (mako/dunst convention), and positive means milliseconds.
pub(super) fn resolve_expiry(urgency: Urgency, expire_timeout: i32) -> Option<Duration> {
    if urgency == Urgency::Critical {
        return None;
    }
    match expire_timeout {
        0 => None,
        t if t < 0 => Some(Duration::from_millis(DEFAULT_EXPIRE_MS)),
        t => Some(Duration::from_millis(t as u64)),
    }
}

/// Whether sound plays (ADR-0033): it needs a registered tier sound and bypasses DND only for
/// critical urgency.
pub(super) fn should_play_sound(dnd: bool, urgency: Urgency, sound_registered: bool) -> bool {
    sound_registered && (!dnd || urgency == Urgency::Critical)
}

/// Selects one `Notify` sound (ADR-0033): `suppress` wins; otherwise `client_sound_file`, already
/// validated through the same path-trust boundary as `image-path`, beats the tier default.
/// `hints["sound-name"]` is not honored; DND/urgency is separate.
pub(super) fn resolve_sound_path(
    suppress: bool,
    client_sound_file: Option<PathBuf>,
    tier_default: Option<PathBuf>,
) -> Option<PathBuf> {
    if suppress {
        return None;
    }
    client_sound_file.or(tier_default)
}

// Queue mutation is pure `VecDeque` logic; callers delete reported orphaned icons.

/// Allocates a monotonic content stamp for expiry timers to compare against replacements
/// ([`find_expiring_entry`]). No wraparound guard: exhausting `u64` in one process is unrealistic.
pub(super) fn next_incarnation(counter: &mut u64) -> u64 {
    let value = *counter;
    *counter = counter.wrapping_add(1);
    value
}

/// Resolves ids: `0` allocates monotonically and wraps to `1` (wire `0` means "new"); nonzero
/// `replaces_id` passes through without consuming an id (base spec/ADR-0033).
pub(super) fn resolve_notification_id(replaces_id: u32, next_id: &mut u32) -> u32 {
    if replaces_id != 0 {
        return replaces_id;
    }
    let id = *next_id;
    *next_id = if *next_id == u32::MAX { 1 } else { *next_id + 1 };
    id
}

/// Cleanup after queue mutation. FIFO eviction needs icon deletion and
/// `NotificationClosed(evicted_id, reason=Evicted)` (finding 3); same-id replacement needs only
/// deletion because the id remains queued.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum QueueCleanup {
    /// Same id; a changed/cleared icon is orphaned by `replaces_id`.
    ReplacedImage(String),
    /// A different notification fell off the queue.
    Evicted { id: u32, image_path: Option<String> },
}

/// Appends a new arrival, evicting past [`NOTIFICATION_QUEUE_CAP`]. FIFO eviction returns the
/// evicted id/icon so the caller deletes its spooled file in the same step and emits finding 3's
/// `NotificationClosed(..., reason=Evicted)` signal.
fn push_new(queue: &mut VecDeque<Notification>, notification: Notification) -> Option<QueueCleanup> {
    queue.push_back(notification);
    if queue.len() > NOTIFICATION_QUEUE_CAP {
        queue.pop_front().map(|evicted| QueueCleanup::Evicted { id: evicted.id, image_path: evicted.image_path })
    } else {
        None
    }
}

/// Resolves replacement images: no fresh image clears and deletes the old path rather than leaving
/// a stale image attached to new text (ADR-0033); a different path also deletes old; reusing the
/// same path deletes nothing.
fn resolve_replacement_image(
    previous_image_path: Option<String>,
    fresh_image_path: Option<String>,
) -> (Option<String>, Option<String>) {
    match (&previous_image_path, &fresh_image_path) {
        (Some(old), Some(new)) if old != new => (fresh_image_path, previous_image_path),
        (Some(_), Some(_)) => (fresh_image_path, None),
        (Some(_), None) => (None, previous_image_path),
        (None, _) => (fresh_image_path, None),
    }
}

/// Replaces a present id in place, preserving order; otherwise falls back to [`push_new`]. Returns
/// the old icon cleanup or the fallback eviction (including its close signal).
pub(super) fn replace_or_push(
    queue: &mut VecDeque<Notification>,
    mut notification: Notification,
) -> Option<QueueCleanup> {
    let target_id = notification.id;
    if let Some(existing) = queue.iter_mut().find(|entry| entry.id == target_id) {
        let previous_icon = existing.image_path.clone();
        let (resolved_icon, to_delete) = resolve_replacement_image(previous_icon, notification.image_path.take());
        notification.image_path = resolved_icon;
        *existing = notification;
        to_delete.map(QueueCleanup::ReplacedImage)
    } else {
        push_new(queue, notification)
    }
}

/// Finds the entry a timer captured as `(id, incarnation)` for, or `None` when it was removed or a
/// newer `replaces_id` superseded it. The newer timer handles expiry on its own schedule.
pub(super) fn find_expiring_entry(queue: &VecDeque<Notification>, id: u32, incarnation: u64) -> Option<usize> {
    queue.iter().position(|entry| entry.id == id && entry.incarnation == incarnation)
}

/// What an expiry did to the queue (ADR-0100).
#[derive(Debug, PartialEq)]
pub(super) enum Expiry {
    /// The entry stays, now marked `expired`: out of the popup, still in the history.
    Retired,
    /// The entry is gone, because the sender marked it `transient` -- a popup and nothing more.
    /// Carries the spooled image path, if any, so the caller can delete the file.
    Removed { image_path: Option<String> },
}

/// Expiry decision for [`NotificationsController::expire`] (ADR-0100): expiry used to remove an
/// entry, making history show what was still popped up rather than what had happened. Retire
/// ordinary entries for history and let `dismiss` remove them; remove `transient` entries as
/// before.
/// `None` covers stale/missing or already-expired entries, preventing duplicate close signals.
pub(super) fn expire_entry(queue: &mut VecDeque<Notification>, id: u32, incarnation: u64) -> Option<Expiry> {
    let index = find_expiring_entry(queue, id, incarnation)?;
    if queue[index].expired {
        return None;
    }
    if queue[index].transient {
        return queue.remove(index).map(|removed| Expiry::Removed { image_path: removed.image_path });
    }
    queue[index].expired = true;
    Some(Expiry::Retired)
}

/// Shared removal for `dismiss`, `reply`, and `CloseNotification`; callers delete any icon and
/// emit the appropriate signal.
pub(super) fn remove_by_id(queue: &mut VecDeque<Notification>, id: u32) -> Option<Notification> {
    let index = queue.iter().position(|entry| entry.id == id)?;
    queue.remove(index)
}

/// Newest [`NOTIFICATION_FEED_VIEW`] entries for `notifications.feed` (ADR-0033), newest first.
pub(super) fn feed_view(queue: &VecDeque<Notification>) -> Vec<Notification> {
    queue.iter().rev().take(NOTIFICATION_FEED_VIEW).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::text;
    use super::*;

    #[test]
    fn resolve_expiry_critical_never_expires_regardless_of_timeout() {
        assert_eq!(resolve_expiry(Urgency::Critical, 1000), None);
        assert_eq!(resolve_expiry(Urgency::Critical, 0), None);
        assert_eq!(resolve_expiry(Urgency::Critical, -1), None);
    }

    #[test]
    fn resolve_expiry_zero_means_never_for_non_critical() {
        assert_eq!(resolve_expiry(Urgency::Normal, 0), None);
        assert_eq!(resolve_expiry(Urgency::Low, 0), None);
    }

    #[test]
    fn resolve_expiry_negative_one_uses_the_server_default() {
        assert_eq!(resolve_expiry(Urgency::Normal, -1), Some(Duration::from_millis(DEFAULT_EXPIRE_MS)));
    }

    #[test]
    fn resolve_expiry_positive_uses_that_many_milliseconds() {
        assert_eq!(resolve_expiry(Urgency::Normal, 2500), Some(Duration::from_millis(2500)));
        assert_eq!(resolve_expiry(Urgency::Low, 100), Some(Duration::from_millis(100)));
    }

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

    #[test]
    fn resolve_sound_path_suppress_always_wins_to_none() {
        let client = Some(PathBuf::from("/usr/share/sounds/client.wav"));
        let tier = Some(PathBuf::from("/usr/share/sounds/tier.wav"));
        assert_eq!(
            resolve_sound_path(true, client.clone(), tier.clone()),
            None,
            "suppress-sound must force silence even with both a client file and a tier default present"
        );
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

    fn sample_notification(id: u32, image_path: Option<&str>) -> Notification {
        Notification {
            id,
            timestamp: 0,
            app_name: "app".to_string(),
            summary: "summary".to_string(),
            body: vec![text("body", false, false, false, None)],
            image_path: image_path.map(str::to_string),
            app_icon: None,
            urgency: Urgency::Normal,
            expired: false,
            transient: false,
            desktop_entry: None,
            has_reply: false,
            reply_placeholder: None,
            actions: Vec::new(),
            has_default_action: false,
            resident: false,
            incarnation: 0,
        }
    }

    #[test]
    fn expiring_an_ordinary_notification_retires_it_and_keeps_it_in_the_queue() {
        let mut queue = VecDeque::from([sample_notification(1, Some("/dev/shm/x/notif-1.png"))]);
        assert_eq!(expire_entry(&mut queue, 1, 0), Some(Expiry::Retired));
        assert_eq!(queue.len(), 1, "expiry is not removal any more");
        assert!(queue[0].expired);
        assert_eq!(queue[0].image_path.as_deref(), Some("/dev/shm/x/notif-1.png"), "the history still draws it");
    }

    #[test]
    fn expiring_a_transient_notification_removes_it_outright() {
        let mut transient = sample_notification(1, Some("/dev/shm/x/notif-1.png"));
        transient.transient = true;
        let mut queue = VecDeque::from([transient]);
        assert_eq!(
            expire_entry(&mut queue, 1, 0),
            Some(Expiry::Removed { image_path: Some("/dev/shm/x/notif-1.png".to_string()) })
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn a_stale_or_repeated_expiry_does_nothing() {
        let mut queue = VecDeque::from([sample_notification(1, None)]);
        queue[0].incarnation = 5;
        assert_eq!(expire_entry(&mut queue, 1, 4), None, "an older incarnation's timer");
        assert_eq!(expire_entry(&mut queue, 1, 5), Some(Expiry::Retired));
        assert_eq!(expire_entry(&mut queue, 1, 5), None, "already expired: nothing to announce twice");
        assert_eq!(expire_entry(&mut queue, 2, 0), None, "no such id");
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
        // The 101st insert evicts id 0. Finding 3 requires returning its id even without an icon,
        // so the caller can emit NotificationClosed(0, reason=Evicted).
        assert_eq!(evicted, Some(QueueCleanup::Evicted { id: 0, image_path: None }));
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
        assert_eq!(evicted, Some(QueueCleanup::Evicted { id: 0, image_path: Some("/tmp/oldest.png".to_string()) }));
    }

    #[test]
    fn resolve_replacement_icon_clears_and_deletes_the_old_icon_when_no_fresh_image_supplied() {
        let (resolved, to_delete) = resolve_replacement_image(Some("/tmp/old.png".to_string()), None);
        assert_eq!(resolved, None);
        assert_eq!(to_delete, Some("/tmp/old.png".to_string()));
    }

    #[test]
    fn resolve_replacement_icon_deletes_the_old_icon_when_a_different_fresh_image_is_supplied() {
        let (resolved, to_delete) =
            resolve_replacement_image(Some("/tmp/old.png".to_string()), Some("/tmp/new.png".to_string()));
        assert_eq!(resolved, Some("/tmp/new.png".to_string()));
        assert_eq!(to_delete, Some("/tmp/old.png".to_string()));
    }

    #[test]
    fn resolve_replacement_icon_deletes_nothing_when_the_same_path_is_reused() {
        let (resolved, to_delete) =
            resolve_replacement_image(Some("/tmp/same.png".to_string()), Some("/tmp/same.png".to_string()));
        assert_eq!(resolved, Some("/tmp/same.png".to_string()));
        assert_eq!(to_delete, None);
    }

    #[test]
    fn resolve_replacement_icon_no_previous_icon_just_uses_the_fresh_one() {
        let (resolved, to_delete) = resolve_replacement_image(None, Some("/tmp/new.png".to_string()));
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

        assert_eq!(
            cleanup,
            Some(QueueCleanup::ReplacedImage("/tmp/old.png".to_string())),
            "a same-id replace must never report an Evicted cleanup"
        );
        assert_eq!(queue.len(), 2, "a replace must not grow the queue");
        assert_eq!(queue[0].summary, "updated");
        assert_eq!(queue[0].image_path, None);
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
        // Missing id falls back to push_new, which evicts id 0 at the cap.
        let cleanup = replace_or_push(&mut queue, sample_notification(9999, None));
        assert_eq!(cleanup, Some(QueueCleanup::Evicted { id: 0, image_path: None }));
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

    /// Finding 2's race: after `replaces_id=7` moves incarnation 1 to 2, timer A must not remove
    /// the replacement 29 seconds early; timer B must still expire incarnation 2 normally.
    #[test]
    fn find_expiring_entry_ignores_a_stale_timer_superseded_by_a_replace() {
        let mut queue = VecDeque::new();
        let mut notification = sample_notification(7, None);
        notification.incarnation = 1;
        queue.push_back(notification);

        // Replace in place: id stays queued while its incarnation advances.
        let mut replacement = sample_notification(7, None);
        replacement.incarnation = 2;
        queue[0] = replacement;

        // Timer A's incarnation is superseded.
        assert_eq!(
            find_expiring_entry(&queue, 7, 1),
            None,
            "a stale timer for the pre-replace incarnation must be a no-op"
        );
        // Timer B carries the replacement's incarnation.
        assert_eq!(
            find_expiring_entry(&queue, 7, 2),
            Some(0),
            "the current timer for the post-replace incarnation must still fire normally"
        );
    }
}
