//! [`IdleController`]: the `oblisk.idle` write-action dispatcher and state owner, wiring
//! together the notify and inhibit halves.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::sync::mpsc::UnboundedSender;

use super::gate::{IdleGate, blocks_idle};
use super::inhibit::{
    INHIBIT_MODE, INHIBIT_WHAT, INHIBIT_WHO, InhibitState, LiveInhibit, Login1ManagerProxy, apply_inhibit,
    apply_release_inhibit, cleanup_generation_inhibit,
};
use super::notify::{
    NotifyState, cleanup_generation_thresholds, connect_wayland_idle, register_threshold_entry,
    spawn_idle_event_forwarder,
};
use super::state::{IdleState, foreign_idle_inhibitors};

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

/// Bound on [`connect_wayland_idle`]'s background `spawn_blocking` task (see
/// [`IdleController::new`]). A local Wayland roundtrip completes well under a second against
/// niri; 5s is generous headroom while still bounding a genuinely hung compositor (observed
/// live once as a real deadlock, 60+ seconds with no timeout) to a human-noticeable window.
const IDLE_NOTIFY_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct IdleController {
    /// Registrations that arrived while notify was still [`NotifyState::Inert`], replayed the
    /// moment it goes `Live` (ADR-0139). A config registers its thresholds during evaluation,
    /// which reliably beats the Wayland setup this constructor spawns, so without this every
    /// threshold a config asks for at boot is dropped -- observed live as
    /// `register_threshold(generation 0, 20s) ignored` on every single start.
    pending: Arc<std::sync::Mutex<Vec<(u32, u64)>>>,
    /// `tokio::sync::RwLock`, not a bare `Arc<NotifyState>`: starts `Inert` and is swapped to
    /// `Live` in place by the background setup task [`IdleController::new`] spawns, so
    /// constructing an `IdleController` never blocks on Wayland.
    notify: Arc<RwLock<NotifyState>>,
    inhibit: Arc<LiveInhibit>,
    /// The last state [`watch_idle_inhibitors`] published, so `Capabilities::start` can hand a
    /// config the current answer the moment it reads `oblisk.idle` rather than leaving it `nil`
    /// until the next inhibitor appears (ADR-0141).
    published: Arc<std::sync::Mutex<IdleState>>,
}

