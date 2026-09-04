//! What the Supervisor knows between one loop iteration and the next.
//!
//! `run_supervisor`'s `select!` says which events exist and what order they run in; this says what
//! each does. State lives here, and the channel receivers the loop `await`s stay locals in
//! `main.rs`, each read by exactly one arm. Not a borrow-checker constraint: `tokio::select!` drops
//! the branch futures before running the winner, so `supervisor.authoritative.child.wait()` and
//! `&mut supervisor` compile in different branches, which the departure arm relies on. Some
//! operations must address only the *authoritative* generation, never the frame's:
//! `self.authoritative.generation_id` is read directly, not passed at each call site, except
//! `hydrate` and `answer_unchanged_report`, which take it as an argument. Not testable in
//! isolation: [`Capabilities`] needs a live system bus, so no unit test can build a `Supervisor`;
//! [`crate::is_current_reload`], [`crate::begin_reload`], and [`push_snapshot`] stay testable.

use std::collections::HashMap;

use shared::{ApplyPendingReload, Capability, SupervisorFrame};

use crate::capabilities::lock::{self, LockController};
use crate::capabilities::polkit::{self, Answer, PolkitController};
use crate::capabilities::{Capabilities, Signal};
use crate::generation::{
    Authoritative, RESTART_LIMIT, RESTART_WINDOW, RendererDeparture, RestartBrake, classify_departure, departure_report,
};
use crate::pam_worker;
use crate::polkit::AgentRequest;
use crate::process::registry::{LiveProcesses, reap_all_processes, take_exited_process, wait_and_report_exit};
use crate::reload_link::SocketCandidateLink;
use crate::snapshot::push_snapshot;
use crate::socket::{self, InboundFrame};
use crate::{PBA_TIMINGS, Shutdown, begin_reload, memory, process, reload, send_frame_logged};

/// Why a generation is being asked to take a lock it did not request. The log line differs by cause
/// though both arrive at the same state: a lock the compositor already holds with nothing of ours
/// on it. Naming it beats a `bool` whose two sides read identically at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelockReason {
    /// ADR-0058 decision 4: the Renderer holding the lock died and a replacement was spawned.
    RendererReplaced,
    /// ADR-0060: this Supervisor started with `$XDG_RUNTIME_DIR`'s marker set, so a previous one
    /// died while the session was locked.
    SupervisorRestarted,
}

impl RelockReason {
    /// The subject noun for the outcome messages below.
    fn subject(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the replacement",
            RelockReason::SupervisorRestarted => "the restarted shell",
        }
    }

    /// Why the lock is going out unasked, for the request-in-flight log line.
    fn because(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the Renderer that held it died",
            RelockReason::SupervisorRestarted => "the Supervisor that held it died",
        }
    }
}

/// Everything the Supervisor loop carries between iterations.
pub(crate) struct Supervisor {
    /// Every live Renderer connection, keyed by generation id.
    pub(crate) registry: socket::GenerationRegistry,
    /// The generation whose frames count and whose id every outbound frame below is addressed to.
    pub(crate) authoritative: Authoritative,
    /// Every capability's controller and sender (ADR-0076).
    pub(crate) capabilities: Capabilities,
    /// The lock capability, built here rather than in `Capabilities` (ADR-0052).
    pub(crate) lock: LockController,
    /// Kept alive for the whole run so the channel never closes; each outcome carries the
    /// acquisition it answers for (see `lock::accepts_outcome`).
    pub(crate) pam_outcome_tx: tokio::sync::mpsc::UnboundedSender<(u64, shared::PamOutcome)>,
    /// The polkit challenge on screen, built here like `lock` (ADR-0114).
    polkit: PolkitController,
    /// The polkit sibling of `pam_outcome_tx`, tagged with the challenge's cookie.
    polkit_outcome_tx: tokio::sync::mpsc::UnboundedSender<(String, shared::PamOutcome)>,

