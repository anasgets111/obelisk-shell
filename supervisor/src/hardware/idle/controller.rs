//! [`IdleController`]: the `oblisk.idle` write-action dispatcher and state owner, wiring
//! together the notify and inhibit halves.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;

use super::inhibit::{INHIBIT_MODE, INHIBIT_WHAT, INHIBIT_WHO, InhibitState, LiveInhibit, Login1ManagerProxy, apply_inhibit, apply_release_inhibit, cleanup_generation_inhibit};
use super::notify::{NotifyState, cleanup_generation_thresholds, connect_wayland_idle, register_threshold_entry, spawn_idle_event_forwarder};


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

}
