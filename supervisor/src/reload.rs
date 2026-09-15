//! Generation swap orchestration: a Candidate presents before it takes authority.
//!
//! Ordering/gating only for six steps: spawn, hydrate, null-buffer stage, activate draw, verify
//! evidence, swap/reap. [`process`] owns spawn/reap; [`CandidateLink`] is the control-socket trait
//! with `socket::SocketCandidateLink` in production and a fake in this module's tests. Not here
//! (ADR-0025): independent per-output timing from ADR-0003, or partial-candidate abort. No output
//! transfers when its own evidence lands; sibling evidence is a barrier. This gates all expected
//! `surface_id`s within one `evidence_timeout`, a deliberate safety simplification. The generation
//! swap never shows a black frame or performs an unverified swap. Any failure before all evidence
//! is verified aborts the Candidate and leaves Generation `N` untouched. `run_swap` never touches
//! `N`; [`swap_and_reap`] does. All four link steps have deadlines.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::time::Duration;

use tokio::process::Child;

use crate::generation::Authoritative;
use crate::process::registry::{LiveProcesses, reap_generations_processes};
use crate::socket::{GenerationRegistry, send_frame_logged};
use tokio::time::timeout;

use crate::process;

/// Control-socket operations from Supervisor to Candidate.
pub trait CandidateLink {
    /// Control-link failure.
    type Error: std::fmt::Debug;

    /// Names the pid the just-spawned Candidate will connect from, so the listener
    /// can refuse anyone else claiming its generation id (`socket::GenerationRegistry`). Called
    /// before the Candidate can have connected. `None` releases the binding instead, for a
    /// Candidate that is about to be aborted or whose pid could not be read; either way the id is
    /// left unclaimable rather than open.
    fn expect_candidate_pid(&mut self, pid: Option<u32>);

    /// Pushes every cached capability snapshot so Candidate hydration needs no system
    /// query (ADR-0029).
    async fn push_state_snapshot(&mut self, snapshots: &[shared::StateSnapshot]) -> Result<(), Self::Error>;

    /// Waits for Wayland layer-shell handshake and null-buffer commit, then returns
    /// staged `surface_id`s as the evidence set (ADR-0025).
    async fn recv_ready_signal(&mut self) -> Result<Vec<String>, Self::Error>;

    /// Sends nonce-bound `ActivateDraw` to compile and draw the first GPU frame.
    async fn send_activate_draw(&mut self, nonce: u64) -> Result<(), Self::Error>;

    /// Waits for `wp_presentation_feedback`'s `presented` evidence for `nonce` and
    /// returns its `surface_id`; called once per surface.
    async fn recv_presentation_evidence(&mut self, nonce: u64) -> Result<String, Self::Error>;
}

/// Step in the handshake where [`SwapFailure`] occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    StateHydration,
    NullBufferStaging,
    ActivateDraw,
    /// A barrier over every expected surface, not one receipt.
    EvidenceVerification,
}

/// Why reload missed [`SwapOutcome`]. Except `SpawnFailed`, the Candidate was reaped; Generation
/// `N` remains untouched.
#[derive(Debug)]
pub enum SwapFailure<E> {
    /// Step 1 spawn failed; no Candidate exists to abort.
    SpawnFailed(io::Error),
    /// A `CandidateLink` call failed during `stage`.
    Link { stage: Stage, source: E },
    /// Ready/evidence collection missed its deadline in `stage`.
    Timeout { stage: Stage },
    /// Candidate reported an unannounced or duplicate `surface_id`: wire desync, not timeout/link.
    UnexpectedEvidence { stage: Stage, surface_id: String },
    /// Abort reap also failed; retain both errors.
    AbortReapFailed { original: Box<SwapFailure<E>>, reap_error: io::Error },
}

/// Log format for failed swaps, including `AbortReapFailed`'s nested `original` as text.
impl<E: fmt::Display> fmt::Display for SwapFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwapFailure::SpawnFailed(err) => write!(f, "could not spawn the candidate: {err}"),
            SwapFailure::Link { stage, source } => write!(f, "{stage:?} failed: {source}"),
            SwapFailure::Timeout { stage } => write!(f, "{stage:?} timed out"),
            SwapFailure::UnexpectedEvidence { stage, surface_id } => {
                write!(f, "{stage:?} got unexpected evidence for surface_id {surface_id:?}")
            }
            SwapFailure::AbortReapFailed { original, reap_error } => {
                write!(f, "{original}, and the candidate's abort-reap also failed: {reap_error}")
            }
        }
    }
}

