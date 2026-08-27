//! Idle capability (`oblisk.idle`, docs/oblisk-supervisor-services-dbus.md §7;
//! docs/adr/0032). Splits transport -- `ext_idle_notifier_v1` on the Supervisor's own dedicated
//! Wayland connection for notify, `org.freedesktop.login1.Manager.Inhibit` on the existing
//! system-bus connection for inhibit -- but shares one controller and one generation-scoped
//! cleanup hook (ADR-0032's "Consequences": "share generation-scoped cleanup but not code
//! paths").
//!
//! Mirrors `dbus::tray`'s degrade-to-inert precedent for notify: if `ext_idle_notifier_v1` or
//! `wl_seat` isn't advertised, the dedicated Wayland connection itself fails to establish, or
//! setup doesn't finish within [`IDLE_NOTIFY_SETUP_TIMEOUT`] (a hung/misbehaving compositor --
//! observed live once against a real niri session as a genuine `roundtrip()` stall with every
//! thread parked, not a slow-but-progressing one), `register_threshold` becomes a silent no-op
//! (logged once when the outcome is known, not per call). That setup -- a genuinely blocking,
//! synchronous Wayland connect+roundtrip -- runs inside `tokio::task::spawn_blocking` (never
//! inline on the async executor, per this project's own async-hygiene rule: docs/build-steps.md
//! Phase 9) as its own background task kicked off from [`IdleController::new`], which returns
//! immediately with notify starting `Inert` and only swapping to `Live` if/when that task
//! actually succeeds -- so a slow or hung compositor can never delay `socket::spawn_listener`
//! (or anything else in `main.rs`'s boot sequence) behind idle-notify setup.
//!
//! Inhibit has no equivalent degrade path -- it rides the Supervisor's already-required system-bus
//! connection, so its only failure mode is the `Inhibit` call itself failing per-request (ADR-0032).
//! Its `Login1ManagerProxy` is therefore built fresh on every `inhibit()` call rather than cached
//! once at construction (a cached failure would freeze that per-request failure mode into a
//! permanent one), and its refcount decision, D-Bus call, and fd write are one atomic critical
//! section under a single `tokio::sync::Mutex` -- see [`IdleController::inhibit`]'s doc comment.
//!
//! Two pure, unit-testable seams carry the real decision logic, wrapped by thin
//! async/Wayland-touching methods on [`IdleController`]:
//! - [`register_threshold_entry`] / [`cleanup_generation_thresholds`]: the notify fan-out
//!   registry's create-vs-append decision and per-generation cleanup.
//! - [`apply_inhibit`] / [`apply_release_inhibit`] / [`cleanup_generation_inhibit`]: the inhibit
//!   refcount arithmetic and per-generation cleanup.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

// -------------------------------------------------------------------------------------------
// Wire-facing types.
// -------------------------------------------------------------------------------------------

/// `idle:register_threshold(sec, on_idle, on_resume)`'s `arguments: [sec]`
/// (docs/oblisk-supervisor-services-dbus.md §7.1; ADR-0032). The callbacks themselves stay
/// Renderer-side (Lua-local); only `sec` crosses the wire.
pub fn parse_register_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `idle:inhibit(reason)`'s `arguments: [reason]` (ADR-0032).
pub fn parse_inhibit_args(arguments: &[serde_json::Value]) -> Option<String> {
    arguments.first()?.as_str().map(str::to_string)
}

// -------------------------------------------------------------------------------------------
// Notify: threshold fan-out registry (ADR-0032's "one ext_idle_notification_v1 listener per
// distinct threshold duration, fanned out to every registration sharing it").
// -------------------------------------------------------------------------------------------

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

// -------------------------------------------------------------------------------------------
// Inhibit: per-generation refcount arithmetic (ADR-0032's "refcounted per generation, not a
// single boolean").
// -------------------------------------------------------------------------------------------

