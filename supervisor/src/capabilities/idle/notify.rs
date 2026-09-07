//! Notify half of `oblisk.idle` (ADR-0032): `ext_idle_notifier_v1` on the Supervisor's dedicated
//! Wayland connection, with one listener per distinct threshold and a dispatch thread.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;
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

/// Registers `generation_id` for `sec`; returns whether a new listener is needed (ADR-0032: one
/// per duration). No dedup: repeated `sec` entries are distinct Lua callback pairs, so lists are
/// multisets.
pub fn register_threshold_entry(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32, sec: u64) -> bool {
    let duration = Duration::from_secs(sec);
    let created_new_listener = !fanout.contains_key(&duration);
    fanout.entry(duration).or_default().push(generation_id);
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
    raw_events_tx: UnboundedSender<(Duration, shared::IdleState)>,
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

/// Raw, not-yet-fanned-out `(Duration, IdleState)` events from [`connect_wayland_idle`], one per
/// `idled`/`resumed` on a live listener.
pub(crate) type RawIdleEventReceiver = UnboundedReceiver<(Duration, shared::IdleState)>;

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

    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=1, ())?;
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

/// Expands each raw `(Duration, IdleState)` into one [`shared::IdleEvent`] per registered
/// `generation_id` (ADR-0032), forwarding them to `main.rs`'s `select!` channel.
pub(crate) fn spawn_idle_event_forwarder(
    registry: Arc<Mutex<NotifyRegistry>>,
    gate: Arc<Mutex<super::gate::IdleGate>>,
    mut raw_events_rx: RawIdleEventReceiver,
    events_tx: UnboundedSender<shared::IdleEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((duration, state)) = raw_events_rx.recv().await {
            let generation_ids = registry.lock().unwrap().fanout.get(&duration).cloned().unwrap_or_default();
            for generation_id in generation_ids {
                let event = shared::IdleEvent { generation_id, threshold_sec: duration.as_secs(), state };
                // Gate after fan-out: it must see each pair to know which `Resumed` events it owes
                // when an inhibitor arrives (ADR-0139).
                let Some(event) = gate.lock().unwrap().observe(event) else { continue };
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

    #[test]
    fn register_threshold_entry_does_not_dedupe_a_generations_own_repeated_registration() {
        let mut fanout = HashMap::new();
        let first_created = register_threshold_entry(&mut fanout, 1, 30);
        let second_created = register_threshold_entry(&mut fanout, 1, 30);

        assert!(first_created);
        assert!(!second_created);
        assert_eq!(
            fanout.get(&Duration::from_secs(30)),
            Some(&vec![1, 1]),
            "a generation's own repeated registration must append, not dedupe"
        );
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