/// Completed reload: `N+1` is presented on every expected surface; `promoted_surfaces` follows
/// `ReadySignal` order. `run_swap` neither reaps nor takes `N`: the swap orders deselection,
/// promotion, reap, with messages on two connections while the link reaches only Candidate.
/// [`swap_and_reap`] sends them, then reaps `superseded` via [`process::reap_process_group`].
#[derive(Debug)]
pub struct SwapOutcome {
    pub candidate: Child,
    pub promoted_surfaces: Vec<String>,
}

/// Runs steps 2-5 against an existing Candidate link, tagging failures by [`Stage`]. Separate
/// from spawn/abort/reap because it never touches the process handle. Collects every returned
/// `surface_id` under one `timeout(evidence_timeout, ...)`, not one timeout per surface, and
/// returns confirmed ids in `expected` order.
async fn drive_handshake<L: CandidateLink>(
    link: &mut L,
    snapshots: &[shared::StateSnapshot],
    nonce: u64,
    ready_timeout: Duration,
    evidence_timeout: Duration,
) -> Result<Vec<String>, SwapFailure<L::Error>> {
    timeout(ready_timeout, link.push_state_snapshot(snapshots))
        .await
        .map_err(|_elapsed| SwapFailure::Timeout { stage: Stage::StateHydration })?
        .map_err(|source| SwapFailure::Link { stage: Stage::StateHydration, source })?;

    let expected = timeout(ready_timeout, link.recv_ready_signal())
        .await
        .map_err(|_elapsed| SwapFailure::Timeout { stage: Stage::NullBufferStaging })?
        .map_err(|source| SwapFailure::Link { stage: Stage::NullBufferStaging, source })?;

    timeout(evidence_timeout, link.send_activate_draw(nonce))
        .await
        .map_err(|_elapsed| SwapFailure::Timeout { stage: Stage::ActivateDraw })?
        .map_err(|source| SwapFailure::Link { stage: Stage::ActivateDraw, source })?;

    let collect = async {
        let mut collected: std::collections::HashSet<String> = std::collections::HashSet::new();
        while collected.len() < expected.len() {
            let surface_id = link
                .recv_presentation_evidence(nonce)
                .await
                .map_err(|source| SwapFailure::Link { stage: Stage::EvidenceVerification, source })?;
            if !expected.contains(&surface_id) || !collected.insert(surface_id.clone()) {
                return Err(SwapFailure::UnexpectedEvidence { stage: Stage::EvidenceVerification, surface_id });
            }
        }
        Ok(())
    };
    timeout(evidence_timeout, collect)
        .await
        .map_err(|_elapsed| SwapFailure::Timeout { stage: Stage::EvidenceVerification })??;

    Ok(expected)
}

/// Aborts before evidence verification by reaping the group, folding any reap error into `failure`.
/// The only failure-path reap.
async fn abort_candidate<E>(candidate: &mut Child, grace: Duration, failure: SwapFailure<E>) -> SwapFailure<E> {
    match process::reap_process_group(candidate, grace).await {
        Ok(_) => failure,
        Err(reap_error) => SwapFailure::AbortReapFailed { original: Box::new(failure), reap_error },
    }
}

/// [`run_swap`] deadlines: ready, evidence, and process reap grace (`SIGTERM` to `SIGKILL`, passed
/// to [`process::reap_process_group`]). Ready bounds hydration+ready-wait; evidence bounds activate
/// through the evidence-verification barrier. Grouped only to limit parameters; no shared
/// invariant.
#[derive(Debug, Clone, Copy)]
pub struct SwapTimings {
    pub ready_timeout: Duration,
    pub evidence_timeout: Duration,
    pub reap_grace: Duration,
}