/// What one refcount transition means for the single shared inhibit fd: whether this call must
/// open it (the global 0->1 transition) or close it (the global ->0 transition). Never both --
/// [`apply_inhibit`] can only ever report `should_open_fd`, [`apply_release_inhibit`]/
/// [`cleanup_generation_inhibit`] can only ever report `should_close_fd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InhibitTransition {
    pub should_open_fd: bool,
    pub should_close_fd: bool,
}

fn total(counts: &HashMap<u32, u32>) -> u32 {
    counts.values().sum()
}

/// `idle:inhibit(reason)`'s refcount half: increments `generation_id`'s own count.
/// `should_open_fd` is `true` exactly when the *global* total across every generation was zero
/// before this call (ADR-0032's "opens the fd on 0->1").
pub fn apply_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    *counts.entry(generation_id).or_insert(0) += 1;
    InhibitTransition { should_open_fd: total_before == 0, should_close_fd: false }
}

/// `idle:release_inhibit()`'s refcount half: decrements `generation_id`'s own count.
/// `should_close_fd` is `true` exactly when the global total was non-zero before this call and
/// drops to zero because of it (ADR-0032's "closes it on 1->0"). Releasing a generation whose
/// count is already zero (or that never inhibited at all) is a silent no-op -- `saturating_sub`
/// never underflows, and `should_close_fd` stays `false` since the global total didn't actually
/// change.
pub fn apply_release_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    if let Some(count) = counts.get_mut(&generation_id) {
        *count = count.saturating_sub(1);
    }
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

/// The inhibit half of `reset_registrations` (ADR-0006/ADR-0032): zeros `generation_id`'s entire
/// count in one step (a crashed/reloading generation's count is dropped outright, not decremented
/// once) -- "the same per-generation reset `reset_registrations` already triggers on reload or
/// crash zeros this count too". `should_close_fd` follows the same global-total-reaches-zero rule
/// as [`apply_release_inhibit`], so a crashed generation holding the *only* live inhibit still
/// releases the real fd instead of leaking it.
pub fn cleanup_generation_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    counts.remove(&generation_id);
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

// -------------------------------------------------------------------------------------------
// Inhibit: hand-written `org.freedesktop.login1.Manager` proxy (no maintained zbus proxy crate
// for logind, same "hand-write it" precedent `dbus::tray`/`dbus::polkit` already established for
// SNI/DBusMenu and the polkit agent side respectively).
// -------------------------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1Manager {
    #[zbus(name = "Inhibit")]
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

/// `what`/`who`/`mode` are fixed by ADR-0032: `what` scoped to `"idle"` only (not
/// `"sleep"`/`"shutdown"`/lid-switch -- those govern unrelated system actions nothing here asked
/// for), `who` identifies this process, `mode = "block"` is the only mode that actually blocks
/// systemd's auto-suspend-on-idle action rather than merely delaying it.
const INHIBIT_WHAT: &str = "idle";
const INHIBIT_WHO: &str = "oblisk";
const INHIBIT_MODE: &str = "block";

// -------------------------------------------------------------------------------------------
// Notify: dedicated Wayland connection and dispatch thread.
// -------------------------------------------------------------------------------------------

