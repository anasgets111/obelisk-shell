//! What the Supervisor knows between one loop iteration and the next.
//!
//! `run_supervisor`'s `select!` owns event order; this owns each operation's state. Receivers stay
//! in `main.rs`, one per arm. `select!` drops losing futures before running the winner, allowing
//! `child.wait()` and `&mut supervisor` in separate branches. Most operations address only the
//! authoritative generation via `self.authoritative.generation_id`; `hydrate` is the exception.
//! [`Capabilities`] needs a live bus, so only [`push_snapshot`] is tested in isolation.

use std::collections::HashMap;

use shared::{Capability, SupervisorFrame};

use crate::capabilities::lock::{self, LockController};
use crate::capabilities::polkit::{self, Answer, PolkitController};
use crate::capabilities::{Capabilities, Signal};
use crate::generation::{
    Authoritative, RESTART_COOLDOWN, RESTART_LIMIT, RESTART_WINDOW, RendererDeparture, RestartBrake,
    classify_departure, departure_report,
};
use crate::pam_worker;
use crate::polkit::AgentRequest;
use crate::process::registry::{LiveProcesses, reap_all_processes, wait_and_report_exit};
use crate::snapshot::push_snapshot;
use crate::socket;
use crate::{process, send_frame_logged};

/// Why a generation must take an unrequested lock. Causes differ in logs but both mean the
/// compositor holds a lock with nothing of ours on it; a named enum beats an ambiguous `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelockReason {
    /// ADR-0058 decision 4: lock-holder Renderer died; replacement spawned.
    RendererReplaced,
    /// ADR-0060: runtime marker was set, so a previous Supervisor died while locked.
    SupervisorRestarted,
}

impl RelockReason {
    /// Subject noun for outcome messages.
    fn subject(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the replacement",
            RelockReason::SupervisorRestarted => "the restarted shell",
        }
    }

    /// Why the lock is being retaken, for the in-flight log.
    fn because(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the Renderer that held it died",
            RelockReason::SupervisorRestarted => "the Supervisor that held it died",
        }
    }
}

/// Supervisor loop state.
pub(crate) struct Supervisor {
    /// Live Renderer connections by generation.
    pub(crate) registry: socket::GenerationRegistry,
    /// Generation whose frames count and receive outbound frames.
    pub(crate) authoritative: Authoritative,
    /// Capability controllers and senders (ADR-0076).
    pub(crate) capabilities: Capabilities,
    /// Lock capability, built here rather than in `Capabilities` (ADR-0052).
    pub(crate) lock: LockController,
    /// Kept alive so the channel stays open; outcomes carry their acquisition
    /// (`lock::accepts_outcome`).
    pub(crate) pam_outcome_tx: tokio::sync::mpsc::UnboundedSender<(u64, shared::PamOutcome)>,
    /// Onscreen polkit challenge, built here like `lock` (ADR-0114).
    polkit: PolkitController,
    /// Polkit counterpart to `pam_outcome_tx`, tagged by challenge cookie.
    polkit_outcome_tx: tokio::sync::mpsc::UnboundedSender<(String, shared::PamOutcome)>,

    /// Persistent `$XDG_RUNTIME_DIR` locked marker (ADR-0060).
    locked_flag: lock::SessionLockedFlag,
    /// logind half of the same fact (ADR-0138), publishing `loginctl show-session`'s `LockedHint`
    /// whenever the marker changes.
    session_bridge: lock::logind::SessionBridge,
    /// Capability state-version counters by name (ADR-0004).
    revisions: HashMap<String, u32>,
    /// Last snapshot per capability, replayed to each new generation by [`Supervisor::hydrate`].
    last_snapshots: HashMap<String, shared::StateSnapshot>,
    /// Id for the next crash replacement.
    next_generation_id: u32,
    /// Renderer binary for every spawn.
    renderer_path: String,
    restart_brake: RestartBrake,
    pub(crate) respawn_at: Option<std::time::Instant>,
    /// Set only by [`Supervisor::replace_departed_renderer`]; distinguishes shutdown reaping from
    /// an already-gone Renderer.
    renderer_departed: bool,
    /// ADR-0058 decision 4, ADR-0060: intent after lock-holder death or locked startup;
    /// `relock_in_flight` distinguishes its report.
    relock_when_connected: Option<RelockReason>,
    relock_in_flight: Option<RelockReason>,
    /// Tracked `process.run` children (ADR-0026).
    processes: LiveProcesses,
    process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
}

