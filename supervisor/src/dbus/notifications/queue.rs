//! Urgency/critical-bypass expiry logic, DND sound-gating logic, and queue mutation (FIFO
//! eviction, replace/icon lifecycle). Split from `dbus::notifications` -- see
//! `dbus/notifications/mod.rs` for the module-level doc.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use super::{DEFAULT_EXPIRE_MS, NOTIFICATION_FEED_VIEW, NOTIFICATION_QUEUE_CAP, Notification, Urgency};

// -------------------------------------------------------------------------------------------
// Urgency / critical-bypass expiry logic (TDD seam 5).
// -------------------------------------------------------------------------------------------

/// Whether, and after how long, a notification auto-expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExpiryPolicy {
    Never,
    After(Duration),
}

/// `expire_timeout`'s resolution (docs/oblisk-supervisor-services-dbus.md §1; ADR-0033's
/// "Critical urgency ignores `expire_timeout`" policy fix): critical never expires regardless of
/// what the sender requested; otherwise `0` means never, a negative value means "use the server
/// default" ([`DEFAULT_EXPIRE_MS`], matching mako/dunst), and a positive value is that many
/// milliseconds.
pub(super) fn resolve_expiry(urgency: Urgency, expire_timeout: i32) -> ExpiryPolicy {
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
pub(super) fn should_play_sound(dnd: bool, urgency: Urgency, sound_registered: bool) -> bool {
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
pub(super) fn resolve_sound_path(suppress: bool, client_sound_file: Option<PathBuf>, tier_default: Option<PathBuf>) -> Option<PathBuf> {
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
pub(super) fn next_incarnation(counter: &mut u64) -> u64 {
    let value = *counter;
    *counter = counter.wrapping_add(1);
    value
}

/// `replaces_id == 0`'s id half: allocates the next id, monotonic and wrap-safe (never lands on
/// `0`, which is reserved to mean "new" on the wire). `replaces_id != 0` passes through unchanged
/// without bumping the allocator (base spec/ADR-0033: id reuse doesn't consume a fresh id).
pub(super) fn resolve_notification_id(replaces_id: u32, next_id: &mut u32) -> u32 {
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
pub(super) enum QueueCleanup {
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
pub(super) fn replace_or_push(queue: &mut VecDeque<Notification>, mut notification: Notification) -> Option<QueueCleanup> {
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
pub(super) fn find_expiring_entry(queue: &VecDeque<Notification>, id: u32, incarnation: u64) -> Option<usize> {
    queue.iter().position(|entry| entry.id == id && entry.incarnation == incarnation)
}

/// `dismiss(id)`/`reply(id, ...)`/`CloseNotification(id)`'s shared removal: pulls the matching
/// entry out of the queue entirely (not just clearing a field), for the caller to delete its icon
/// file (if any) and emit the appropriate `NotificationClosed`/`ActionInvoked` signal.
pub(super) fn remove_by_id(queue: &mut VecDeque<Notification>, id: u32) -> Option<Notification> {
    let index = queue.iter().position(|entry| entry.id == id)?;
    queue.remove(index)
}

/// `notifications.feed`'s truncated view (ADR-0033): the newest [`NOTIFICATION_FEED_VIEW`] entries,
/// most recent first.
pub(super) fn feed_view(queue: &VecDeque<Notification>) -> Vec<Notification> {
    queue.iter().rev().take(NOTIFICATION_FEED_VIEW).cloned().collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::text;

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
}