/// Dispatch target for the Supervisor's own, separate Wayland connection (ADR-0010's sibling:
/// lock authority already owns a dedicated connection for the same "survives a Renderer crash or
/// reload" reason). Holds nothing but the raw-event forwarding channel -- every other bit of
/// state this feature needs (the fan-out registry, the bound proxies) lives on the async side,
/// reachable from [`IdleController`] directly, since Wayland proxies/`Connection`/`QueueHandle`
/// are `Send` (docs/adr/0032). This thread's only job is pumping `idled`/`resumed` events back
/// out over `raw_events_tx`.
struct WaylandThreadState {
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
struct NotifyRegistry {
    fanout: HashMap<Duration, Vec<u32>>,
    listeners: HashMap<Duration, ExtIdleNotificationV1>,
}

struct LiveNotify {
    notifier: ExtIdleNotifierV1,
    seat: WlSeat,
    connection: Connection,
    queue_handle: QueueHandle<WaylandThreadState>,
    registry: Arc<Mutex<NotifyRegistry>>,
    /// Kept alive for the controller's whole lifetime -- dropping it would stop the dispatch
    /// thread from ever being joined on shutdown, but nothing in this codebase joins Wayland
    /// dispatch threads on shutdown today (matches `audio::mixer::run`'s own detached
    /// `std::thread::spawn` in `main.rs`, which is never joined either).
    #[allow(dead_code)]
    dispatch_thread: std::thread::JoinHandle<()>,
}

enum NotifyState {
    Live(LiveNotify),
    /// `ext_idle_notifier_v1`/`wl_seat` weren't advertised, or the dedicated Wayland connection
    /// itself failed to establish -- same "degrade to inert, don't take the Supervisor down"
    /// precedent `TrayController::inert`/`BluetoothController::new` already established
    /// (ADR-0032).
    Inert,
}

/// The channel [`connect_wayland_idle`] hands back: raw, not-yet-fanned-out `(Duration,
/// IdleState)` events, one per fired `idled`/`resumed` on any live listener.
type RawIdleEventReceiver = UnboundedReceiver<(Duration, shared::IdleState)>;

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
fn connect_wayland_idle() -> Result<(LiveNotify, RawIdleEventReceiver), Box<dyn std::error::Error + Send + Sync>> {
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
/// `dbus::idle`-local signal type to relabel later (Standards review: unlike `TraySignal`/
/// `BluetoothSignal`, which are bare markers with no payload, a payload-carrying duplicate of
/// `shared::IdleEvent` would exist only to be copied field-for-field into it). Exits once either
/// side of the pipe closes.
fn spawn_idle_event_forwarder(registry: Arc<Mutex<NotifyRegistry>>, mut raw_events_rx: RawIdleEventReceiver, events_tx: UnboundedSender<shared::IdleEvent>) -> JoinHandle<()> {
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

// -------------------------------------------------------------------------------------------
// Inhibit: shared fd + per-generation refcount state.
// -------------------------------------------------------------------------------------------

struct InhibitState {
    counts: HashMap<u32, u32>,
    fd: Option<zbus::zvariant::OwnedFd>,
}

struct LiveInhibit {
    /// The Supervisor's already-established system-bus connection (shared with NetworkManager/
    /// BlueZ/polkit -- ADR-0032, no new connection). `Login1ManagerProxy` is built fresh from
    /// this on every [`IdleController::inhibit`] call rather than cached at construction time
    /// (Correctness review, Fix 2): caching a proxy built once at startup would freeze a single
    /// `Proxy::new` failure into a permanent, process-lifetime inhibit outage, directly
    /// contradicting ADR-0032's "its only failure mode is the `Inhibit` call itself failing
    /// per-request... not a startup-time capability gap". `Proxy::new` performs no D-Bus round
    /// trip of its own (this trait declares no cached properties), so rebuilding it per call
    /// costs nothing worth caching against.
    system_bus: zbus::Connection,
    /// `tokio::sync::Mutex`, not `std::sync::Mutex` (Correctness review, Fix 1): the refcount
    /// decision, the `Inhibit` D-Bus call, and the `fd` write must run as one atomic critical
    /// section with the lock held throughout -- `std::sync::Mutex`'s guard cannot be held across
    /// an `.await` point, `tokio::sync::Mutex`'s can. See [`IdleController::inhibit`]'s doc
    /// comment for the two races this closes.
    state: tokio::sync::Mutex<InhibitState>,
}

// -------------------------------------------------------------------------------------------
// Controller.
// -------------------------------------------------------------------------------------------

/// Bound on [`connect_wayland_idle`]'s background `spawn_blocking` task (see [`IdleController::new`]).
/// A local Wayland roundtrip against an already-running compositor completes in well under a
/// second in every observed live-tested run against niri; 5 seconds is generous headroom for a
/// busy compositor while still keeping a genuinely hung/misbehaving one (the failure mode this
/// bound exists for -- observed live once as a real deadlock, every thread parked, no CPU used,
/// for 60+ seconds with no timeout in place) bounded to a short, human-noticeable window instead
/// of wedging notify setup indefinitely. Notify-setup-specific, not reused from `reload.rs`'s
/// `PBA_TIMINGS` -- those bound a real Renderer's EGL/GL bring-up plus IPC round trips end to
/// end, an unrelated order of magnitude from one local `wl_display.sync()`.
const IDLE_NOTIFY_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct IdleController {
    /// `tokio::sync::RwLock`, not a bare `Arc<NotifyState>`: starts `Inert` and is swapped to
    /// `Live` in place by the background setup task [`IdleController::new`] spawns, so
    /// constructing an `IdleController` itself never blocks on Wayland at all -- see this
    /// module's doc comment and [`IdleController::new`].
    notify: Arc<RwLock<NotifyState>>,
    inhibit: Arc<LiveInhibit>,
}

impl IdleController {
    /// Constructs both halves and returns immediately. `system_bus` is the Supervisor's
    /// already-established `zbus::Connection::system()` (the same one NetworkManager/BlueZ/polkit
    /// share) -- inhibit rides it directly, no new connection (ADR-0032), and has no fallible
    /// construction step of its own (see [`LiveInhibit::system_bus`]'s doc comment for why its
    /// proxy isn't built here).
    ///
    /// Notify starts [`NotifyState::Inert`] and stays that way until (if ever) a background task
    /// -- spawned here, not awaited -- finishes [`connect_wayland_idle`] inside
    /// `tokio::task::spawn_blocking` (it's a genuinely blocking, synchronous function; running it
    /// inline on the async executor is exactly the anti-pattern docs/build-steps.md Phase 9 warns
    /// against for blocking calls in async code) within [`IDLE_NOTIFY_SETUP_TIMEOUT`]. This is
    /// deliberate, not an oversight: a real hang was observed live against niri inside that
    /// function's `roundtrip()` with no timeout in place, and it wedged the whole Supervisor
    /// because `main.rs` used to `.await` this constructor directly before opening the control
    /// socket. Returning immediately here, with the real setup relegated to a background task
    /// that can only ever *upgrade* `Inert` to `Live` (never block boot), makes that class of bug
    /// structurally impossible regardless of what `main.rs` calls next.
    pub async fn new(system_bus: zbus::Connection, events_tx: UnboundedSender<shared::IdleEvent>) -> Self {
        let notify = Arc::new(RwLock::new(NotifyState::Inert));

        let notify_for_task = notify.clone();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(IDLE_NOTIFY_SETUP_TIMEOUT, tokio::task::spawn_blocking(connect_wayland_idle)).await;
            match outcome {
                Ok(Ok(Ok((live, raw_events_rx)))) => {
                    spawn_idle_event_forwarder(live.registry.clone(), raw_events_rx, events_tx);
                    *notify_for_task.write().await = NotifyState::Live(live);
                    eprintln!("idle: dedicated Wayland connection for ext_idle_notifier_v1 established; notify live for this run");
                }
                Ok(Ok(Err(err))) => {
                    eprintln!("idle: dedicated Wayland connection for ext_idle_notifier_v1 unavailable; notify disabled for this run: {err}");
                }
                Ok(Err(join_err)) => {
                    eprintln!("idle: the dedicated Wayland connection setup task panicked; notify disabled for this run: {join_err}");
                }
                Err(_) => {
                    eprintln!(
                        "idle: dedicated Wayland connection setup for ext_idle_notifier_v1 did not complete within {IDLE_NOTIFY_SETUP_TIMEOUT:?} (possible compositor stall); notify disabled for this run"
                    );
                }
            }
        });

        Self {
            notify,
            inhibit: Arc::new(LiveInhibit {
                system_bus,
                state: tokio::sync::Mutex::new(InhibitState { counts: HashMap::new(), fd: None }),
            }),
        }
    }

    /// `idle:register_threshold(sec, on_idle, on_resume)`'s Supervisor-side half
    /// (docs/oblisk-supervisor-services-dbus.md §7.1; ADR-0032): a silent no-op when notify is
    /// inert (either permanently degraded, or the background setup task from
    /// [`IdleController::new`] just hasn't finished yet -- both look identical from here, by
    /// design), otherwise the pure fan-out decision ([`register_threshold_entry`]) followed by
    /// the real Wayland object creation exactly when that decision says a new listener is needed.
    pub async fn register_threshold(&self, generation_id: u32, sec: u64) {
        let notify = self.notify.read().await;
        let NotifyState::Live(live) = &*notify else {
            eprintln!("idle: register_threshold(generation {generation_id}, {sec}s) ignored: notify is inert for this run");
            return;
        };

        let created_new_listener = {
            let mut registry = live.registry.lock().unwrap();
            register_threshold_entry(&mut registry.fanout, generation_id, sec)
        };

        if created_new_listener {
            let duration = Duration::from_secs(sec);
            let timeout_ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
            let notification = live.notifier.get_idle_notification(timeout_ms, &live.seat, &live.queue_handle, duration);
            live.registry.lock().unwrap().listeners.insert(duration, notification);
            if let Err(err) = live.connection.flush() {
                eprintln!("idle: failed to flush the get_idle_notification request for {sec}s: {err}");
            }
        }
    }

    /// `idle:inhibit(reason)` (ADR-0032): the refcount decision ([`apply_inhibit`]), the real
    /// `Inhibit` D-Bus call (attempted exactly on the global 0->1 transition), and the resulting
    /// `fd`/count write all happen under one held `state` lock for the whole call -- not
    /// released between steps and re-acquired later. `release_inhibit`/`reset_registrations`
    /// need that same lock, so neither can run at all until this call either commits (writes
    /// `fd`) or rolls its own count bump back on failure and returns; there is no window in
    /// which they could observe or act on a partially-applied inhibit (Correctness review, Fix
    /// 1 -- this closes two races a prior "decide, unlock, await, re-lock, write" version had):
    ///
    /// - **Leak**: generation 1 calls `inhibit` (0->1, about to await the D-Bus call) while a
    ///   concurrent `release_inhibit` for the same generation is also in flight. With the lock
    ///   held for the whole `inhibit` call, that `release_inhibit` cannot even read `counts`
    ///   until `inhibit` has already written the real `fd` (or rolled its count back on
    ///   failure) and released the lock -- so `release_inhibit` always sees the count *after*
    ///   the open actually committed, never a stale zero it could no-op against while a real fd
    ///   is still on its way in.
    /// - **Clobber**: generation A holds the only inhibit and calls `release_inhibit` (1->0,
    ///   about to null `fd`) while generation B calls `inhibit` (0->1) concurrently. `inhibit`
    ///   cannot acquire the lock -- and so cannot open a fresh fd -- until `release_inhibit` has
    ///   finished nulling the old one and released the lock; B's fresh fd write can therefore
    ///   never land before A's null write and get wiped out by it.
    ///
    /// A silent no-op (nothing bumped, no D-Bus call attempted) if building the login1 proxy
    /// fails for this call -- see [`LiveInhibit::system_bus`]'s doc comment for why that proxy
    /// is built fresh here rather than once at startup (Fix 2). A failed `Inhibit` call itself
    /// rolls its own count bump back too -- an open that never actually happened must not leave
    /// this generation believing it holds an inhibit it doesn't.
    pub async fn inhibit(&self, generation_id: u32, reason: &str) {
        let mut state = self.inhibit.state.lock().await;

        let should_open_fd = apply_inhibit(&mut state.counts, generation_id).should_open_fd;
        if !should_open_fd {
            return;
        }

        let proxy = match Login1ManagerProxy::new(&self.inhibit.system_bus).await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!("idle: inhibit(generation {generation_id}, {reason:?}) failed to build the login1 Manager proxy: {err}");
                if let Some(count) = state.counts.get_mut(&generation_id) {
                    *count = count.saturating_sub(1);
                }
                return;
            }
        };