    /// `$XDG_RUNTIME_DIR`'s "the session is locked" marker, which outlives this process (ADR-0060).
    locked_flag: lock::SessionLockedFlag,
    /// Every capability's state-version counter, keyed by name (ADR-0004).
    revisions: HashMap<String, u32>,
    /// The last StateSnapshot pushed per capability, keyed by name: hydrates a fresh Candidate's
    /// first evaluation (§ 15.2 point 1; ADR-0029).
    last_snapshots: HashMap<String, shared::StateSnapshot>,
    /// The most recently sent `Reevaluate`'s sequence (ADR-0024).
    next_sequence: u64,
    /// The id the next spawned generation gets, whether a PBA candidate or a crash replacement.
    next_generation_id: u32,
    /// The Renderer binary every spawn below runs.
    renderer_path: String,
    restart_brake: RestartBrake,
    /// Set only by [`Supervisor::replace_departed_renderer`], telling shutdown "still running,
    /// needs reaping" from "already gone".
    renderer_departed: bool,
    /// ADR-0058 decision 4, ADR-0060: set when a Renderer dies holding the lock, or this process
    /// started already locked. `relock_when_connected` is the intent; `relock_in_flight` tells a
    /// re-acquisition's LockReport from an ordinary one's.
    relock_when_connected: Option<RelockReason>,
    relock_in_flight: Option<RelockReason>,
    /// Whether a topology-changing reload was refused while locked (ADR-0042). A bool, not a queue:
    /// a second change while locked is still one reload to run.
    swap_owed_on_unlock: bool,
    /// Every process.run-spawned child still tracked (ADR-0026).
    processes: LiveProcesses,
    process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
}