impl Supervisor {
    /// Generation-0 child, spawned before construction so failure is fatal (ADR-0025).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        registry: socket::GenerationRegistry,
        boot_child: tokio::process::Child,
        renderer_path: String,
        capabilities: Capabilities,
        lock: LockController,
        locked_flag: lock::SessionLockedFlag,
        session_bridge: lock::logind::SessionBridge,
        pam_outcome_tx: tokio::sync::mpsc::UnboundedSender<(u64, shared::PamOutcome)>,
        polkit_outcome_tx: tokio::sync::mpsc::UnboundedSender<(String, shared::PamOutcome)>,
        process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    ) -> Self {
        // Read before boot registration; reading later would race it.
        let relock_when_connected = locked_flag.is_set().then_some(RelockReason::SupervisorRestarted);
        if relock_when_connected.is_some() {
            eprintln!(
                "the session was locked when the last Supervisor stopped and the compositor has not unlocked it, so the boot Renderer will be asked \
                 to take that lock over (ADR-0060)"
            );
        }
        Self {
            registry,
            authoritative: Authoritative { generation_id: 0, child: boot_child },
            capabilities,
            lock,
            pam_outcome_tx,
            polkit: PolkitController::default(),
            polkit_outcome_tx,
            locked_flag,
            session_bridge,
            revisions: HashMap::new(),
            last_snapshots: HashMap::new(),
            next_generation_id: 1,
            renderer_path,
            restart_brake: RestartBrake::default(),
            respawn_at: None,
            renderer_departed: false,
            relock_when_connected,
            relock_in_flight: None,
            processes: HashMap::new(),
            process_done_tx,
        }
    }

    /// Sends `SetSessionLock` and its state snapshot; every command is a lock-screen-visible state
    /// change (ADR-0052 decision 4).
    pub(crate) fn send_lock_command(&mut self, command: shared::SetSessionLock) {
        send_frame_logged(&self.registry, self.authoritative.generation_id, &SupervisorFrame::SetSessionLock(command));
        self.push_lock_state();
    }

    /// Pushes lock state and revision from the loop, since `main` builds its controller
    /// (ADR-0052 decision 4).
    pub(crate) fn push_lock_state(&mut self) {
        push_snapshot(
            &self.registry,
            self.authoritative.generation_id,
            &mut self.revisions,
            &mut self.last_snapshots,
            Capability::Lock,
            &self.lock.snapshot(),
        );
    }

    fn push_polkit_state(&mut self) {
        push_snapshot(
            &self.registry,
            self.authoritative.generation_id,
            &mut self.revisions,
            &mut self.last_snapshots,
            Capability::Polkit,
            &self.polkit.snapshot(),
        );
    }

    /// Handles a polkitd challenge or withdrawal (ADR-0114).
    pub(crate) fn handle_polkit_request(&mut self, request: AgentRequest) {
        let changed = match request {
            AgentRequest::Begin { call, reply } => self.polkit.begin(call, reply),
            AgentRequest::Cancel { cookie } => self.polkit.cancel(Some(&cookie)),
        };
        if changed {
            self.push_polkit_state();
        }
    }

    /// Starts one helper conversation for the onscreen challenge. Spawned like lock; `secret` is
    /// zeroized here on rejection and by the helper otherwise.
    pub(crate) fn begin_polkit_authentication(&mut self, mut secret: Vec<u8>, generation_id: u32) {
        match self.polkit.try_begin_authentication() {
            Some((uid, cookie)) => {
                self.push_polkit_state();
                let outcome_tx = self.polkit_outcome_tx.clone();
                tokio::spawn(pam_worker::run_polkit_helper(uid, cookie, shared::Zeroizing::new(secret), outcome_tx));
            }
            None => {
                eprintln!(
                    "generation {generation_id}'s secure_submit(polkit, authenticate) arrived with no challenge on screen, or with an attempt already in flight; dropping"
                );
                shared::Zeroize::zeroize(&mut secret);
            }
        }
    }

    /// Handles the helper's answer. On success polkitd was already told; release the held
    /// `BeginAuthentication` reply in the required order.
    pub(crate) fn record_polkit_outcome(&mut self, cookie: String, outcome: shared::PamOutcome) {
        match self.polkit.record_outcome(&cookie, outcome) {
            Answer::Stale => {
                eprintln!("polkit: dropping an outcome for {cookie:?}, which is no longer the challenge on screen")
            }
            Answer::Failed => self.push_polkit_state(),
            Answer::Succeeded { reply } => {
                self.push_polkit_state();
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Pushes one roster capability signal as a snapshot (ADR-0076).
    pub(crate) fn push_capability_signal(&mut self, signal: Signal) {
        self.capabilities.push(
            signal,
            &self.registry,
            self.authoritative.generation_id,
            &mut self.revisions,
            &mut self.last_snapshots,
        );
    }

    /// Asks the authoritative generation to re-evaluate its config (ADR-0216).
    pub(crate) fn begin_reload(&self) {
        send_frame_logged(&self.registry, self.authoritative.generation_id, &SupervisorFrame::Reevaluate);
    }

    /// Replays snapshots to a newly registered authoritative generation, then gives it an owed
    /// lock.
    pub(crate) fn hydrate(&mut self, generation_id: u32) {
        if generation_id != self.authoritative.generation_id {
            return;
        }
        for snapshot in self.last_snapshots.values() {
            send_frame_logged(&self.registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
        }
        // ADR-0058 decision 4: replay before relock, or one default frame looks like a broken
        // shell. No Supervisor-side auth-capability check here; Renderer reports Refused if its
        // tree cannot reach PAM (ADR-0052 decision 3).
        if let Some(reason) = self.relock_when_connected.take() {
            self.relock_in_flight = Some(reason);
            eprintln!(
                "asking generation {generation_id} to take the session lock over, because {} (ADR-0058 decision 4, ADR-0060)",
                reason.because()
            );
            self.lock.lock();
        }
    }

    /// Reports authoritative Renderer death and spawns a replacement (ADR-0058); `true` stops the loop.
    pub(crate) async fn replace_departed_renderer(
        &mut self,
        status: std::io::Result<std::process::ExitStatus>,
    ) -> bool {
        let departure = match status {
            Ok(status) => classify_departure(status),
            Err(err) => {
                eprintln!("failed to wait on generation {}'s renderer: {err}", self.authoritative.generation_id);
                RendererDeparture::Failed { code: -1 }
            }
        };
        let was_locked = self.lock.snapshot().active;
        eprintln!("{}", departure_report(departure, self.authoritative.generation_id, was_locked));
        self.renderer_departed = true;

        // Before the brake: the session ended under the whole shell, so every replacement would
        // find the same missing compositor. Stop the way a SIGTERM does, because it means the same thing.
        if matches!(departure, RendererDeparture::Failed { code } if code == shared::EXIT_COMPOSITOR_GONE) {
            eprintln!("the compositor is gone, so there is nothing to respawn into; shutting down");
            return true;
        }
        self.respawn_renderer().await
    }

    /// Spawns the replacement unless the brake defers it to `respawn_at` (ADR-0058 decision 3); `true` stops the loop.
    pub(crate) async fn respawn_renderer(&mut self) -> bool {
        if !self.restart_brake.allow(std::time::Instant::now()) {
            eprintln!(
                "{RESTART_LIMIT} renderers died within {RESTART_WINDOW:?}; respawning in {RESTART_COOLDOWN:?} (ADR-0058 decision 3)"
            );
            self.respawn_at = Some(std::time::Instant::now() + RESTART_COOLDOWN);
            return false;
        }
        self.respawn_at = None;
        // At respawn, not departure, and `requested` counts: a lock asked for during a cooldown is still owed.
        let state = self.lock.snapshot();
        let was_locked = state.active || state.requested;
        let replacement_generation_id = self.take_generation_id();
        match process::spawn_group_leader(
            &self.renderer_path,
            &[],
            &[(shared::GENERATION_ID_ENV.to_string(), replacement_generation_id.to_string())],
        ) {
            Ok(child) => {
                if let Some(pid) = child.id() {
                    self.registry.expect_generation(replacement_generation_id, pid);
                }
                // The departed generation's id must not stay claimable by whatever inherits its pid.
                let departed = self.authoritative.generation_id;
                self.registry.forget_generation(departed);
                self.authoritative = Authoritative { generation_id: replacement_generation_id, child };
                self.renderer_departed = false;
                eprintln!("spawned generation {replacement_generation_id} to replace it");
                self.capabilities.forget_departed_requests();
                // Without this the dead id kept its idle fan-out entry
                // (a failed push per idle transition, and any inhibit it held) and its `process.run`
                // children.
                if let Some(idle) = self.capabilities.idle() {
                    idle.reset_registrations(departed).await;
                }
                crate::process::registry::reap_generations_processes(&mut self.processes, departed).await;
                // Registration replays every `last_snapshots` entry via `hydrate`.
                if was_locked {
                    // ADR-0058 decision 4: the lock object died; `active` is stale. `RendererLost`
                    // lets `lock()` proceed, then waits for a connection to send it.
                    self.lock.record(lock::LockEvent::RendererLost);
                    self.relock_when_connected = Some(RelockReason::RendererReplaced);
                    eprintln!(
                        "the session is still locked, so generation {replacement_generation_id} will be asked to retake the lock once it connects"
                    );
                }
                false
            }
            Err(err) => {
                eprintln!("could not spawn a replacement renderer: {err}");
                true
            }
        }
    }

    /// Handles a PAM lock answer (ADR-0042). `acquisition` rejects stale answers: PAM normally
    /// takes a second, or `PAM_EXCHANGE_TIMEOUT`'s 30 seconds when wedged, while the compositor or
    /// idle timer may change locks. `loginctl lock-session` (ADR-0138) arrives as logind's `Lock`
    /// signal, uses the same lock path and already-locked guard as `lock:invoke("lock")`.
    pub(crate) fn lock_requested_by_logind(&mut self) {
        eprintln!("lock: logind asked for a lock (loginctl lock-session)");
        self.lock.lock();
        self.push_lock_state();
    }

    pub(crate) fn record_pam_outcome(&mut self, acquisition: u64, outcome: shared::PamOutcome) {
        let succeeded = outcome == shared::PamOutcome::Success;
        // Every answer, not only the refusals below. A wrong password logged nothing at all, so a
        // lock screen that would not open read the same in the log whether PAM said no or the
        // attempt never arrived.
        eprintln!("lock: pam answered {outcome:?} for acquisition {acquisition}");
        if !self.lock.record_authentication(acquisition, outcome) {
            // No push: refusal changed no state; `push_lock_state` would bump the revision anyway.
            eprintln!(
                "lock: dropping a pam outcome for acquisition {acquisition}, which is no longer the lock on the glass"
            );
        } else {
            // Push before scheduling: `unlocking` is now true, and the config cannot animate a
            // window it has not been told about (ADR-0190). The release is already committed by
            // the time the config sees it. Every accepted outcome pushes exactly once.
            self.push_lock_state();
            if succeeded {
                self.lock.unlock_after_animation();
            }
        }
    }

    /// Records the authoritative lock answer (ADR-0052 decision 4).
    pub(crate) fn record_lock_report(&mut self, report: shared::LockReport) {
        if let Some(who) = self.relock_in_flight.take() {
            let who = who.subject();
            match &report.outcome {
                shared::LockOutcome::Locked => {
                    eprintln!("{who} took the session lock over; the lock screen is back on the glass")
                }
                // Only the compositor's fallback is onscreen, not `lock_stays_authenticatable`.
                shared::LockOutcome::Refused(reason) => eprintln!(
                    "{who} could not take the session lock over: {reason}. The session stays locked with no lock screen on it, \
                     so the way back in is a VT switch (ADR-0058 decision 4, ADR-0060)"
                ),
                other => eprintln!("{who}'s lock re-acquisition ended as {other:?} rather than a lock"),
            }
        }
        // Derive from the outcome before recording; the marker stays "locked" through RendererLost,
        // which clears `active` (ADR-0060).
        let change = lock::compositor_lock_change(&report.outcome);
        self.locked_flag.apply(change);
        // Publish the same outcome-derived change to logind, keeping `LockedHint` and the marker
        // aligned (ADR-0138).
        match change {
            lock::SessionLock::Taken => self.session_bridge.publish_locked_hint(true),
            lock::SessionLock::Released => self.session_bridge.publish_locked_hint(false),
            lock::SessionLock::Unchanged => {}
        }
        self.lock.record(lock::LockEvent::Reported(report.outcome));
        self.push_lock_state();
    }

    /// Routes a roster command to its controller (ADR-0037); `lock` is separate (ADR-0052).
    pub(crate) fn dispatch_capability_command(&mut self, capability: Capability, envelope: &shared::CommandEnvelope) {
        // Keep polkit here beside the cancel path (ADR-0114).
        if capability == Capability::Polkit {
            if polkit::dispatch(&mut self.polkit, envelope) {
                self.push_polkit_state();
            }
            return;
        }
        self.capabilities.dispatch(capability, envelope, &self.lock);
    }

    /// Routes a `process` command (ADR-0026).
    pub(crate) async fn dispatch_process_command(&mut self, envelope: &shared::CommandEnvelope) {
        process::registry::dispatch(&mut self.processes, &self.registry, &self.process_done_tx, envelope).await;
    }

    /// Removes one finished `process.run` child inline; `wait` stays off `select!` (see
    /// `wait_and_report_exit`).
    pub(crate) fn reap_exited_process(&mut self, generation_id: u32, id: u64) {
        if let Some(child) = self.processes.remove(&(generation_id, id)) {
            tokio::spawn(wait_and_report_exit(self.registry.clone(), generation_id, id, child));
        }
    }

    /// Reaps authoritative Renderer and live `process.run` children with SIGTERM/SIGKILL and
    /// `DEFAULT_REAP_GRACE`. Skip a Renderer already collected by
    /// [`Supervisor::replace_departed_renderer`], avoiding a misleading "already reaped" log.
    pub(crate) async fn reap(&mut self) {
        if !self.renderer_departed
            && let Err(err) =
                process::reap_process_group(&mut self.authoritative.child, process::DEFAULT_REAP_GRACE).await
        {
            eprintln!(
                "failed to reap authoritative generation {}'s renderer on shutdown: {err}",
                self.authoritative.generation_id
            );
        }
        reap_all_processes(&mut self.processes).await;
        // Session processes are not in `self.processes`: they outlive generations by design, so
        // the per-generation sweep never sees them and this is their only reap.
        self.capabilities.reap_sessions().await;
    }

    /// Hands out unique generation ids for replacements.
    fn take_generation_id(&mut self) -> u32 {
        let id = self.next_generation_id;
        self.next_generation_id += 1;
        id
    }
}