/// Runs the swap steps 1-5:
///
/// 1. **Overlapping Spawn**: [`process::spawn_group_leader`], passing `candidate_envs` unchanged
///    (for example `OBELISK_GENERATION_ID`/`OBELISK_SWAP_CANDIDATE`).
/// 2. **State Hydration** through 5. **Evidence Verification**: [`drive_handshake`].
///
/// Step 6 (**Swap & Reap**) is outside this function (see [`SwapOutcome`]). Failure in steps 1-5
/// aborts Candidate and leaves the authoritative superseded generation alone.
pub async fn run_swap<L: CandidateLink>(
    candidate_cmd: &str,
    candidate_args: &[String],
    candidate_envs: &[(String, String)],
    link: &mut L,
    snapshots: &[shared::StateSnapshot],
    nonce: u64,
    timings: SwapTimings,
) -> Result<SwapOutcome, SwapFailure<L::Error>> {
    let mut candidate =
        process::spawn_group_leader(candidate_cmd, candidate_args, candidate_envs).map_err(SwapFailure::SpawnFailed)?;
    // Before step 2 pushes it anything: the Candidate's generation id is bound to this pid.
    link.expect_candidate_pid(candidate.id());

    match drive_handshake(link, snapshots, nonce, timings.ready_timeout, timings.evidence_timeout).await {
        Ok(promoted_surfaces) => Ok(SwapOutcome { candidate, promoted_surfaces }),
        Err(failure) => {
            // About to be reaped, so release the id it was holding.
            link.expect_candidate_pid(None);
            Err(abort_candidate(&mut candidate, timings.reap_grace, failure).await)
        }
    }
}

/// Builds Swap messages in wire order: per surface, superseded stops input before Candidate
/// starts. A value makes that order testable; disconnected `send_frame_logged` only logs/drops and
/// cannot prove ordering.
fn swap_frames(
    superseded_generation_id: u32,
    candidate_generation_id: u32,
    promoted_surfaces: &[String],
) -> Vec<(u32, shared::SupervisorFrame)> {
    promoted_surfaces
        .iter()
        .flat_map(|surface_id| {
            [
                (
                    superseded_generation_id,
                    shared::SupervisorFrame::DeselectInput(shared::DeselectInput { surface_id: surface_id.clone() }),
                ),
                (
                    candidate_generation_id,
                    shared::SupervisorFrame::PromoteGeneration(shared::PromoteGeneration {
                        surface_id: surface_id.clone(),
                    }),
                ),
            ]
        })
        .collect()
}

/// Returns frames consumed by the handshake to the main loop. A failed Candidate is dead before
/// its deferred frames can be dispatched, so replay only frames belonging to other generations.
pub(crate) fn replay_deferred_frames(
    replay: &mut VecDeque<crate::socket::InboundFrame>,
    deferred: Vec<crate::socket::InboundFrame>,
    candidate_generation_id: u32,
    candidate_succeeded: bool,
) {
    replay.extend(
        deferred.into_iter().filter(|frame| candidate_succeeded || frame.generation_id != candidate_generation_id),
    );
}