        match proxy.inhibit(INHIBIT_WHAT, INHIBIT_WHO, reason, INHIBIT_MODE).await {
            Ok(fd) => {
                state.fd = Some(fd);
            }
            Err(err) => {
                eprintln!("idle: Inhibit({INHIBIT_WHAT:?}, {INHIBIT_WHO:?}, {reason:?}, {INHIBIT_MODE:?}) failed: {err}");
                if let Some(count) = state.counts.get_mut(&generation_id) {
                    *count = count.saturating_sub(1);
                }
            }
        }
    }

    /// `idle:release_inhibit()` (ADR-0032): the refcount decision ([`apply_release_inhibit`])
    /// and the resulting `fd` clear happen under the same held `state` lock as
    /// [`IdleController::inhibit`] -- see its doc comment for why that matters. Dropping
    /// `OwnedFd` closes it, which releases the logind lock.
    pub async fn release_inhibit(&self, generation_id: u32) {
        let mut state = self.inhibit.state.lock().await;
        if apply_release_inhibit(&mut state.counts, generation_id).should_close_fd {
            state.fd = None;
        }
    }

    /// The notify + inhibit halves of `reset_registrations` (ADR-0006/ADR-0032): drops
    /// `generation_id`'s threshold fan-out entries and zeros its inhibit count, closing the
    /// shared fd if that was the last generation holding it, under the same held `state` lock
    /// [`IdleController::inhibit`]/[`IdleController::release_inhibit`] use -- a reload or crash
    /// racing a concurrent `inhibit`/`release_inhibit` for the same generation gets the same
    /// serialization guarantee they get from each other. No D-Bus/Wayland round trip needed
    /// either way (a `HashMap` mutation and, at most, dropping an already-open `OwnedFd`), but
    /// this is still `async` (not sync) purely because acquiring a `tokio::sync::Mutex` always
    /// is -- its caller (`main.rs`) already runs inside an async context.
    pub async fn reset_registrations(&self, generation_id: u32) {
        {
            let notify = self.notify.read().await;
            if let NotifyState::Live(live) = &*notify {
                let mut registry = live.registry.lock().unwrap();
                cleanup_generation_thresholds(&mut registry.fanout, generation_id);
            }
        }

        let mut state = self.inhibit.state.lock().await;
        if cleanup_generation_inhibit(&mut state.counts, generation_id).should_close_fd {
            state.fd = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_register_args / parse_inhibit_args ----

    #[test]
    fn parse_register_args_reads_the_first_argument_as_seconds() {
        assert_eq!(parse_register_args(&[serde_json::json!(30)]), Some(30));
    }

    #[test]
    fn parse_register_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_register_args(&[]), None);
        assert_eq!(parse_register_args(&[serde_json::json!("30")]), None);
    }

    #[test]
    fn parse_inhibit_args_reads_the_first_argument_as_a_reason_string() {
        assert_eq!(parse_inhibit_args(&[serde_json::json!("playing a video")]), Some("playing a video".to_string()));
    }

    #[test]
    fn parse_inhibit_args_is_none_for_an_empty_or_wrong_typed_argument() {
        assert_eq!(parse_inhibit_args(&[]), None);
        assert_eq!(parse_inhibit_args(&[serde_json::json!(42)]), None);
    }

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

    // ---- apply_inhibit / apply_release_inhibit (inhibit refcount arithmetic, TDD seam 2) ----

    #[test]
    fn apply_inhibit_opens_the_fd_on_the_global_zero_to_one_transition() {
        let mut counts = HashMap::new();
        let transition = apply_inhibit(&mut counts, 1);
        assert_eq!(transition, InhibitTransition { should_open_fd: true, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
    }

    #[test]
    fn apply_inhibit_does_not_reopen_the_fd_for_a_second_concurrent_generation() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        let transition = apply_inhibit(&mut counts, 2);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
        assert_eq!(counts.get(&2), Some(&1));
    }

    #[test]
    fn apply_release_inhibit_closes_the_fd_on_the_global_one_to_zero_transition() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        let transition = apply_release_inhibit(&mut counts, 1);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: true });
        assert_eq!(counts.get(&1), Some(&0));
    }

    #[test]
    fn apply_release_inhibit_from_one_generation_does_not_close_while_another_still_holds() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_inhibit(&mut counts, 2);

        let transition = apply_release_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false }, "generation 2 still holds an inhibit");
        assert_eq!(counts.get(&1), Some(&0));
        assert_eq!(counts.get(&2), Some(&1));
    }

    #[test]
    fn apply_release_inhibit_on_an_already_zero_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_release_inhibit(&mut counts, 1);

        let transition = apply_release_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&0), "must not underflow below zero");
    }

    #[test]
    fn apply_release_inhibit_on_a_never_registered_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        let transition = apply_release_inhibit(&mut counts, 42);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert!(!counts.contains_key(&42));
    }

    // ---- cleanup_generation_inhibit (TDD seam 5, inhibit half) ----

    #[test]
    fn cleanup_generation_inhibit_zeros_only_the_named_generation_and_closes_if_it_was_the_last() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);

        let transition = cleanup_generation_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: true });
        assert!(!counts.contains_key(&1));
    }

    #[test]
    fn cleanup_generation_inhibit_does_not_close_while_another_generation_still_holds() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_inhibit(&mut counts, 2);

        let transition = cleanup_generation_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert!(!counts.contains_key(&1));
        assert_eq!(counts.get(&2), Some(&1), "generation 2's own count must be untouched");
    }

    #[test]
    fn cleanup_generation_inhibit_on_an_untracked_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);

        let transition = cleanup_generation_inhibit(&mut counts, 99);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
    }

    // ---- Login1ManagerProxy::inhibit (TDD seam 3: real D-Bus call, p2p pattern) ----

    use tokio::net::UnixStream;

    /// A connected pair of p2p zbus connections, no bus daemon involved -- same pattern as
    /// `dbus::tray`/`dbus::polkit`/`dbus::bluetooth`'s own `p2p_pair` helpers.
    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder = zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    /// A stand-in for logind's own `org.freedesktop.login1.Manager` object, exported on the peer
    /// end of a p2p connection. Returns a real, valid fd (`/dev/null`, opened fresh) so the proxy
    /// call this test drives gets back something a real `OwnedFd` can wrap -- the actual fd's
    /// provenance doesn't matter to this test, only that one comes back at all.
    struct StubLogin1Manager {
        calls: tokio::sync::mpsc::UnboundedSender<(String, String, String, String)>,
    }

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl StubLogin1Manager {
        #[zbus(name = "Inhibit")]
        fn inhibit(&self, what: String, who: String, why: String, mode: String) -> zbus::zvariant::OwnedFd {
            let _ = self.calls.send((what, who, why, mode));
            let file = std::fs::File::open("/dev/null").expect("open /dev/null for a test fd");
            let owned: std::os::fd::OwnedFd = file.into();
            zbus::zvariant::OwnedFd::from(owned)
        }
    }

    #[tokio::test]
    async fn login1_manager_inhibit_sends_the_expected_arguments_and_returns_a_fd() {
        let (manager_side, caller_side) = p2p_pair().await;
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        manager_side
            .object_server()
            .at("/org/freedesktop/login1", StubLogin1Manager { calls: calls_tx })
            .await
            .expect("failed to export the stub Login1Manager");

        let proxy: Login1ManagerProxy<'_> = zbus::proxy::Builder::new(&caller_side)
            .destination("org.oblisk.test")
            .expect("valid destination bus name")
            .path("/org/freedesktop/login1")
            .expect("valid object path")
            .interface("org.freedesktop.login1.Manager")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p Login1ManagerProxy");

        let fd = proxy.inhibit("idle", "oblisk", "playing a video", "block").await.expect("Inhibit call should succeed");
        // A real fd came back -- `OwnedFd`'s own `Drop` closing it cleanly (no panic) is itself
        // part of what this assertion is checking.
        drop(fd);

        let (what, who, why, mode) = calls_rx.recv().await.expect("stub Login1Manager never received Inhibit");
        assert_eq!(what, "idle");
        assert_eq!(who, "oblisk");
        assert_eq!(why, "playing a video");
        assert_eq!(mode, "block");
    }
}
