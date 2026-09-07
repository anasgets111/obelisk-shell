//! [`IdleController`]: `oblisk.idle`'s write dispatcher and state owner for notify and inhibit.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use wayland_client::Proxy;

use super::gate::{IdleGate, blocks_idle};
use super::inhibit::{
    INHIBIT_MODE, INHIBIT_WHAT, INHIBIT_WHO, InhibitState, LiveInhibit, Login1ManagerProxy, apply_inhibit,
    apply_release_inhibit, cleanup_generation_inhibit,
};
use super::notify::{
    ListenerId, NotifyState, cleanup_generation_thresholds, connect_wayland_idle, register_threshold_entry,
    spawn_idle_event_forwarder,
};
use super::state::{IdleState, foreign_idle_inhibitors};

/// `idle:register_threshold(sec, on_idle, on_resume)`'s `arguments: [sec]`
/// (docs/oblisk-supervisor-services-dbus.md §7.1; ADR-0032). Callbacks stay Renderer-side;
/// only `sec` crosses the wire.
pub fn parse_register_args(arguments: &[serde_json::Value]) -> Option<u64> {
    arguments.first()?.as_u64()
}

/// `idle:inhibit(reason)`'s `arguments: [reason]` (ADR-0032).
pub fn parse_inhibit_args(arguments: &[serde_json::Value]) -> Option<String> {
    arguments.first()?.as_str().map(str::to_string)
}

/// Bound for [`connect_wayland_idle`]'s `spawn_blocking` task (see [`IdleController::new`]). A
/// local niri roundtrip is well under one second; 5s leaves headroom while bounding the genuine
/// compositor deadlock observed once at 60+ seconds without a timeout.
const IDLE_NOTIFY_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The two answers to "is anything holding the session awake", and the last payload built from
/// them (ADR-0160).
///
/// Different tasks watch logind's `BlockInhibited` and the compositor's silence. Neither may
/// publish a whole `IdleState`, which would erase what the other knows. Each sets its own field
/// here and takes back a payload to send, or `None` when nothing moved.
#[derive(Default)]
pub struct PublishedIdle {
    logind_blocked: bool,
    logind_inhibitors: Vec<super::state::IdleInhibitor>,
    wayland_inhibited: bool,
    last_sent: IdleState,
}

impl PublishedIdle {
    /// The merged payload. Nothing can name the compositor's half. No protocol lists
    /// idle-inhibitor holders, and the withholding has causes besides a surface inhibitor (see
    /// `notify::wayland_inhibited`). So it is an inhibitor with an empty `who`, the shape a config
    /// already draws for a logind holder that gave none, and a `why` stating the observation.
    fn merged(&self) -> IdleState {
        let mut inhibitors = self.logind_inhibitors.clone();
        if self.wayland_inhibited {
            inhibitors.push(super::state::IdleInhibitor {
                who: String::new(),
                why: "the compositor is holding off idle notifications".to_string(),
            });
        }
        IdleState { inhibited: self.logind_blocked || self.wayland_inhibited, inhibitors }
    }

    /// Records a change and returns the payload to send, or `None` when the answer is unchanged.
    fn settle(&mut self) -> Option<IdleState> {
        let next = self.merged();
        if next == self.last_sent {
            return None;
        }
        self.last_sent = next.clone();
        Some(next)
    }

    /// `None` means the compositor gave no evidence this round, so the last answer stands; see
    /// `notify::wayland_inhibited`.
    pub(crate) fn set_wayland_inhibited(&mut self, held: Option<bool>) -> Option<IdleState> {
        self.wayland_inhibited = held.unwrap_or(self.wayland_inhibited);
        self.settle()
    }

    fn set_logind(&mut self, blocked: bool, inhibitors: Vec<super::state::IdleInhibitor>) -> Option<IdleState> {
        self.logind_blocked = blocked;
        self.logind_inhibitors = inhibitors;
        self.settle()
    }
}

#[derive(Clone)]
pub struct IdleController {
    /// Registrations received while notify was [`NotifyState::Inert`], replayed when it becomes
    /// `Live` (ADR-0139). Config evaluation reliably beats setup, so without this every boot
    /// threshold was dropped, observed as `register_threshold(generation 0, 20s) ignored`.
    pending: Arc<std::sync::Mutex<Vec<(u32, u64)>>>,
    /// `std::sync::RwLock` around `Inert`/`Live`, swapped by the background setup task so
    /// constructing an `IdleController` never blocks on Wayland. Not the tokio one. Nothing holds
    /// it across an await, and a sync threshold half is what lets `dispatch` apply a forget and the
    /// registrations behind it in the order the socket delivered them (ADR-0158).
    notify: Arc<RwLock<NotifyState>>,
    inhibit: Arc<LiveInhibit>,
    /// The last state [`watch_idle_inhibitors`] published, so `Capabilities::start` returns the
    /// current answer instead of `nil` until the next inhibitor (ADR-0141).
    published: Arc<std::sync::Mutex<PublishedIdle>>,
}