/// Owns everything after [`run_swap`] verifies evidence, which previously lived in `main.rs`'s
/// `TopologyChanged` arm. It implements Swap & Reap. Rules: deselect each surface before promotion,
/// and reassign `authoritative` last. Deselect/reap read its generation id, so promoting first
/// would target Candidate and sweep its `process.run` children while leaving superseded ones. Reap
/// order is irrelevant: each generation's children are their own group leaders (ADR-0026), so
/// Renderer reap cannot reach them and either sweep collects them. Consume [`SwapOutcome`] because
/// Candidate becomes authoritative and must have one owner.
pub(crate) async fn swap_and_reap(
    registry: &GenerationRegistry,
    processes: &mut LiveProcesses,
    authoritative: &mut Authoritative,
    candidate_generation_id: u32,
    outcome: SwapOutcome,
) {
    for (generation_id, frame) in
        swap_frames(authoritative.generation_id, candidate_generation_id, &outcome.promoted_surfaces)
    {
        send_frame_logged(registry, generation_id, &frame);
    }
    reap_generations_processes(processes, authoritative.generation_id).await;
    match process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await {
        Ok(process::ReapOutcome::ExitedCleanly(status)) => {
            eprintln!("superseded generation {} exited cleanly: {status}", authoritative.generation_id);
        }
        Ok(process::ReapOutcome::Escalated(status)) => {
            eprintln!("superseded generation {} had to be escalated to SIGKILL: {status}", authoritative.generation_id);
        }
        Err(err) => {
            eprintln!("failed to reap superseded generation {}: {err}", authoritative.generation_id);
        }
    }
    // Reaped, so its id must stop being claimable before the kernel hands that pid to something
    // else (`socket::GenerationRegistry`).
    registry.forget_generation(authoritative.generation_id);
    *authoritative = Authoritative { generation_id: candidate_generation_id, child: outcome.candidate };
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::process::registry::spawn_and_register_process;

    fn sh_args(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    /// Deselect superseded before promoting Candidate on every surface, so no surface is live on
    /// both. Two frames per surface target opposite generations in that order.
    #[test]
    fn every_surface_is_deselected_on_the_superseded_generation_before_the_candidate_is_promoted() {
        let surfaces = vec!["bar@DP-1".to_string(), "bar@HDMI-A-1".to_string()];

        let frames = swap_frames(1, 2, &surfaces);

        let addressed: Vec<(u32, &str)> = frames
            .iter()
            .map(|(generation_id, frame)| {
                (
                    *generation_id,
                    match frame {
                        shared::SupervisorFrame::DeselectInput(_) => "deselect",
                        shared::SupervisorFrame::PromoteGeneration(_) => "promote",
                        other => panic!("a swap sends only DeselectInput and PromoteGeneration, not {other:?}"),
                    },
                )
            })
            .collect();
        assert_eq!(addressed, vec![(1, "deselect"), (2, "promote"), (1, "deselect"), (2, "promote")]);

        let named: Vec<&str> = frames
            .iter()
            .map(|(_, frame)| match frame {
                shared::SupervisorFrame::DeselectInput(f) => f.surface_id.as_str(),
                shared::SupervisorFrame::PromoteGeneration(f) => f.surface_id.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            named,
            vec!["bar@DP-1", "bar@DP-1", "bar@HDMI-A-1", "bar@HDMI-A-1"],
            "each surface hands over completely before the next starts"
        );
    }

    #[test]
    fn a_swap_with_no_promoted_surfaces_sends_nothing() {
        assert!(swap_frames(1, 2, &[]).is_empty());
    }

    /// After the swap, superseded has nothing running; `authoritative` names Candidate.
    /// It pins reassignment last; promoting first sweeps generation 2 instead of generation 1, so
    /// the registered `sleep` outlives reload. Hoisting the assignment fails this test.
    #[tokio::test]
    async fn a_swap_leaves_nothing_of_the_superseded_generation_running() {
        let registry = GenerationRegistry::default();
        let mut processes: LiveProcesses = std::collections::HashMap::new();
        spawn_and_register_process(&mut processes, 1, 7, "sh", &sh_args("sleep 30"));
        let child_pid = processes.get(&(1, 7)).unwrap().id().expect("a freshly spawned child has a pid");

        let superseded = process::spawn_group_leader("sh", &sh_args("sleep 30"), &[]).unwrap();
        let superseded_pid = superseded.id().expect("a freshly spawned child has a pid");
        let mut authoritative = Authoritative { generation_id: 1, child: superseded };
        let candidate = process::spawn_group_leader("sh", &sh_args("sleep 30"), &[]).unwrap();
        let outcome = SwapOutcome { candidate, promoted_surfaces: vec!["bar@DP-1".to_string()] };

        swap_and_reap(&registry, &mut processes, &mut authoritative, 2, outcome).await;

        assert_eq!(authoritative.generation_id, 2, "the candidate is authoritative once the swap returns");
        assert!(
            !processes.contains_key(&(1, 7)),
            "the superseded generation's `process.run` entry must be gone from the registry"
        );
        for (pid, what) in [
            (child_pid, "the superseded generation's `process.run` child"),
            (superseded_pid, "the superseded Renderer"),
        ] {
            assert!(process::exited(&[pid]).await, "{what} (pid {pid}) is still running after the swap");
        }

        let _ = process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await;
    }

    fn sample_snapshot() -> shared::StateSnapshot {
        shared::StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({}) }
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    struct FakeLinkError(String);

    /// Configured behavior for push/ready/activate. Governs only Ok/Err/hang; ready surfaces live
    /// separately on [`FakeCandidateLink`].
    enum StepBehavior {
        Succeed,
        Fail,
        /// Never resolves; simulates a wedged/silent Candidate and tests the orchestrator timeout.
        Hang,
    }

    /// One evidence call's behavior, popped from a scripted queue.
    enum EvidenceOutcome {
        /// Reports evidence for this surface_id.
        Return(String),
        Fail,
    }

    /// In-memory [`CandidateLink`] fake with configured behavior and ordered call recording.
    struct FakeCandidateLink {
        hydration: StepBehavior,
        ready: StepBehavior,
        ready_surfaces: Vec<String>,
        activate: StepBehavior,
        evidence: Mutex<VecDeque<EvidenceOutcome>>,
        calls: Mutex<Vec<&'static str>>,
        candidate_pid: Option<u32>,
    }

    impl FakeCandidateLink {
        /// Common case: one `main_bar` surface, successful hydration/activate, explicit ready and
        /// evidence.
        fn new(ready: StepBehavior, evidence: EvidenceOutcome) -> Self {
            Self::with_steps(
                StepBehavior::Succeed,
                ready,
                vec!["main_bar".to_string()],
                StepBehavior::Succeed,
                VecDeque::from([evidence]),
            )
        }

        /// Full control for hydration/activate failures or multiple expected surfaces.
        fn with_steps(
            hydration: StepBehavior,
            ready: StepBehavior,
            ready_surfaces: Vec<String>,
            activate: StepBehavior,
            evidence: VecDeque<EvidenceOutcome>,
        ) -> Self {
            Self {
                hydration,
                ready,
                ready_surfaces,
                activate,
                evidence: Mutex::new(evidence),
                calls: Mutex::new(Vec::new()),
                candidate_pid: None,
            }
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().expect("test-only lock").push(call);
        }

        async fn run_step(&self, behavior: &StepBehavior) -> Result<(), FakeLinkError> {
            match behavior {
                StepBehavior::Succeed => Ok(()),
                StepBehavior::Fail => Err(FakeLinkError("candidate link failed".to_string())),
                StepBehavior::Hang => std::future::pending().await,
            }
        }
    }

    impl CandidateLink for FakeCandidateLink {
        type Error = FakeLinkError;

        fn expect_candidate_pid(&mut self, pid: Option<u32>) {
            // Keeps the bound pid past the abort's `None`, so a test can check the Candidate is gone.
            self.candidate_pid = self.candidate_pid.or(pid);
            self.record("expect_candidate_pid");
        }

        async fn push_state_snapshot(&mut self, _snapshots: &[shared::StateSnapshot]) -> Result<(), Self::Error> {
            self.record("push_state_snapshot");
            self.run_step(&self.hydration).await
        }

        async fn recv_ready_signal(&mut self) -> Result<Vec<String>, Self::Error> {
            self.record("recv_ready_signal");
            self.run_step(&self.ready).await?;
            Ok(self.ready_surfaces.clone())
        }

        async fn send_activate_draw(&mut self, _nonce: u64) -> Result<(), Self::Error> {
            self.record("send_activate_draw");
            self.run_step(&self.activate).await
        }

        async fn recv_presentation_evidence(&mut self, _nonce: u64) -> Result<String, Self::Error> {
            self.record("recv_presentation_evidence");
            let step = self.evidence.lock().expect("test-only lock").pop_front();
            match step {
                Some(EvidenceOutcome::Return(surface_id)) => Ok(surface_id),
                Some(EvidenceOutcome::Fail) => Err(FakeLinkError("candidate link failed".to_string())),
                // An exhausted queue never resolves, like a silent Candidate.
                None => std::future::pending().await,
            }
        }
    }

    const SHORT_DEADLINE: Duration = Duration::from_millis(80);
    /// Bounds only a stalled reap; `sleep` exits on SIGTERM in milliseconds.
    const GRACE: Duration = Duration::from_secs(1);

    fn timings(ready_timeout: Duration, evidence_timeout: Duration) -> SwapTimings {
        SwapTimings { ready_timeout, evidence_timeout, reap_grace: GRACE }
    }

    /// Stage classification without a Candidate process, so reap timing cannot flake it.
    async fn handshake(link: &mut FakeCandidateLink) -> Result<Vec<String>, SwapFailure<FakeLinkError>> {
        drive_handshake(link, &[sample_snapshot()], 1, SHORT_DEADLINE, SHORT_DEADLINE).await
    }

    #[tokio::test]
    async fn run_swap_promotes_the_candidate_on_full_success() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Return("main_bar".to_string()));

        let mut outcome = run_swap(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            42,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect("full success must promote");

        assert_eq!(outcome.promoted_surfaces, vec!["main_bar".to_string()]);
        assert_eq!(
            *link.calls.lock().unwrap(),
            vec![
                "expect_candidate_pid",
                "push_state_snapshot",
                "recv_ready_signal",
                "send_activate_draw",
                "recv_presentation_evidence"
            ],
            "expect_candidate_pid must precede push_state_snapshot, recv_ready_signal, \
             send_activate_draw and recv_presentation_evidence, in that order, so the \
             Candidate's generation id is bound to its pid before the state-snapshot push \
             sends it anything"
        );

        // Clean up the newly-promoted candidate rather than leaking the sleep.
        let _ = process::reap_process_group(&mut outcome.candidate, GRACE).await;
    }

    #[tokio::test]
    async fn handshake_promotes_all_expected_surfaces_in_readysignals_order_regardless_of_arrival_order() {
        let expected = vec!["main_bar".to_string(), "overlay_canvas".to_string(), "wallpaper_layer@DP-1".to_string()];
        // Evidence arrives out of order; promoted order must follow ReadySignal, not arrival.
        let arrival_order = VecDeque::from([
            EvidenceOutcome::Return("overlay_canvas".to_string()),
            EvidenceOutcome::Return("wallpaper_layer@DP-1".to_string()),
            EvidenceOutcome::Return("main_bar".to_string()),
        ]);
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Succeed,
            StepBehavior::Succeed,
            expected.clone(),
            StepBehavior::Succeed,
            arrival_order,
        );

        let promoted = handshake(&mut link).await.expect("full success across 3 surfaces must promote");

        assert_eq!(promoted, expected);
    }

    #[tokio::test]
    async fn handshake_times_out_when_state_hydration_hangs() {
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Hang,
            StepBehavior::Succeed,
            vec!["main_bar".to_string()],
            StepBehavior::Succeed,
            VecDeque::from([EvidenceOutcome::Return("main_bar".to_string())]),
        );

        let failure = handshake(&mut link).await.expect_err("a hanging state-snapshot push must not promote");

        assert!(matches!(failure, SwapFailure::Timeout { stage: Stage::StateHydration }));
    }

    #[tokio::test]
    async fn handshake_times_out_when_activate_draw_hangs() {
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Succeed,
            StepBehavior::Succeed,
            vec!["main_bar".to_string()],
            StepBehavior::Hang,
            VecDeque::from([EvidenceOutcome::Return("main_bar".to_string())]),
        );

        let failure = handshake(&mut link).await.expect_err("a hanging activate-draw send must not promote");

        assert!(matches!(failure, SwapFailure::Timeout { stage: Stage::ActivateDraw }));
    }

    #[tokio::test]
    async fn run_swap_aborts_the_candidate_process_group_on_a_ready_timeout() {
        let mut link = FakeCandidateLink::new(StepBehavior::Hang, EvidenceOutcome::Return("main_bar".to_string()));

        let failure = run_swap(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            3,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging ready signal must not promote");

        assert!(matches!(failure, SwapFailure::Timeout { stage: Stage::NullBufferStaging }), "{failure:?}");
        let pid = link.candidate_pid.expect("run_swap binds the spawned candidate's pid");
        assert!(process::exited(&[pid]).await, "the aborted candidate (pid {pid}) must have been reaped, not leaked");
    }

    #[tokio::test]
    async fn handshake_times_out_when_one_of_three_expected_surfaces_never_reports_evidence() {
        let expected = vec!["main_bar".to_string(), "overlay_canvas".to_string(), "wallpaper_layer@DP-1".to_string()];
        // No third queue entry: `recv_presentation_evidence` treats it as a hang.
        let evidence = VecDeque::from([
            EvidenceOutcome::Return("main_bar".to_string()),
            EvidenceOutcome::Return("overlay_canvas".to_string()),
        ]);
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Succeed,
            StepBehavior::Succeed,
            expected,
            StepBehavior::Succeed,
            evidence,
        );

        let failure = handshake(&mut link)
            .await
            .expect_err("2 of 3 expected surfaces reporting evidence must not promote -- all-or-nothing gating");

        assert!(matches!(failure, SwapFailure::Timeout { stage: Stage::EvidenceVerification }));
    }

    #[tokio::test]
    async fn handshake_fails_with_unexpected_evidence_for_a_surface_id_never_announced() {
        let mut link =
            FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Return("never_announced".to_string()));

        let failure = handshake(&mut link).await.expect_err("evidence for an unannounced surface_id must not promote");

        match failure {
            SwapFailure::UnexpectedEvidence { stage: Stage::EvidenceVerification, surface_id } => {
                assert_eq!(surface_id, "never_announced");
            }
            other => panic!("expected UnexpectedEvidence, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_fails_with_unexpected_evidence_when_the_same_surface_id_is_reported_twice() {
        let expected = vec!["main_bar".to_string(), "overlay_canvas".to_string()];
        let evidence = VecDeque::from([
            EvidenceOutcome::Return("main_bar".to_string()),
            EvidenceOutcome::Return("main_bar".to_string()),
        ]);
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Succeed,
            StepBehavior::Succeed,
            expected,
            StepBehavior::Succeed,
            evidence,
        );

        let failure = handshake(&mut link).await.expect_err("a duplicate surface_id must not promote");

        match failure {
            SwapFailure::UnexpectedEvidence { stage: Stage::EvidenceVerification, surface_id } => {
                assert_eq!(surface_id, "main_bar");
            }
            other => panic!("expected UnexpectedEvidence, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_fails_on_a_link_error() {
        let mut link = FakeCandidateLink::new(StepBehavior::Fail, EvidenceOutcome::Return("main_bar".to_string()));

        let failure = handshake(&mut link).await.expect_err("a link error must not promote");

        match failure {
            SwapFailure::Link { stage: Stage::NullBufferStaging, source } => {
                assert_eq!(source, FakeLinkError("candidate link failed".to_string()))
            }
            other => panic!("expected a NullBufferStaging link error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_fails_on_a_link_error_during_evidence_collection() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Fail);

        let failure = handshake(&mut link).await.expect_err("a link error during evidence collection must not promote");

        match failure {
            SwapFailure::Link { stage: Stage::EvidenceVerification, source } => {
                assert_eq!(source, FakeLinkError("candidate link failed".to_string()))
            }
            other => panic!("expected an EvidenceVerification link error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_swap_reports_spawn_failure_without_spawning_a_candidate() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Return("main_bar".to_string()));

        let failure = run_swap(
            "/no/such/binary-obelisk-reload-test",
            &[],
            &[],
            &mut link,
            &[sample_snapshot()],
            5,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("spawning a nonexistent binary must fail");

        assert!(matches!(failure, SwapFailure::SpawnFailed(_)));
        assert!(link.calls.lock().unwrap().is_empty(), "no handshake call should happen if the spawn itself failed");
    }

    #[test]
    fn failed_candidate_frames_are_not_replayed_but_other_generations_are() {
        let mut replay = VecDeque::new();
        let deferred = vec![
            crate::socket::InboundFrame { generation_id: 0, frame: shared::RendererFrame::RequestReload },
            crate::socket::InboundFrame {
                generation_id: 1,
                frame: shared::RendererFrame::StartCapability { capability: "idle".to_string() },
            },
        ];

        replay_deferred_frames(&mut replay, deferred, 1, false);

        assert_eq!(replay.len(), 1);
        assert_eq!(replay.front().expect("the authoritative frame survives").generation_id, 0);
    }
}