impl Supervisor {
    /// `boot_child` is generation 0, spawned earlier so a failure is fatal to `main` (ADR-0025).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        registry: socket::GenerationRegistry,
        boot_child: tokio::process::Child,
        renderer_path: String,
        capabilities: Capabilities,
        lock: LockController,
        locked_flag: lock::SessionLockedFlag,
        pam_outcome_tx: tokio::sync::mpsc::UnboundedSender<(u64, shared::PamOutcome)>,
        polkit_outcome_tx: tokio::sync::mpsc::UnboundedSender<(String, shared::PamOutcome)>,
        process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    ) -> Self {
        // Read once, here: reading later would race the boot Renderer's registration.
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
            revisions: HashMap::new(),
            last_snapshots: HashMap::new(),
            next_sequence: 0,
            next_generation_id: 1,
            renderer_path,
            restart_brake: RestartBrake::new(RESTART_LIMIT, RESTART_WINDOW),
            renderer_departed: false,
            relock_when_connected,
            relock_in_flight: None,
            swap_owed_on_unlock: false,
            processes: HashMap::new(),
            process_done_tx,
        }
    }

    /// Addresses a `SetSessionLock` and pushes the state that goes with it: every command this
    /// capability sends is also a state change a lock screen must see (ADR-0052 decision 4).
    pub(crate) fn send_lock_command(&mut self, command: shared::SetSessionLock) {
        send_frame_logged(&self.registry, self.authoritative.generation_id, &SupervisorFrame::SetSessionLock(command));
        self.push_lock_state();
    }

    /// Bumps the lock's revision and pushes its state (ADR-0052 decision 4): the only capability
    /// pushed from the loop rather than `Capabilities`, since its controller is built in `main`.
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

    /// polkitd asked for a challenge or withdrew one (ADR-0114).
    pub(crate) fn handle_polkit_request(&mut self, request: AgentRequest) {
        let changed = match request {
            AgentRequest::Begin { call, reply } => self.polkit.begin(call, reply),
            AgentRequest::Cancel { cookie } => self.polkit.cancel(Some(&cookie)),
        };
        if changed {
            self.push_polkit_state();
        }
    }

    /// `secure_submit(polkit, authenticate)`: one helper conversation for the challenge on screen,
    /// spawned for the reason the lock's is. `secret` is zeroized on the refusing path here and by
    /// the helper task on the other.
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

    /// The helper's answer for a polkit challenge. On success the helper has already told polkitd,
    /// so ending the held `BeginAuthentication` is all that is left, in the order its docs require.
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

    /// One roster capability's signal, straight back out as a snapshot (ADR-0076).
    pub(crate) async fn push_capability_signal(&mut self, signal: Signal) {
        self.capabilities
            .push(
                signal,
                &self.registry,
                self.authoritative.generation_id,
                &mut self.revisions,
                &mut self.last_snapshots,
            )
            .await;
    }

    /// Starts one reload cycle (ADR-0024, ADR-0041 decision 4), always for the authoritative
    /// generation: a superseded one must not start a cycle.
    pub(crate) fn begin_reload(&mut self) {
        begin_reload(&self.registry, self.authoritative.generation_id, &mut self.next_sequence);
    }

    /// Answers an `Unchanged` report: clears the reporting generation's idle registrations and
    /// sends the go-ahead, unless a newer `Reevaluate` already went out, in which case it must not
    /// fire for a superseded evaluation (ADR-0024). Addressed to the reporter: the apply lands on
    /// whoever evaluated, not the authoritative generation.
    pub(crate) async fn answer_unchanged_report(&self, generation_id: u32, sequence: u64) {
        if !crate::is_current_reload(sequence, self.next_sequence) {
            eprintln!(
                "generation {generation_id}'s Unchanged report (sequence {sequence}) is stale -- a newer Reevaluate (sequence {}) is \
                 already in flight; not applying",
                self.next_sequence
            );
            return;
        }
        // Only if the config asked for a threshold: with no controller nothing is registered.
        if let Some(idle) = self.capabilities.idle() {
            idle.reset_registrations(generation_id).await;
        }
        send_frame_logged(
            &self.registry,
            generation_id,
            &SupervisorFrame::ApplyPendingReload(ApplyPendingReload { sequence }),
        );
    }

    /// Records that a swap could not run because the session is locked (ADR-0042); a later report
    /// clearing the lock redeems it in [`Supervisor::record_lock_report`].
    pub(crate) fn defer_swap(&mut self, sequence: u64) {
        eprintln!("generation swap for sequence {sequence} deferred: the session is locked (ADR-0042)");
        self.swap_owed_on_unlock = true;
    }

    /// Replays every recorded snapshot to a generation that just registered, then hands it the lock
    /// if one is owed. Only for the authoritative generation: a PBA candidate gets its own
    /// hydration from `run_pba`'s snapshots argument instead (ADR-0029). Fixes the boot-time race:
    /// state captured before this connection existed is delivered now.
    pub(crate) fn hydrate(&mut self, generation_id: u32) {
        if generation_id != self.authoritative.generation_id {
            return;
        }
        for snapshot in self.last_snapshots.values() {
            send_frame_logged(&self.registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
        }
        // ADR-0058 decision 4, after the replay above: a lock acquired before hydration paints one
        // frame of defaults, indistinguishable from a broken shell. No auth-capability check here:
        // the Renderer reports Refused when its tree has no way to reach PAM (ADR-0052 decision 3).
        if let Some(reason) = self.relock_when_connected.take() {
            self.relock_in_flight = Some(reason);
            eprintln!(
                "asking generation {generation_id} to take the session lock over, because {} (ADR-0058 decision 4, ADR-0060)",
                reason.because()
            );
            self.lock.lock();
        }
    }

    /// Reports the authoritative Renderer's death and spawns its replacement (ADR-0058); `Some`
    /// means the loop must stop, and carries why.
    pub(crate) fn replace_departed_renderer(
        &mut self,
        status: std::io::Result<std::process::ExitStatus>,
    ) -> Option<Shutdown> {
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

        // Checked before the spawn, not after a failure (ADR-0058 decision 3): this defends against
        // every spawn succeeding while every Renderer then dies on the same config.
        if !self.restart_brake.allow(std::time::Instant::now()) {
            eprintln!(
                "giving up: {RESTART_LIMIT} renderers have died within {}s, which is a config that kills whatever it is \
                 handed rather than a transient (ADR-0058 decision 3)",
                RESTART_WINDOW.as_secs()
            );
            return Some(Shutdown::RestartBrakeTripped);
        }
        let replacement_generation_id = self.take_generation_id();
        match process::spawn_group_leader(
            &self.renderer_path,
            &[],
            &[(shared::GENERATION_ID_ENV.to_string(), replacement_generation_id.to_string())],
        ) {
            Ok(child) => {
                self.authoritative = Authoritative { generation_id: replacement_generation_id, child };
                self.renderer_departed = false;
                eprintln!("spawned generation {replacement_generation_id} to replace it");
                // Hydration needs no code here: the replacement's connected registration replays
                // every last_snapshots entry via `hydrate` above.
                if was_locked {
                    // ADR-0058 decision 4: the lock object died with the process, so `active` no
                    // longer describes anything held; `RendererLost` lets `lock()` through anyway,
                    // waiting to register since `send_frame_logged` needs a connection.
                    self.lock.record(lock::LockEvent::RendererLost);
                    self.relock_when_connected = Some(RelockReason::RendererReplaced);
                    eprintln!(
                        "the session is still locked, so generation {replacement_generation_id} will be asked to retake the lock once it connects"
                    );
                }
                None
            }
            Err(err) => {
                eprintln!("could not spawn a replacement renderer: {err}");
                Some(Shutdown::Requested)
            }
        }
    }

    /// Answers a PAM outcome for a lock authentication (ADR-0042). `acquisition` matters because a
    /// PAM answer outlives the lock it answers for: about a second normally, up to
    /// PAM_EXCHANGE_TIMEOUT's thirty when wedged, and in that window the compositor can end the
    /// lock or an idle timer can take a new one; `record_authentication` refuses a stale answer.
    pub(crate) fn record_pam_outcome(&mut self, acquisition: u64, outcome: shared::PamOutcome) {
        let succeeded = outcome == shared::PamOutcome::Success;
        if !self.lock.record_authentication(acquisition, outcome) {
            // No push: a refused answer changed no state, and push_lock_state bumps it regardless.
            eprintln!(
                "lock: dropping a pam outcome for acquisition {acquisition}, which is no longer the lock on the glass"
            );
        } else if succeeded {
            // The command's own arm pushes the snapshot that goes with it.
            self.lock.unlock();
        } else {
            self.push_lock_state();
        }
    }

    /// Records the authoritative generation's answer to a lock order (ADR-0052 decision 4), and
    /// runs any swap ADR-0042 deferred once `defers_swap` clears, which Refused satisfies too (only
    /// a request in flight keeps the gate shut), so a deferred change is never lost.
    pub(crate) fn record_lock_report(&mut self, report: shared::LockReport) {
        if let Some(who) = self.relock_in_flight.take() {
            let who = who.subject();
            match &report.outcome {
                shared::LockOutcome::Locked => {
                    eprintln!("{who} took the session lock over; the lock screen is back on the glass")
                }
                // Deliberately not lock_stays_authenticatable's wording: only the compositor's own
                // fallback is on screen here.
                shared::LockOutcome::Refused(reason) => eprintln!(
                    "{who} could not take the session lock over: {reason}. The session stays locked with no lock screen on it, \
                     so the way back in is a VT switch (ADR-0058 decision 4, ADR-0060)"
                ),
                other => eprintln!("{who}'s lock re-acquisition ended as {other:?} rather than a lock"),
            }
        }
        // Before record, off the outcome rather than LockState: the marker keeps saying "locked"
        // through a RendererLost that clears active (ADR-0060).
        self.locked_flag.apply(lock::compositor_lock_change(&report.outcome));
        self.lock.record(lock::LockEvent::Reported(report.outcome));
        self.push_lock_state();
        if !self.lock.defers_swap() && std::mem::take(&mut self.swap_owed_on_unlock) {
            // A fresh reload call, not the deferred evaluation replayed: its sequence is stale and
            // the config may have changed again since.
            self.begin_reload();
        }
    }

    /// Runs one generation swap for a `TopologyChanged` report (ADR-0025). Inlined synchronously,
    /// not tokio::spawn'd: swaps are rare and bounded (seconds, `PBA_TIMINGS`), nothing else
    /// capability-routed over this socket to starve. `inbound` is the loop's own receiver, borrowed
    /// so `SocketCandidateLink` can read the Candidate's ReadySignal and evidence off it.
    pub(crate) async fn swap_generation(
        &mut self,
        sequence: u64,
        inbound: &mut tokio::sync::mpsc::UnboundedReceiver<InboundFrame>,
    ) {
        let candidate_generation_id = self.take_generation_id();
        let candidate_envs = vec![
            (shared::GENERATION_ID_ENV.to_string(), candidate_generation_id.to_string()),
            ("OBLISK_PBA_CANDIDATE".to_string(), "1".to_string()),
        ];
        // Every capability's latest snapshot hydrates the Candidate's first evaluation (§ 15.2
        // point 1; ADR-0029), not just audio's.
        let snapshots: Vec<shared::StateSnapshot> = self.last_snapshots.values().cloned().collect();
        let mut link = SocketCandidateLink { registry: self.registry.clone(), candidate_generation_id, inbound };

        match reload::run_pba(&self.renderer_path, &[], &candidate_envs, &mut link, &snapshots, sequence, PBA_TIMINGS)
            .await
        {
            Ok(outcome) => {
                // ADR-0043 decision 1: the widest point of the handoff: the Candidate has presented
                // (run_pba returned Ok) and the superseded generation still owns every buffer, both
                // fully resident. Sampled here since the swap reaps one first.
                memory::log_sample(
                    "pba handoff",
                    &[
                        (self.authoritative.generation_id, &self.authoritative.child),
                        (candidate_generation_id, &outcome.candidate),
                    ],
                );
                reload::swap_and_reap(
                    &self.registry,
                    &mut self.processes,
                    &mut self.authoritative,
                    candidate_generation_id,
                    outcome,
                )
                .await;
            }
            Err(failure) => {
                eprintln!("generation swap for sequence {sequence} failed: {failure}");
                eprintln!("{} stays authoritative", self.authoritative.generation_id);
            }
        }
    }

    /// Routes a roster capability's command to its controller (ADR-0037); `lock` rides along
    /// because its controller is built outside `Capabilities` (ADR-0052).
    pub(crate) async fn dispatch_capability_command(
        &mut self,
        capability: Capability,
        envelope: &shared::CommandEnvelope,
    ) {
        // Here rather than in `Capabilities`, beside the push a cancel needs (ADR-0114).
        if capability == Capability::Polkit {
            if polkit::dispatch(&mut self.polkit, envelope) {
                self.push_polkit_state();
            }
            return;
        }
        self.capabilities.dispatch(capability, envelope, &self.lock).await;
    }

    /// Routes a `process` capability command (ADR-0026).
    pub(crate) async fn dispatch_process_command(&mut self, envelope: &shared::CommandEnvelope) {
        process::registry::dispatch(&mut self.processes, &self.registry, &self.process_done_tx, envelope).await;
    }

    /// Collects one finished `process.run` child. `wait` must never block `select!` (see
    /// `wait_and_report_exit`'s doc comment), so only the fast, synchronous removal happens inline.
    pub(crate) fn reap_exited_process(&mut self, generation_id: u32, id: u64) {
        if let Some(child) = take_exited_process(&mut self.processes, generation_id, id) {
            tokio::spawn(wait_and_report_exit(self.registry.clone(), generation_id, id, child));
        }
    }

    /// Reaps the authoritative Renderer and every still-live process.run child rather than exit out
    /// from under them (SIGTERM-then-SIGKILL, `process::DEFAULT_REAP_GRACE`), skipping the Renderer
    /// once [`Supervisor::replace_departed_renderer`] already collected it, so `reap_process_group`
    /// logs "already reaped" under the crash report that explains it.
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
    }

    /// Hands out the next generation id; every spawn goes through here, so no two generations share
    /// an id however many crash replacements and swaps interleave.
    fn take_generation_id(&mut self) -> u32 {
        let id = self.next_generation_id;
        self.next_generation_id += 1;
        id
    }
}
