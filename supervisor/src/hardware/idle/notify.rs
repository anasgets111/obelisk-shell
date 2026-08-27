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


/// Registers `generation_id` against `sec`'s duration in `fanout`, returning whether this is the
/// *first* registration for that exact duration (i.e. whether a real `ext_idle_notification_v1`
/// listener still needs to be created). Pure and synchronous -- the caller creates the real
/// Wayland object only when this returns `true` (docs/adr/0032: "one listener per distinct
/// duration value, not per call").
///
/// No dedup: a generation registering the same `sec` twice (from two Lua call sites, or the same
/// call site called twice) appends twice, not once -- ADR-0032's own framing ("Two Lua call sites
/// registering `30` both get called on the same listener's `idled`/`resumed` pair") extends to a
/// single call site double-registering the same duration: each call establishes its own Lua-side
/// callback pairing and both must fire, so the fan-out list is a multiset, not a set.
pub fn register_threshold_entry(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32, sec: u64) -> bool {
    let duration = Duration::from_secs(sec);
    let created_new_listener = !fanout.contains_key(&duration);
    fanout.entry(duration).or_default().push(generation_id);
    created_new_listener
}

/// Drops every fan-out entry belonging to `generation_id`, across every distinct threshold
/// duration -- the notify half of `reset_registrations` (ADR-0006/ADR-0032: reload or crash
/// cleanup). Leaves other generations' entries, and the durations themselves (including any now
/// empty), untouched: an empty duration's `ext_idle_notification_v1` listener is left alive
/// rather than torn down -- it simply has no destination left to fan out to until a fresh
/// registration reuses it, matching this codebase's "don't invent extra machinery" discipline
/// (YAGNI: no compositor-facing effect from a listener firing to nobody).
pub fn cleanup_generation_thresholds(fanout: &mut HashMap<Duration, Vec<u32>>, generation_id: u32) {
    for entries in fanout.values_mut() {
        entries.retain(|&id| id != generation_id);
    }
}



/// Dispatch target for the Supervisor's own, separate Wayland connection (ADR-0010's sibling:
/// lock authority already owns a dedicated connection for the same "survives a Renderer crash or
/// reload" reason). Holds nothing but the raw-event forwarding channel -- every other bit of
/// state this feature needs (the fan-out registry, the bound proxies) lives on the async side,
/// reachable from [`IdleController`] directly, since Wayland proxies/`Connection`/`QueueHandle`
/// are `Send` (docs/adr/0032). This thread's only job is pumping `idled`/`resumed` events back
/// out over `raw_events_tx`.
pub(crate) struct WaylandThreadState {
    raw_events_tx: UnboundedSender<(Duration, shared::IdleState)>,
}

/// Required by [`registry_queue_init`] -- this codebase's own registry-driven global lookup
/// happens once, synchronously, via `GlobalList::bind` right after init (see
/// [`connect_wayland_idle`]), so dynamic registry events after that point are never acted on.
/// Mirrors `TextInputManagerData`'s "acknowledged, not acted on" precedent in
/// `renderer/src/wayland/mod.rs` for a global this feature doesn't need to track dynamically.
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

/// `wl_seat` is bound only so `get_idle_notification` has an object to name -- this feature never
/// creates a pointer/keyboard/touch object from it, so every seat event is acknowledged and
/// dropped (mirrors `renderer/src/wayland/mod.rs`'s own `SeatHandler` impl, which does the same
/// for the same reason).
impl Dispatch<WlSeat, ()> for WaylandThreadState {
    fn event(_state: &mut Self, _proxy: &WlSeat, _event: wl_seat::Event, _data: &(), _conn: &Connection, _qhandle: &QueueHandle<Self>) {}
}

/// `ext_idle_notifier_v1` has no `<event>` in its protocol XML (only `destroy`/
/// `get_idle_notification`/`get_input_idle_notification` requests) -- this can never actually
/// fire; kept only because `Dispatch` must be implemented for every proxy type an object is
/// created for.
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

