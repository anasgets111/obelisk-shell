//! Notify half of `oblisk.idle` (ADR-0032): `ext_idle_notifier_v1` on the Supervisor's own
//! dedicated Wayland connection -- the fan-out registry (one listener per distinct threshold
//! duration) and the dispatch thread/connection setup itself.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;


/// Registers `generation_id` against `sec`'s duration in `fanout`; returns whether a new
/// `ext_idle_notification_v1` listener is needed for that duration (docs/adr/0032: one listener
/// per distinct duration). No dedup: the same `sec` registered twice appends twice -- each
/// registration is its own Lua-side callback pairing, so `fanout`'s lists are multisets.
pub fn register_threshold_entry(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32, sec: u64) -> bool {
    let duration = Duration::from_secs(sec);
    let created_new_listener = !fanout.contains_key(&duration);
    fanout.entry(duration).or_default().push(generation_id);
    created_new_listener
}

/// Drops every fan-out entry belonging to `generation_id` (the notify half of
/// `reset_registrations`, ADR-0006/ADR-0032). Leaves durations themselves untouched, even when
/// now empty -- the listener stays alive until a fresh registration reuses it.
pub fn cleanup_generation_thresholds(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32) {
    for entries in fanout.values_mut() {
        entries.retain(|&id| id != generation_id);
    }
}



/// Dispatch target for the Supervisor's own, separate Wayland connection (ADR-0010, survives a
/// Renderer crash or reload). Holds only the raw-event forwarding channel -- other state
/// (fan-out registry, bound proxies) lives on the async side, reachable from [`IdleController`]
/// directly, since Wayland proxies are `Send` (ADR-0032).
pub(crate) struct WaylandThreadState {
    raw_events_tx: UnboundedSender<(Duration, shared::IdleState)>,
}

/// Required by [`registry_queue_init`]. The global lookup happens once, via `GlobalList::bind`
/// right after init (see [`connect_wayland_idle`]); later registry events are never acted on.
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

/// `wl_seat` is bound only so `get_idle_notification` has an object to name -- this feature
/// never creates a pointer/keyboard/touch object from it, so every seat event is dropped.
impl Dispatch<WlSeat, ()> for WaylandThreadState {
    fn event(_state: &mut Self, _proxy: &WlSeat, _event: wl_seat::Event, _data: &(), _conn: &Connection, _qhandle: &QueueHandle<Self>) {}
}

/// `ext_idle_notifier_v1` has no `<event>` in its protocol XML -- this can never actually fire;
/// kept only because `Dispatch` must be implemented for every proxy type an object is created for.
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

/// Forwards `idled`/`resumed` straight to the async side, tagged with the `Duration` this
/// listener was created for (this proxy's user-data, set at `get_idle_notification` time --
/// see [`IdleController::register_threshold`]). A dropped receiver is not logged.
impl Dispatch<ExtIdleNotificationV1, Duration> for WaylandThreadState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        data: &Duration,
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
    pub(crate) listeners: HashMap<Duration, ExtIdleNotificationV1>,
}

pub(crate) struct LiveNotify {
    pub(crate) notifier: ExtIdleNotifierV1,
    pub(crate) seat: WlSeat,
    pub(crate) connection: Connection,
    pub(crate) queue_handle: QueueHandle<WaylandThreadState>,
    pub(crate) registry: Arc<Mutex<NotifyRegistry>>,
    /// Kept alive for the controller's whole lifetime -- dropping it would stop the dispatch
    /// thread. Never joined on shutdown.
    #[allow(dead_code)]
    dispatch_thread: std::thread::JoinHandle<()>,
}

pub(crate) enum NotifyState {
    Live(LiveNotify),
    /// `ext_idle_notifier_v1`/`wl_seat` weren't advertised, or the dedicated Wayland connection
    /// failed to establish -- degrade to inert, don't take the Supervisor down (ADR-0032).
    Inert,
}

/// The channel [`connect_wayland_idle`] hands back: raw, not-yet-fanned-out `(Duration,
/// IdleState)` events, one per fired `idled`/`resumed` on any live listener.
pub(crate) type RawIdleEventReceiver = UnboundedReceiver<(Duration, shared::IdleState)>;

/// Establishes the Supervisor's own, separate Wayland connection (ADR-0010), binds `wl_seat`
/// and `ext_idle_notifier_v1` at version 1, and spawns the dedicated dispatch thread
/// (`wayland-client` 0.31's `blocking_dispatch` cannot run on the tokio executor -- ADR-0032).
/// Returns the raw event receiver so the caller can wire up fan-out expansion against its own
/// registry.
///
/// Genuinely blocking top to bottom -- never call this inline on the tokio executor;
/// [`IdleController::new`] runs it inside `tokio::task::spawn_blocking`, bounded by
/// [`IDLE_NOTIFY_SETUP_TIMEOUT`]. The error type is `Send + Sync` so the `Result` can cross
/// that boundary.
pub(crate) fn connect_wayland_idle() -> Result<(LiveNotify, RawIdleEventReceiver), Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<WaylandThreadState>(&connection)?;
    let qh = event_queue.handle();

    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=1, ())?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;

    let (raw_events_tx, raw_events_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WaylandThreadState { raw_events_tx };

    // One roundtrip so the freshly-bound proxies are fully live before this function hands them back.
    event_queue.roundtrip(&mut state)?;

    let dispatch_thread = std::thread::spawn(move || {
        loop {
            if event_queue.blocking_dispatch(&mut state).is_err() {
                // connection died (compositor exited, socket closed); exit quietly rather than spin
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

/// Drains `raw_events_rx`, expanding each raw `(Duration, IdleState)` event into one
/// [`shared::IdleEvent`] per `generation_id` registered against that duration (ADR-0032's
/// fan-out), forwarding each to `events_tx` (drained by `main.rs`'s `select!` loop).
pub(crate) fn spawn_idle_event_forwarder(registry: Arc<Mutex<NotifyRegistry>>, mut raw_events_rx: RawIdleEventReceiver, events_tx: UnboundedSender<shared::IdleEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((duration, state)) = raw_events_rx.recv().await {
            let generation_ids = registry.lock().unwrap().fanout.get(&duration).cloned().unwrap_or_default();
            for generation_id in generation_ids {
                let event = shared::IdleEvent { generation_id, threshold_sec: duration.as_secs(), state };
                if events_tx.send(event).is_err() {
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
        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![1, 2]), "both generations must appear in the same duration's fan-out list");
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

    #[test]
    fn register_threshold_entry_does_not_dedupe_a_generations_own_repeated_registration() {
        let mut fanout = HashMap::new();
        let first_created = register_threshold_entry(&mut fanout, 1, 30);
        let second_created = register_threshold_entry(&mut fanout, 1, 30);

        assert!(first_created);
        assert!(!second_created);
        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![1, 1]), "a generation's own repeated registration must append, not dedupe");
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
        assert_eq!(fanout.get(&Duration::from_secs(60)), Some(&vec![]), "generation 1's only entry at 60s must be dropped");
    }

    #[test]
    fn cleanup_generation_thresholds_is_a_no_op_for_an_unregistered_generation() {
        let mut fanout = HashMap::new();
        register_threshold_entry(&mut fanout, 1, 30);

        cleanup_generation_thresholds(&mut fanout, 99);

        assert_eq!(fanout.get(&Duration::from_secs(30)), Some(&vec![1]));
    }

}
