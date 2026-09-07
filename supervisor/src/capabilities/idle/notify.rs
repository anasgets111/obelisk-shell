//! Notify half of `oblisk.idle` (ADR-0032): `ext_idle_notifier_v1` on the Supervisor's dedicated
//! Wayland connection, with one listener per distinct threshold and a dispatch thread.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// Which listener an event came from (ADR-0160).
///
/// A duration has two. The compositor withholds `get_idle_notification` while a Wayland client
/// holds a surface idle inhibitor, and never withholds `get_input_idle_notification`. Comparing
/// them is the only way to see such a holder: no protocol lists them, and logind's
/// `BlockInhibited` does not cover them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ListenerId {
    pub(crate) duration: Duration,
    /// `get_idle_notification`; false for the `get_input_idle_notification` twin.
    pub(crate) respects_inhibitors: bool,
}

/// Whether the compositor is withholding idle notifications (ADR-0160). The seat is idle, and the
/// listener that honours inhibitors was not told.
///
/// Both sets hold raw compositor answers, taken before the logind gate. This names the withholding
/// and no holder. A surface inhibitor is the usual cause, not the only one: niri also folds in its
/// freedesktop screensaver flag, sway adds configured focus and fullscreen policy, and an ordinary
/// notification may weigh a presence sensor that the input twin ignores.
///
/// Only the shortest fired threshold votes. Releasing an inhibitor restarts the gated timers, so a
/// seat idle at 1s and 300s gets the 1s listener back a second later and the 300s one five minutes
/// later. Any-of read that gap as a still-held inhibitor for the whole five minutes.
///
/// `None` means no evidence, which is whenever the seat is in use. The caller keeps its last
/// answer, so the published one has no staleness bound. Only a fresh idle period replaces it.
pub(crate) fn wayland_inhibited(input_idle: &HashSet<Duration>, gated_idle: &HashSet<Duration>) -> Option<bool> {
    input_idle.iter().min().map(|shortest| !gated_idle.contains(shortest))
}

/// Registers `generation_id` for `sec`; returns whether a new listener is needed (ADR-0032: one
/// per duration).
///
/// One entry per generation, not per registration. This list is an event's destinations, and
/// `lua::idle::IdleRegistry::dispatch_event` already runs every callback registered at that
/// duration. A second entry therefore sent a second event that ran every callback again, so two
/// `register_threshold(300, ...)` calls fired four times.
pub fn register_threshold_entry(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32, sec: u64) -> bool {
    let duration = Duration::from_secs(sec);
    let created_new_listener = !fanout.contains_key(&duration);
    let entries = fanout.entry(duration).or_default();
    if !entries.contains(&generation_id) {
        entries.push(generation_id);
    }
    created_new_listener
}

/// Drops every `generation_id` entry (notify half of `reset_registrations`, ADR-0006/ADR-0032),
/// but leaves durations and empty listeners alive for reuse.
pub fn cleanup_generation_thresholds(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32) {
    for entries in fanout.values_mut() {
        entries.retain(|&id| id != generation_id);
    }
}

/// Dispatch target for the separate Wayland connection (ADR-0010, survives Renderer crash/reload).
/// Holds only the raw-event channel; fan-out state and bound `Send` proxies stay on the async side
/// via [`IdleController`] (ADR-0032).
pub(crate) struct WaylandThreadState {
    raw_events_tx: UnboundedSender<(ListenerId, shared::IdleState)>,
}

/// Required by [`registry_queue_init`]. `GlobalList::bind` performs the only lookup after init;
/// later registry events are ignored (see [`connect_wayland_idle`]).
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandThreadState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

/// Bind `wl_seat` only to name `get_idle_notification`; never create pointer/keyboard/touch
/// objects, so seat events are dropped.
impl Dispatch<WlSeat, ()> for WaylandThreadState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

/// `ext_idle_notifier_v1` has no `<event>` in its XML, so this cannot fire. It remains because
/// `Dispatch` is required for every created proxy type.
impl Dispatch<ExtIdleNotifierV1, ()> for WaylandThreadState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!("ext_idle_notifier_v1 (version 1) has no events to dispatch")
    }
}

/// Forwards `idled`/`resumed` with the listener's creation `Duration` from proxy user-data (set by
/// [`IdleController::register_threshold`]). A dropped receiver is not logged.
impl Dispatch<ExtIdleNotificationV1, ListenerId> for WaylandThreadState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        data: &ListenerId,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let mapped = match event {
            ext_idle_notification_v1::Event::Idled => Some(shared::IdleState::Idled),
            ext_idle_notification_v1::Event::Resumed => Some(shared::IdleState::Resumed),
            _ => None,
        };
        if let Some(state_value) = mapped {
            let _ = state.raw_events_tx.send((*data, state_value));
        }
    }
}