/// The real payload: forwards `idled`/`resumed` straight to the async side, tagged with the
/// `Duration` this particular listener was created for (this proxy's own user-data, set at
/// `get_idle_notification` time -- see [`IdleController::register_threshold`]). A dropped
/// receiver (the controller side has gone away, e.g. mid-shutdown) is not logged -- matches
/// every other best-effort forwarder in this codebase (`dbus::tray`'s signal forwarders).
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
    /// thread from ever being joined on shutdown, but nothing in this codebase joins Wayland
    /// dispatch threads on shutdown today (matches `audio::mixer::run`'s own detached
    /// `std::thread::spawn` in `main.rs`, which is never joined either).
    #[allow(dead_code)]
    dispatch_thread: std::thread::JoinHandle<()>,
}

pub(crate) enum NotifyState {
    Live(LiveNotify),
    /// `ext_idle_notifier_v1`/`wl_seat` weren't advertised, or the dedicated Wayland connection
    /// itself failed to establish -- same "degrade to inert, don't take the Supervisor down"
    /// precedent `TrayController::inert`/`BluetoothController::new` already established
    /// (ADR-0032).
    Inert,
}

/// The channel [`connect_wayland_idle`] hands back: raw, not-yet-fanned-out `(Duration,
/// IdleState)` events, one per fired `idled`/`resumed` on any live listener.
pub(crate) type RawIdleEventReceiver = UnboundedReceiver<(Duration, shared::IdleState)>;

/// Establishes the Supervisor's own, separate Wayland connection (ADR-0010's sibling to lock
/// authority), binds `wl_seat` and `ext_idle_notifier_v1` (bound at version 1 only for both --
/// version 1 is always within whatever the compositor and this crate's generated code both
/// support, and ADR-0032 explicitly rejects `get_input_idle_notification`, the only reason a
/// higher `ext_idle_notifier_v1` version would matter here), and spawns the dedicated dispatch
/// thread (`wayland-client` 0.31's `blocking_dispatch` cannot run on the tokio executor -- ADR-0032).
/// Returns the raw, not-yet-fanned-out event receiver so the caller can wire up fan-out
/// expansion against its own registry (see [`IdleController::new`]).
///
/// Genuinely blocking and synchronous top to bottom (`Connection::connect_to_env`, the registry
/// roundtrip inside `registry_queue_init`, and the explicit `roundtrip` below all make blocking
/// syscalls) -- never call this inline on the tokio executor; [`IdleController::new`] always runs
/// it inside `tokio::task::spawn_blocking`, bounded by [`IDLE_NOTIFY_SETUP_TIMEOUT`]. The error
/// type is `Send + Sync` (unlike a bare `Box<dyn Error>`) so the whole `Result` can cross that
/// `spawn_blocking` boundary.
pub(crate) fn connect_wayland_idle() -> Result<(LiveNotify, RawIdleEventReceiver), Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<WaylandThreadState>(&connection)?;
    let qh = event_queue.handle();

    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=1, ())?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;

    let (raw_events_tx, raw_events_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WaylandThreadState { raw_events_tx };

    // One roundtrip so the freshly-bound proxies are fully live before this function hands them
    // back -- mirrors `renderer/src/wayland/mod.rs::run`'s own post-bind roundtrip discipline.
    event_queue.roundtrip(&mut state)?;

    let dispatch_thread = std::thread::spawn(move || {
        loop {
            if event_queue.blocking_dispatch(&mut state).is_err() {
                // The connection died (compositor exited, socket closed) -- nothing left to
                // pump; exit quietly rather than spin. Matches `NotifyState::Inert`'s own
                // silent-degrade philosophy: a live idle capability isn't essential enough to
                // take the whole Supervisor down when it stops working mid-session either.
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

/// Drains `raw_events_rx` for the controller's whole lifetime, expanding each raw
/// `(Duration, IdleState)` event into one [`shared::IdleEvent`] per `generation_id` currently
/// registered against that duration (ADR-0032's fan-out) and forwarding each to `events_tx` --
/// the channel `main.rs`'s own `select!` loop drains (mirrors `dbus::tray`'s `TraySignal`
/// forwarding pattern). Builds the real wire type directly rather than an intermediate
/// `hardware::idle`-local signal type to relabel later (Standards review: unlike `TraySignal`/
/// `BluetoothSignal`, which are bare markers with no payload, a payload-carrying duplicate of
/// `shared::IdleEvent` would exist only to be copied field-for-field into it). Exits once either
/// side of the pipe closes.
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
        // ADR-0032's own framing ("two Lua call sites... both get called") applies just as much
        // to one call site registering the same sec twice -- each registration is its own
        // fan-out entry, not deduplicated by generation_id.
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