impl IdleController {
    /// Constructs both halves and returns immediately. `system_bus` is the Supervisor's
    /// already-established `zbus::Connection::system()`; inhibit rides it directly (ADR-0032).
    ///
    /// Notify starts [`NotifyState::Inert`] and only upgrades to `Live` from a background task
    /// running [`connect_wayland_idle`] inside `spawn_blocking`, bounded by
    /// [`IDLE_NOTIFY_SETUP_TIMEOUT`] -- its `roundtrip()` hung once live against niri with no
    /// timeout, and awaiting it directly here would wedge the whole Supervisor.
    pub async fn new(
        system_bus: zbus::Connection,
        events_tx: UnboundedSender<shared::IdleEvent>,
        state_tx: UnboundedSender<IdleState>,
    ) -> Self {
        let notify = Arc::new(RwLock::new(NotifyState::Inert));
        // Watched on the system bus, which is always there, rather than alongside notify: a held
        // inhibitor is worth knowing about even on a run where the Wayland half degraded to inert,
        // and the watch is what makes `idle:inhibit` mean anything at all (ADR-0139).
        let gate = Arc::new(std::sync::Mutex::new(IdleGate::default()));
        let published = Arc::new(std::sync::Mutex::new(IdleState::default()));
        tokio::spawn(watch_idle_inhibitors(
            system_bus.clone(),
            gate.clone(),
            events_tx.clone(),
            state_tx,
            published.clone(),
        ));

        let controller = Self {
            notify: notify.clone(),
            published,
            pending: Arc::new(std::sync::Mutex::new(Vec::new())),
            inhibit: Arc::new(LiveInhibit {
                system_bus,
                state: tokio::sync::Mutex::new(InhibitState { counts: HashMap::new(), fd: None }),
            }),
        };

        let notify_for_task = notify.clone();
        let gate_for_task = gate.clone();
        let controller_for_task = controller.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(IDLE_NOTIFY_SETUP_TIMEOUT, tokio::task::spawn_blocking(connect_wayland_idle))
                    .await;
            match outcome {
                Ok(Ok(Ok((live, raw_events_rx)))) => {
                    spawn_idle_event_forwarder(live.registry.clone(), gate_for_task, raw_events_rx, events_tx);
                    *notify_for_task.write().await = NotifyState::Live(live);
                    eprintln!(
                        "idle: dedicated Wayland connection for ext_idle_notifier_v1 established; notify live for this run"
                    );
                    // After the swap, never before: `register_threshold` reads `notify` and would
                    // find it still inert and queue the replay right back onto the list.
                    controller_for_task.replay_pending_registrations().await;
                }
                Ok(Ok(Err(err))) => {
                    eprintln!(
                        "idle: dedicated Wayland connection for ext_idle_notifier_v1 unavailable; notify disabled for this run: {err}"
                    );
                }
                Ok(Err(join_err)) => {
                    eprintln!(
                        "idle: the dedicated Wayland connection setup task panicked; notify disabled for this run: {join_err}"
                    );
                }
                Err(_) => {
                    eprintln!(
                        "idle: dedicated Wayland connection setup for ext_idle_notifier_v1 did not complete within {IDLE_NOTIFY_SETUP_TIMEOUT:?} (possible compositor stall); notify disabled for this run"
                    );
                }
            }
        });

        controller
    }

    /// Registers everything that arrived while notify was inert, oldest first. Drains under its
    /// own lock and registers outside it: `register_threshold` awaits, and holding a
    /// `std::sync::Mutex` across an await is the deadlock this codebase avoids everywhere else.
    /// The current inhibitor state, for the push `Capabilities::start` makes when a config first
    /// reads `oblisk.idle`. Without it the member reads `nil` until something takes or drops an
    /// inhibitor, which on a quiet machine is never -- the "reads `nil` forever" failure ADR-0076
    /// exists to prevent.
    pub fn snapshot(&self) -> IdleState {
        self.published.lock().unwrap().clone()
    }

    async fn replay_pending_registrations(&self) {
        let queued: Vec<(u32, u64)> = std::mem::take(&mut *self.pending.lock().unwrap());
        if queued.is_empty() {
            return;
        }
        eprintln!("idle: notify is live; registering {} threshold(s) that arrived before it was", queued.len());
        for (generation_id, sec) in queued {
            self.register_threshold(generation_id, sec).await;
        }
    }

    /// `idle:register_threshold(sec, on_idle, on_resume)`'s Supervisor-side half
    /// (docs/oblisk-supervisor-services-dbus.md §7.1; ADR-0032): a silent no-op when notify is
    /// inert (degraded or still setting up -- indistinguishable here), otherwise the fan-out
    /// decision ([`register_threshold_entry`]) plus real Wayland object creation for a new
    /// listener.
    pub async fn register_threshold(&self, generation_id: u32, sec: u64) {
        let notify = self.notify.read().await;
        let NotifyState::Live(live) = &*notify else {
            // Queued, not dropped: inert here means "degraded" *or* "still setting up", and the
            // two are indistinguishable from this side. A run where setup ultimately fails leaves
            // the queue holding entries nothing will ever register, which costs a `(u32, u64)`
            // each and is the right trade against losing every boot-time registration.
            self.pending.lock().unwrap().push((generation_id, sec));
            eprintln!("idle: register_threshold(generation {generation_id}, {sec}s) queued: notify is not live yet");
            return;
        };

        let created_new_listener = {
            let mut registry = live.registry.lock().unwrap();
            register_threshold_entry(&mut registry.fanout, generation_id, sec)
        };

        if created_new_listener {
            let duration = Duration::from_secs(sec);
            let timeout_ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
            let notification =
                live.notifier.get_idle_notification(timeout_ms, &live.seat, &live.queue_handle, duration);
            live.registry.lock().unwrap().listeners.insert(duration, notification);
            if let Err(err) = live.connection.flush() {
                eprintln!("idle: failed to flush the get_idle_notification request for {sec}s: {err}");
            }
        }
    }

    /// `idle:inhibit(reason)` (ADR-0032): the refcount decision ([`apply_inhibit`]), the real
    /// `Inhibit` D-Bus call (on the global 0->1 transition), and the resulting `fd`/count write
    /// all happen under one held `state` lock -- releasing it between steps reopens two races:
    /// a concurrent `release_inhibit` reading a stale zero count while a real fd is still in
    /// flight (leak), or a concurrent `inhibit`/`release_inhibit` pair writing `fd` out of
    /// order (clobber).
    ///
    /// A silent no-op if building the login1 proxy fails (built fresh, not cached -- see
    /// [`LiveInhibit::system_bus`]). A failed `Inhibit` call rolls its count bump back too.
    pub async fn inhibit(&self, generation_id: u32, reason: &str) {
        let mut state = self.inhibit.state.lock().await;

        let should_open_fd = apply_inhibit(&mut state.counts, generation_id).should_open_fd;
        if !should_open_fd {
            return;
        }

        let proxy = match Login1ManagerProxy::new(&self.inhibit.system_bus).await {
            Ok(proxy) => proxy,
            Err(err) => {
                eprintln!(
                    "idle: inhibit(generation {generation_id}, {reason:?}) failed to build the login1 Manager proxy: {err}"
                );
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
                eprintln!(
                    "idle: Inhibit({INHIBIT_WHAT:?}, {INHIBIT_WHO:?}, {reason:?}, {INHIBIT_MODE:?}) failed: {err}"
                );
                if let Some(count) = state.counts.get_mut(&generation_id) {
                    *count = count.saturating_sub(1);
                }
            }
        }
    }

    /// `idle:release_inhibit()` (ADR-0032): the refcount decision ([`apply_release_inhibit`])
    /// and the `fd` clear happen under the same held `state` lock as [`IdleController::inhibit`].
    /// Dropping `OwnedFd` closes it, releasing the logind lock.
    pub async fn release_inhibit(&self, generation_id: u32) {
        let mut state = self.inhibit.state.lock().await;
        if apply_release_inhibit(&mut state.counts, generation_id).should_close_fd {
            state.fd = None;
        }
    }

    /// The notify + inhibit halves of `reset_registrations` (ADR-0006/ADR-0032): drops
    /// `generation_id`'s threshold fan-out entries and zeros its inhibit count, closing the
    /// shared fd if it was the last holder -- under the same `state` lock
    /// [`IdleController::inhibit`]/[`IdleController::release_inhibit`] use, so a reload or
    /// crash racing either is serialized.
    pub async fn reset_registrations(&self, generation_id: u32) {
        // Queued registrations go too, and before the live ones: a reload that replaced the tree
        // owning those callbacks must not have them registered later by the replay (ADR-0139).
        self.pending.lock().unwrap().retain(|&(queued_generation, _)| queued_generation != generation_id);
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

/// Follows `Manager.BlockInhibited` and keeps [`IdleGate`] in step with it (ADR-0139).
///
/// A property watch, not a poll of `ListInhibitors`: logind emits `PropertiesChanged` for this
/// one, so a `systemd-inhibit --what=idle` anywhere on the system reaches the gate in a round trip
/// and costs nothing while nothing changes.
///
/// Reads the current value before subscribing would be a race; zbus's property stream replays the
/// cached value on subscribe, so the first item is the state at startup and no separate seed read
/// is needed. Degrades to a permanently open gate, logged once: a shell that cannot see inhibitors
/// behaves exactly as it did before this existed.
async fn watch_idle_inhibitors(
    system_bus: zbus::Connection,
    gate: Arc<std::sync::Mutex<IdleGate>>,
    events_tx: UnboundedSender<shared::IdleEvent>,
    state_tx: UnboundedSender<IdleState>,
    published: Arc<std::sync::Mutex<IdleState>>,
) {
    let proxy = match Login1ManagerProxy::new(&system_bus).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("idle: cannot reach logind to watch idle inhibitors; nothing will hold off idle actions: {err}");
            return;
        }
    };
    let mut changes = proxy.receive_block_inhibited_changed().await;
    while futures_util::StreamExt::next(&mut changes).await.is_some() {
        // Read back through the proxy rather than off the change item: zbus caches the property,
        // so this is the same value without having to name the stream item's borrowed type.
        let Ok(what) = proxy.block_inhibited().await else { continue };
        let blocked = blocks_idle(&what);
        // `None` is "same answer as last time", which is most of them: `BlockInhibited` changes on
        // every inhibitor of any kind, and almost none of them are idle. The gate only cares about
        // the transition; the state below is published on every change, because the *list* moves
        // without the answer moving -- mpv releasing while Firefox still holds one.
        if let Some(owed) = gate.lock().unwrap().set_blocked(blocked) {
            if blocked {
                eprintln!(
                    "idle: logind reports an idle inhibitor ({what}); threshold events are held until it is released"
                );
            } else {
                eprintln!("idle: no idle inhibitor is held any more; threshold events resume");
            }
            for event in owed {
                if events_tx.send(event).is_err() {
                    return;
                }
            }
        }

        // One `ListInhibitors` per change, never on a timer: ADR-0139 rejected polling this and
        // still does. What changed is that there is a signalled edge to hang a single call off.
        // Skipped entirely while nothing blocks idle, where the answer is empty by definition.
        let inhibitors = if blocked {
            proxy.list_inhibitors().await.map(foreign_idle_inhibitors).unwrap_or_default()
        } else {
            Vec::new()
        };
        let next_state = IdleState { inhibited: blocked, inhibitors };
        {
            let mut held = published.lock().unwrap();
            if *held == next_state {
                continue;
            }
            *held = next_state.clone();
        }
        if state_tx.send(next_state).is_err() {
            return;
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
}