impl IdleController {
    /// Constructs both halves and returns immediately. `system_bus` is the Supervisor's existing
    /// `zbus::Connection::system()`; inhibit uses it directly (ADR-0032).
    ///
    /// Notify starts [`NotifyState::Inert`] and upgrades to `Live` in a bounded `spawn_blocking`
    /// task running [`connect_wayland_idle`]. Its `roundtrip()` hung once against niri; awaiting
    /// it here would wedge the Supervisor.
    pub async fn new(
        system_bus: zbus::Connection,
        events_tx: UnboundedSender<shared::IdleEvent>,
        state_tx: UnboundedSender<IdleState>,
    ) -> Self {
        let notify = Arc::new(RwLock::new(NotifyState::Inert));
        // Watch the always-present system bus separately: a held inhibitor matters even when
        // Wayland notify degraded to inert (ADR-0139).
        let gate = Arc::new(std::sync::Mutex::new(IdleGate::default()));
        let published = Arc::new(std::sync::Mutex::new(PublishedIdle::default()));
        tokio::spawn(watch_idle_inhibitors(
            system_bus.clone(),
            gate.clone(),
            events_tx.clone(),
            state_tx.clone(),
            published.clone(),
        ));

        let controller = Self {
            notify: notify.clone(),
            published: published.clone(),
            pending: Arc::new(std::sync::Mutex::new(Vec::new())),
            inhibit: Arc::new(LiveInhibit {
                system_bus,
                state: tokio::sync::Mutex::new(InhibitState { counts: HashMap::new(), fd: None }),
            }),
        };

        let notify_for_task = notify.clone();
        let gate_for_task = gate.clone();
        let published_for_task = published.clone();
        let controller_for_task = controller.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(IDLE_NOTIFY_SETUP_TIMEOUT, tokio::task::spawn_blocking(connect_wayland_idle))
                    .await;
            match outcome {
                Ok(Ok(Ok((live, raw_events_rx)))) => {
                    spawn_idle_event_forwarder(
                        live.registry.clone(),
                        gate_for_task,
                        published_for_task,
                        raw_events_rx,
                        events_tx,
                        state_tx,
                    );
                    *notify_for_task.write().unwrap() = NotifyState::Live(live);
                    eprintln!(
                        "idle: dedicated Wayland connection for ext_idle_notifier_v1 established; notify live for this run"
                    );
                    // Swap first: otherwise `register_threshold` sees inert and requeues the
                    // replay.
                    controller_for_task.replay_pending_registrations();
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

    /// Inhibitor state for the initial `Capabilities::start` push; without it quiet machines read
    /// `nil` forever (ADR-0076).
    pub fn snapshot(&self) -> IdleState {
        self.published.lock().unwrap().merged()
    }

    /// Registers queued thresholds oldest first. Drain under the queue lock, then register outside
    /// it: `register_threshold` takes the same lock to requeue when notify is still inert.
    fn replay_pending_registrations(&self) {
        let queued: Vec<(u32, u64)> = std::mem::take(&mut *self.pending.lock().unwrap());
        if queued.is_empty() {
            return;
        }
        eprintln!("idle: notify is live; registering {} threshold(s) that arrived before it was", queued.len());
        for (generation_id, sec) in queued {
            self.register_threshold(generation_id, sec);
        }
    }

    /// Supervisor half of `idle:register_threshold(sec, on_idle, on_resume)`
    /// (docs/oblisk-supervisor-services-dbus.md §7.1; ADR-0032). Inert notify queues nothing
    /// here; live notify applies [`register_threshold_entry`] and creates a new listener if needed.
    pub fn register_threshold(&self, generation_id: u32, sec: u64) {
        let notify = self.notify.read().unwrap();
        let NotifyState::Live(live) = &*notify else {
            // Inert means degraded or still setting up. Queue entries in either case; a failed
            // setup leaves a small `(u32, u64)` queue rather than dropping every boot registration.
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
            let gated = ListenerId { duration, respects_inhibitors: true };
            let notification = live.notifier.get_idle_notification(timeout_ms, &live.seat, &live.queue_handle, gated);
            live.registry.lock().unwrap().listeners.insert(gated, notification);

            // The twin the compositor may not withhold. Its only job is to prove that silence on
            // the gated listener means an application is holding the session awake, rather than a
            // seat that is simply in use (ADR-0160). Version 1 compositors have no such request,
            // and degrade to the pre-ADR-0160 answer: `inhibited` reports logind only.
            if live.notifier.version() >= 2 {
                let input = ListenerId { duration, respects_inhibitors: false };
                let notification =
                    live.notifier.get_input_idle_notification(timeout_ms, &live.seat, &live.queue_handle, input);
                live.registry.lock().unwrap().listeners.insert(input, notification);
            }

            if let Err(err) = live.connection.flush() {
                eprintln!("idle: failed to flush the get_idle_notification request for {sec}s: {err}");
            }
        }
    }

    /// `idle:inhibit(reason)` (ADR-0032): refcount decision, global 0->1 `Inhibit` call, and
    /// `fd`/count write stay under one `state` lock. Releasing it allows a stale zero during the
    /// call (leak) or lets an inhibit/release pair write `fd` out of order (clobber).
    ///
    /// A login1 proxy-build failure is a silent no-op (built fresh, not cached; see
    /// [`LiveInhibit::system_bus`]). A failed `Inhibit` call rolls back its count bump.
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

    /// `idle:release_inhibit()` (ADR-0032): refcount decision and `fd` clear use the same held
    /// `state` lock as [`IdleController::inhibit`]. Dropping `OwnedFd` closes the logind lock.
    pub async fn release_inhibit(&self, generation_id: u32) {
        let mut state = self.inhibit.state.lock().await;
        if apply_release_inhibit(&mut state.counts, generation_id).should_close_fd {
            state.fd = None;
        }
    }

    /// Notify half alone (ADR-0158). Drops the generation's threshold entries because its Renderer
    /// just dropped the callbacks they feed, and is about to register what the new tree asks for.
    /// The Renderer's own socket orders that, so the fresh registrations land behind this one.
    ///
    /// Leaves inhibit counts alone. The VM lives through an in-place reload, so a config's
    /// `state(...)` record of its own hold survives with it. Zeroing the count here would drop the
    /// logind fd while the config still believed it held one, and nothing would retake it.
    pub fn reset_thresholds(&self, generation_id: u32) {
        // Remove queued registrations first: a reload replaced the tree owning their callbacks
        // and must not replay them later (ADR-0139).
        self.pending.lock().unwrap().retain(|&(queued_generation, _)| queued_generation != generation_id);
        let notify = self.notify.read().unwrap();
        if let NotifyState::Live(live) = &*notify {
            let mut registry = live.registry.lock().unwrap();
            cleanup_generation_thresholds(&mut registry.fanout, generation_id);
        }
    }

    /// Notify and inhibit halves of `reset_registrations` (ADR-0006/ADR-0032): everything
    /// `generation_id` owned, for a generation that is gone. Closes the shared fd if it was the
    /// last holder. Uses the same `state` lock as inhibit/release, serializing reload races.
    pub async fn reset_registrations(&self, generation_id: u32) {
        self.reset_thresholds(generation_id);

        let mut state = self.inhibit.state.lock().await;
        if cleanup_generation_inhibit(&mut state.counts, generation_id).should_close_fd {
            state.fd = None;
        }
    }
}

/// Follows `Manager.BlockInhibited` and keeps [`IdleGate`] in step (ADR-0139).
///
/// Watches the property rather than polling `ListInhibitors`: logind emits `PropertiesChanged`,
/// so any `systemd-inhibit --what=idle` reaches the gate in one round trip and costs nothing idle.
///
/// Reading before subscribing races; zbus replays the cached property on subscribe, making the
/// first item the startup state. Failure degrades to a permanently open gate, logged once.
async fn watch_idle_inhibitors(
    system_bus: zbus::Connection,
    gate: Arc<std::sync::Mutex<IdleGate>>,
    events_tx: UnboundedSender<shared::IdleEvent>,
    state_tx: UnboundedSender<IdleState>,
    published: Arc<std::sync::Mutex<PublishedIdle>>,
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
        // Read through the proxy: zbus caches the property and avoids naming the stream item's
        // borrowed type.
        let Ok(what) = proxy.block_inhibited().await else { continue };
        let blocked = blocks_idle(&what);
        // `None` means the same idle answer as before. `BlockInhibited` changes for every kind of
        // inhibitor, while the list can change without the answer moving (mpv releases while
        // Firefox still holds one), so publish state on every change but gate on transitions.
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

        // One `ListInhibitors` per change, never a timer: ADR-0139 rejected polling, and the
        // property edge provides one call to hang it off. Skip it when nothing blocks idle.
        let inhibitors = if blocked {
            proxy.list_inhibitors().await.map(foreign_idle_inhibitors).unwrap_or_default()
        } else {
            Vec::new()
        };
        // Held across the send; see the matching comment in `spawn_idle_event_forwarder`.
        let mut published = published.lock().unwrap();
        let Some(next_state) = published.set_logind(blocked, inhibitors) else { continue };
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