#[derive(Default)]
pub(crate) struct NotifyRegistry {
    pub(crate) fanout: HashMap<Duration, Vec<u32>>,
    pub(crate) listeners: HashMap<ListenerId, ExtIdleNotificationV1>,
}

pub(crate) struct LiveNotify {
    pub(crate) notifier: ExtIdleNotifierV1,
    pub(crate) seat: WlSeat,
    pub(crate) connection: Connection,
    pub(crate) queue_handle: QueueHandle<WaylandThreadState>,
    pub(crate) registry: Arc<Mutex<NotifyRegistry>>,
    /// Keeps the dispatch thread alive; never joined on shutdown.
    #[allow(dead_code)]
    dispatch_thread: std::thread::JoinHandle<()>,
}

pub(crate) enum NotifyState {
    Live(LiveNotify),
    /// Protocols were absent or the dedicated connection failed; degrade to inert, not Supervisor
    /// failure (ADR-0032).
    Inert,
}

/// Raw, not-yet-fanned-out `(ListenerId, IdleState)` events from [`connect_wayland_idle`], one per
/// `idled`/`resumed` on a live listener.
pub(crate) type RawIdleEventReceiver = UnboundedReceiver<(ListenerId, shared::IdleState)>;

/// Establishes the separate Wayland connection (ADR-0010), binds `wl_seat` and
/// `ext_idle_notifier_v1` at version 1, and spawns its dispatch thread. `wayland-client` 0.31's
/// `blocking_dispatch` cannot run on tokio (ADR-0032). Returns the raw receiver for fan-out.
///
/// Blocking throughout: call only from [`IdleController::new`] inside `spawn_blocking`, bounded by
/// [`IDLE_NOTIFY_SETUP_TIMEOUT`]. Its error is `Send + Sync` across that boundary.
pub(crate) fn connect_wayland_idle()
-> Result<(LiveNotify, RawIdleEventReceiver), Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<WaylandThreadState>(&connection)?;
    let qh = event_queue.handle();

    // 1..=2 rather than 1..=1: version 2 adds `get_input_idle_notification`, and a version 1
    // compositor still binds, just without inhibitor detection (ADR-0160).
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=2, ())?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;

    let (raw_events_tx, raw_events_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WaylandThreadState { raw_events_tx };

    // Roundtrip so freshly bound proxies are live before returning them.
    event_queue.roundtrip(&mut state)?;

    let dispatch_thread = std::thread::spawn(move || {
        loop {
            if event_queue.blocking_dispatch(&mut state).is_err() {
                // Compositor/socket died; exit quietly rather than spin.
                break;
            }
        }
    });

    let live = LiveNotify {
        notifier,
        seat,
        connection,
        queue_handle: qh,
        registry: Arc::new(Mutex::new(NotifyRegistry::default())),
        dispatch_thread,
    };
    Ok((live, raw_events_rx))
}

/// Sends one [`shared::IdleEvent`] per registered `generation_id` for each gated event (ADR-0032),
/// and tracks both listeners of each pair to answer [`wayland_inhibited`] (ADR-0160).
///
/// Input-idle events never fan out. They exist to tell the compositor's silence apart from a seat
/// that is not idle.
pub(crate) fn spawn_idle_event_forwarder(
    registry: Arc<Mutex<NotifyRegistry>>,
    gate: Arc<Mutex<super::gate::IdleGate>>,
    published: Arc<Mutex<super::controller::PublishedIdle>>,
    mut raw_events_rx: RawIdleEventReceiver,
    events_tx: UnboundedSender<shared::IdleEvent>,
    state_tx: UnboundedSender<super::IdleState>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut input_idle: HashSet<Duration> = HashSet::new();
        let mut gated_idle: HashSet<Duration> = HashSet::new();
        while let Some(first) = raw_events_rx.recv().await {
            // Drain what already arrived, so both halves of a pair are usually read together
            // rather than showing "inhibited" in the gap between them. This narrows the window
            // without closing it. An mpsc channel is not a compositor batch, so the consumer can
            // run between the producer's two sends, and two timers can expire separately.
            let batch = std::iter::once(first).chain(std::iter::from_fn(|| raw_events_rx.try_recv().ok()));

            for (listener, state) in batch {
                match state {
                    shared::IdleState::Idled => {
                        let seen = if listener.respects_inhibitors { &mut gated_idle } else { &mut input_idle };
                        seen.insert(listener.duration);
                    }
                    // Both sets, whichever listener said it. A resume means the seat is in use, so
                    // idle evidence for that duration is stale either way. Clearing only the
                    // reporting half made the answer depend on read order: a gated `Resumed` alone
                    // left the input half idle, which reads as a held inhibitor, and the input
                    // `Resumed` behind it is no evidence and preserved that false positive for as
                    // long as the seat stayed busy.
                    shared::IdleState::Resumed => {
                        gated_idle.remove(&listener.duration);
                        input_idle.remove(&listener.duration);
                    }
                }
                if !listener.respects_inhibitors {
                    continue;
                }
                let generation_ids =
                    registry.lock().unwrap().fanout.get(&listener.duration).cloned().unwrap_or_default();
                for generation_id in generation_ids {
                    let event = shared::IdleEvent { generation_id, threshold_sec: listener.duration.as_secs(), state };
                    // Gate after fan-out: it must see each pair to know which `Resumed` events it
                    // owes when an inhibitor arrives (ADR-0139).
                    let Some(event) = gate.lock().unwrap().observe(event) else { continue };
                    if events_tx.send(event).is_err() {
                        return;
                    }
                }
            }

            let answer = wayland_inhibited(&input_idle, &gated_idle);
            // Held across the send, not just the settle. `state_tx` is unbounded, so this never
            // blocks. Releasing first let the other writer settle and send between our two steps,
            // so the older payload arrived last with `last_sent` already past it. A subscriber
            // then kept a stale answer that no later unchanged observation could repair.
            let mut published = published.lock().unwrap();
            if let Some(next) = published.set_wayland_inhibited(answer) {
                // Say it, like the logind gate does. Nothing lists the holders, so a user asking
                // "why will this not lock" has only this line and the widget. Report the
                // compositor's own answer. The payload's `inhibited` is merged with logind's, and
                // would claim the compositor was withholding when only logind was.
                if let Some(held) = answer {
                    eprintln!(
                        "idle: the compositor {} idle notifications; a surface idle inhibitor is the usual reason",
                        if held { "is holding off" } else { "is sending" }
                    );
                }
                if state_tx.send(next).is_err() {
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- register_threshold_entry (threshold fan-out decision, TDD seam 1) ----

    #[test]
    fn register_threshold_entry_creates_a_new_listener_for_the_first_registration() {
        let mut fanout = HashMap::new();
        let created = register_threshold_entry(&mut fanout, 1, 30);
        assert!(created);
        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![1]));
    }

    #[test]
    fn register_threshold_entry_shares_one_listener_across_generations_registering_the_same_sec() {
        let mut fanout = HashMap::new();
        let first_created = register_threshold_entry(&mut fanout, 1, 30);
        let second_created = register_threshold_entry(&mut fanout, 2, 30);

        assert!(first_created);
        assert!(!second_created, "the second registration at the same duration must not need a new listener");
        assert_eq!(
            fanout.get(&Duration::from_secs(30)),
            Some(&vec![1, 2]),
            "both generations must appear in the same duration's fan-out list"
        );
    }

    #[test]
    fn register_threshold_entry_creates_a_distinct_listener_per_distinct_sec() {
        let mut fanout = HashMap::new();
        let thirty_created = register_threshold_entry(&mut fanout, 1, 30);
        let sixty_created = register_threshold_entry(&mut fanout, 1, 60);

        assert!(thirty_created);
        assert!(sixty_created, "a different duration must always need its own listener");
        assert_eq!(fanout.len(), 2);
    }

    /// This list is destinations, not registrations. The Renderer runs every callback it holds at
    /// a duration for each event, so a second entry for one generation ran every callback twice.
    /// Two `register_threshold(300, ...)` calls fired four times. The earlier version of this test
    /// asserted the duplicate as correct: each half looked right alone, only the pair was wrong.
    #[test]
    fn a_generations_repeated_registration_adds_no_second_destination() {
        let mut fanout = HashMap::new();
        let first_created = register_threshold_entry(&mut fanout, 1, 30);
        let second_created = register_threshold_entry(&mut fanout, 1, 30);

        assert!(first_created);
        assert!(!second_created);
        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![1]));
    }

    // ---- wayland_inhibited (ADR-0160 detection seam) ----

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// The whole point: the seat is idle, and the listener that honours inhibitors was not told.
    #[test]
    fn a_threshold_the_input_listener_reached_and_the_gated_one_did_not_is_an_inhibitor() {
        let input = HashSet::from([secs(1)]);
        assert_eq!(wayland_inhibited(&input, &HashSet::new()), Some(true));
    }

    #[test]
    fn a_threshold_both_listeners_reached_is_an_ordinary_idle_seat() {
        let both = HashSet::from([secs(1)]);
        assert_eq!(wayland_inhibited(&both, &both), Some(false));
    }

    /// The divergence exists only while the seat is idle, so an active seat is no evidence either
    /// way. A widget that flips to "nothing is holding this awake" on every keystroke is worse than
    /// one briefly stale.
    #[test]
    fn an_active_seat_is_no_evidence_rather_than_evidence_of_nothing_held() {
        assert_eq!(wayland_inhibited(&HashSet::new(), &HashSet::new()), None);
    }

    /// The shortest fired threshold is the whole answer. Releasing an inhibitor restarts the gated
    /// timers, so a seat idle at 1s and 300s gets its 1s listener back a second later and its 300s
    /// one five minutes later. Any-of held a released inhibitor true for those five minutes, and an
    /// earlier version of this test asserted that as correct.
    #[test]
    fn the_shortest_fired_threshold_answers_and_a_slower_one_does_not_outvote_it() {
        let input = HashSet::from([secs(1), secs(300)]);
        assert_eq!(
            wayland_inhibited(&input, &HashSet::from([secs(1)])),
            Some(false),
            "the 1s listener came back, so the inhibitor is gone however long the 300s one stays quiet"
        );
        assert_eq!(
            wayland_inhibited(&input, &HashSet::from([secs(300)])),
            Some(true),
            "and a shortest that is still withheld is still evidence"
        );
    }

    /// The resume half, which the first version got wrong in a way no single-event test could show.
    /// Waking clears both listeners. Clearing only the one that reported left the other's stale
    /// idle behind, which reads as a held inhibitor, and the second `Resumed` is no evidence and
    /// preserves whatever it finds. The false positive then lasted as long as the seat stayed busy.
    #[test]
    fn waking_clears_both_halves_whichever_one_reports_first() {
        for gated_first in [true, false] {
            let mut input = HashSet::from([secs(1)]);
            let mut gated = HashSet::from([secs(1)]);
            let order = if gated_first { [true, false] } else { [false, true] };
            for respects_inhibitors in order {
                // What the forwarder does for a `Resumed`, in one order and then the other.
                let _ = respects_inhibitors;
                gated.remove(&secs(1));
                input.remove(&secs(1));
                assert_ne!(
                    wayland_inhibited(&input, &gated),
                    Some(true),
                    "waking must never read as an inhibitor, in either order"
                );
            }
        }
    }

    // ---- cleanup_generation_thresholds (TDD seam 5, notify half) ----

    #[test]
    fn cleanup_generation_thresholds_drops_only_the_named_generations_entries() {
        let mut fanout = HashMap::new();
        register_threshold_entry(&mut fanout, 1, 30);
        register_threshold_entry(&mut fanout, 2, 30);
        register_threshold_entry(&mut fanout, 1, 60);

        cleanup_generation_thresholds(&mut fanout, 1);

        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![2]), "generation 2's entry at 30s must survive");
        assert_eq!(
            fanout.get(&Duration::from_secs(60)),
            Some(&vec![]),
            "generation 1's only entry at 60s must be dropped"
        );
    }

    /// ADR-0158's ordering, as the two seams see it. A reload cleans the generation out, then its
    /// new tree registers again. Reversing these two lines is the bug: the entry is added and then
    /// deleted, and the config hears nothing for the rest of that generation's life.
    #[test]
    fn a_generation_that_registers_after_its_cleanup_is_listening_again() {
        let mut fanout = HashMap::new();
        register_threshold_entry(&mut fanout, 1, 1);

        cleanup_generation_thresholds(&mut fanout, 1);
        let created_new_listener = register_threshold_entry(&mut fanout, 1, 1);

        assert_eq!(fanout.get(&Duration::from_secs(1)), Some(&vec![1]));
        assert!(!created_new_listener, "the emptied duration keeps its listener, so the reload creates no second one");
    }

    #[test]
    fn cleanup_generation_thresholds_is_a_no_op_for_an_unregistered_generation() {
        let mut fanout = HashMap::new();
        register_threshold_entry(&mut fanout, 1, 30);

        cleanup_generation_thresholds(&mut fanout, 99);

        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![1]));
    }
}
