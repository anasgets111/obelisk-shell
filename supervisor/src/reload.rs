//! Presentation-Before-Authority (PBA) hot-reload orchestration (build-steps.md Phase 8,
//! `docs/oblisk-supervisor-services-dbus.md` § 15.1-15.4).
//!
//! This module is the ordering/gating state machine only: the numbered sequence build-steps.md
//! draws (Overlapping Spawn, State Hydration, Null-Buffer Staging, Activate Draw, Evidence
//! Verification, Swap & Reap), implemented against two real primitives and one seam:
//!
//! - Real process lifecycle: [`process::spawn_group_leader`] and [`process::reap_process_group`]
//!   (Phase 7, `supervisor/src/process/mod.rs`) do the actual spawning and reaping.
//! - [`CandidateLink`]: an operation-level trait over the control socket. `socket::
//!   SocketCandidateLink` (Phase 14) is its real implementation; a fake still drives this
//!   module's own tests -- see the trait doc comment for the § reference each method maps to.
//!   `shared::StateSnapshot` is reused as-is for state hydration's payload (§ 15.2 point 1 names
//!   exactly this: "a state snapshot pushed by the Supervisor"). `shared::CommandEnvelope` is
//!   *not* reused for `ActivateDraw`: § 7.2's envelope is a generation-guarded wrapper around a
//!   Lua-initiated *write action* traveling Renderer -> Supervisor (capability/action/arguments/
//!   expected_revision), the opposite direction and a different shape from a Supervisor-issued
//!   one-off activation nonce -- forcing it on would invent a mismatched payload rather than
//!   reuse a real fit.
//!
//! What this module does *not* build -- see
//! docs/adr/0025-pba-orchestrator-wired-with-atomic-per-candidate-promotion.md: true independent
//! per-output timing (ADR-0003 describes each output transferring the moment its own evidence
//! lands, with no barrier on its siblings; this module still gates the Swap on *all* expected
//! surface_ids within one shared `evidence_timeout` -- a deliberate, safety-motivated
//! simplification, not the full per-output model) and a partial-candidate-abort primitive.
//!
//! Failure semantics (not spelled out by § 15's happy-path text, chosen as the reading
//! consistent with PBA's whole point -- never a black frame, never an unverified swap): any
//! failure before every expected surface_id's presentation evidence is verified -- a link error,
//! an unexpected/duplicate surface_id, or a deadline expiring -- aborts the Candidate (reaps its
//! process group) and leaves Generation `N` untouched and still authoritative. `run_pba` itself
//! never touches Generation `N` at all any more (see [`PbaOutcome`]'s doc comment) -- reaping it
//! is the caller's job, once the caller has sent the Swap messages. All four [`CandidateLink`]
//! steps are deadline-gated, not just two: `ready_timeout` bounds both `push_state_snapshot` and
//! `recv_ready_signal`, and `evidence_timeout` bounds both `send_activate_draw` and the whole
//! evidence-collection loop -- see [`PbaTimings`] and [`drive_handshake`].

use std::io;
use std::time::Duration;

use tokio::process::Child;
use tokio::time::timeout;

use crate::process;

/// The control-socket operations § 15.2-15.3 describe crossing from the Supervisor to the
/// Candidate generation. This trait is the seam a fake implementation drives in this module's
/// own tests; `socket::SocketCandidateLink` (Phase 14) is the real Unix-socket implementation.
pub trait CandidateLink {
    /// What a control-link call can fail with.
    type Error: std::fmt::Debug;

    /// § 15.2 point 1 / build-steps.md step 2 ("State Hydration"): push every pre-cached
    /// per-capability state snapshot down to the Candidate so it can hydrate its signals without
    /// querying the system itself (docs/adr/0029 generalizes this from a single audio-only
    /// snapshot to one per known capability).
    async fn push_state_snapshot(&mut self, snapshots: &[shared::StateSnapshot]) -> Result<(), Self::Error>;

    /// § 15.2 points 2-3 / build-steps.md step 3 ("Null-Buffer Staging"): block until the
    /// Candidate signals it has completed its Wayland layer-shell handshake and committed its
    /// null buffers -- i.e. it's ready to receive `ActivateDraw`. Returns the exact set of
    /// surface_ids the Candidate staged null buffers for; `run_pba` uses this as the expected
    /// set for evidence collection (docs/adr/0025 item 2).
    async fn recv_ready_signal(&mut self) -> Result<Vec<String>, Self::Error>;

    /// § 15.2 point 3 / build-steps.md step 4 ("Activate Draw"): write the unique, nonce-bound
    /// `ActivateDraw` command telling the Candidate to compile its layout and draw its first
    /// GPU frame.
    async fn send_activate_draw(&mut self, nonce: u64) -> Result<(), Self::Error>;

    /// § 15.3 point 4 / build-steps.md step 5 ("Evidence Verification"): block until the
    /// Candidate transmits one piece of presentation evidence for `nonce` -- confirmation the
    /// compositor's `wp_presentation_feedback` `presented` callback fired for one tracked
    /// surface's frame. Returns which surface_id this evidence is for. Called once per expected
    /// surface_id by `run_pba`'s evidence-collection loop.
    async fn recv_presentation_evidence(&mut self, nonce: u64) -> Result<String, Self::Error>;
}

/// Which step of § 15.2-15.3's sequence a [`PbaFailure`] happened during.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// § 15.2 point 1 / build-steps.md step 2.
    StateHydration,
    /// § 15.2 points 2-3 / build-steps.md step 3.
    NullBufferStaging,
    /// § 15.2 point 3 / build-steps.md step 4.
    ActivateDraw,
    /// § 15.3 / build-steps.md step 5.
    EvidenceVerification,
}

/// Why a PBA reload didn't reach a successful [`PbaOutcome`]. Every variant except
/// `SpawnFailed` implies the Candidate's process group was aborted (reaped) before returning --
/// see the module doc comment's failure-semantics paragraph. Generation `N` is never touched by
/// any of these -- `run_pba` never touches `N` at all any more, on any path (see `PbaOutcome`'s
/// doc comment).
#[derive(Debug)]
pub enum PbaFailure<E> {
    /// Step 1 (Overlapping Spawn) itself failed. There's no Candidate process to abort --
    /// nothing was spawned.
    SpawnFailed(io::Error),
    /// A `CandidateLink` call returned an error during `stage`.
    Link { stage: Stage, source: E },
    /// `recv_ready_signal` or the evidence-collection loop didn't resolve before its deadline
    /// during `stage`.
    Timeout { stage: Stage },
    /// The Candidate reported evidence for a surface_id `recv_ready_signal` never announced, or
    /// reported the same surface_id twice -- a wire-protocol-level desync, not an ordinary
    /// timeout or link error.
    UnexpectedEvidence { stage: Stage, surface_id: String },
    /// A failure above happened, and the abort-reap of the Candidate's process group that
    /// followed it *also* failed. Both are kept rather than the reap error replacing the
    /// original, so nothing about why the reload failed in the first place gets lost.
    AbortReapFailed { original: Box<PbaFailure<E>>, reap_error: io::Error },
}

/// A completed PBA reload: Generation `N+1` (`candidate`) is confirmed presented on every
/// expected surface. `promoted_surfaces` is every surface_id that completed presentation-
/// evidence verification, in `ReadySignal`'s order. Unlike earlier versions of this module,
/// `run_pba` does **not** reap Generation `N` (`superseded`) any more, and doesn't even take it
/// as a parameter -- § 15.4's own ordering is Input Deselection -> Candidate Promotion -> Reap,
/// and the Swap messages (`DeselectInput`/`PromoteGeneration`) go to two different connections
/// (`superseded`'s and the candidate's), while `CandidateLink` is deliberately scoped to only
/// the candidate's connection. So the caller sends the Swap messages for each of
/// `promoted_surfaces`, then reaps `superseded` itself via `process::reap_process_group`
/// directly -- see `main.rs`'s `TopologyChanged` handling.
#[derive(Debug)]
pub struct PbaOutcome {
    pub candidate: Child,
    pub promoted_surfaces: Vec<String>,
}

/// Runs steps 2-5 (State Hydration through Evidence Verification) against an already-spawned
/// Candidate's `link`. Split out from [`run_pba`] so its one job -- drive the handshake and
/// tag any failure with the [`Stage`] it happened during -- stays separate from spawn/abort/
/// reap, which need the Candidate's process handle that this function never touches.
///
/// Evidence collection loops over every surface_id `recv_ready_signal` returned, wrapped in one
/// `timeout(evidence_timeout, ...)` for the whole loop -- not one timeout per surface -- since
/// § 15.4's promotion gate is "all expected surface_ids within one shared deadline" (docs/adr/
/// 0025 item 3), not N independent per-surface deadlines. Returns the confirmed surface_ids in
/// `expected`'s order once every one has reported.
async fn drive_handshake<L: CandidateLink>(
    link: &mut L,
    snapshots: &[shared::StateSnapshot],
    nonce: u64,
    ready_timeout: Duration,
    evidence_timeout: Duration,
) -> Result<Vec<String>, PbaFailure<L::Error>> {
    timeout(ready_timeout, link.push_state_snapshot(snapshots))
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::StateHydration })?
        .map_err(|source| PbaFailure::Link { stage: Stage::StateHydration, source })?;

    let expected = timeout(ready_timeout, link.recv_ready_signal())
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::NullBufferStaging })?
        .map_err(|source| PbaFailure::Link { stage: Stage::NullBufferStaging, source })?;

    timeout(evidence_timeout, link.send_activate_draw(nonce))
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::ActivateDraw })?
        .map_err(|source| PbaFailure::Link { stage: Stage::ActivateDraw, source })?;

    let collect = async {
        let mut collected: std::collections::HashSet<String> = std::collections::HashSet::new();
        while collected.len() < expected.len() {
            let surface_id = link
                .recv_presentation_evidence(nonce)
                .await
                .map_err(|source| PbaFailure::Link { stage: Stage::EvidenceVerification, source })?;
            if !expected.contains(&surface_id) || !collected.insert(surface_id.clone()) {
                return Err(PbaFailure::UnexpectedEvidence { stage: Stage::EvidenceVerification, surface_id });
            }
        }
        Ok(())
    };
    timeout(evidence_timeout, collect).await.map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::EvidenceVerification })??;

    Ok(expected)
}

/// Aborts a Candidate that failed before evidence verification: reaps its process group and
/// folds a reap error into `failure` rather than discarding either. The one place that ever
/// reaps a Candidate on a failure path, so every `drive_handshake` error goes through the same
/// cleanup instead of each caller re-implementing it.
async fn abort_candidate<E>(candidate: &mut Child, grace: Duration, failure: PbaFailure<E>) -> PbaFailure<E> {
    match process::reap_process_group(candidate, grace).await {
        Ok(_) => failure,
        Err(reap_error) => PbaFailure::AbortReapFailed { original: Box::new(failure), reap_error },
    }
}

/// The three durations [`run_pba`] gates on: how long to wait for the Candidate's ready signal
/// and its presentation evidence before treating the reload as failed, and how long to give a
/// process group to honor `SIGTERM` before escalating to `SIGKILL` (passed straight through to
/// [`process::reap_process_group`]). Each of `ready_timeout` and `evidence_timeout` actually
/// bounds two handshake steps, not one: § 15.2's "state hydration and ready-signal wait" and
/// § 15.3's "activate draw and evidence wait" are each one logical stage, so `ready_timeout`
/// gates both `push_state_snapshot` and `recv_ready_signal`, and `evidence_timeout` gates both
/// `send_activate_draw` and `recv_presentation_evidence` -- see [`drive_handshake`]. Grouped into
/// one struct purely to keep `run_pba`'s parameter count reasonable -- these three don't share
/// any invariant with each other.
#[derive(Debug, Clone, Copy)]
pub struct PbaTimings {
    pub ready_timeout: Duration,
    pub evidence_timeout: Duration,
    pub reap_grace: Duration,
}

/// Runs one full PBA reload (§ 15.1-15.4, build-steps.md's numbered steps 1-5):
///
/// 1. **Overlapping Spawn**: spawns the Candidate via [`process::spawn_group_leader`], passing
///    `candidate_envs` through unchanged (e.g. `OBLISK_GENERATION_ID`/`OBLISK_PBA_CANDIDATE` --
///    see `main.rs`).
/// 2. **State Hydration** through 5. **Evidence Verification**: see [`drive_handshake`] and
///    [`CandidateLink`].
///
/// Step 6 (**Swap & Reap**) is *not* implemented here any more -- see [`PbaOutcome`]'s doc
/// comment for why reaping `superseded` moved to the caller, alongside sending § 15.4's Swap
/// messages (`DeselectInput`/`PromoteGeneration`), which never had any code in this module to
/// begin with (they cross a different connection than `CandidateLink` is scoped to).
///
/// Any failure in steps 1-5 aborts the Candidate and leaves the (still-authoritative, still
/// untouched by this function) superseded generation alone -- see the module doc comment's
/// failure-semantics paragraph.
pub async fn run_pba<L: CandidateLink>(
    candidate_cmd: &str,
    candidate_args: &[String],
    candidate_envs: &[(String, String)],
    link: &mut L,
    snapshots: &[shared::StateSnapshot],
    nonce: u64,
    timings: PbaTimings,
) -> Result<PbaOutcome, PbaFailure<L::Error>> {
    let mut candidate = process::spawn_group_leader(candidate_cmd, candidate_args, candidate_envs).map_err(PbaFailure::SpawnFailed)?;

    match drive_handshake(link, snapshots, nonce, timings.ready_timeout, timings.evidence_timeout).await {
        Ok(promoted_surfaces) => Ok(PbaOutcome { candidate, promoted_surfaces }),
        Err(failure) => Err(abort_candidate(&mut candidate, timings.reap_grace, failure).await),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    fn sh_args(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    fn sample_snapshot() -> shared::StateSnapshot {
        shared::StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({}) }
    }

    fn proc_exists(pid: i32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Polls `condition` until it's true or `timeout` elapses -- avoids a flaky single-shot
    /// check immediately after a reap, matching `process::mod`'s own test helper.
    async fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    struct FakeLinkError(String);

    /// What `push_state_snapshot`/`recv_ready_signal`/`send_activate_draw` should do, configured
    /// up front per test. `recv_ready_signal`'s payload (which surfaces it reports) is a
    /// separate field on [`FakeCandidateLink`] -- this only governs Ok/Err/hang.
    enum StepBehavior {
        Succeed,
        Fail,
        /// Never resolves within any reasonable test deadline -- simulates a wedged or silent
        /// Candidate so the orchestrator's own timeout is what has to save the test.
        Hang,
    }

    /// What one `recv_presentation_evidence` call should do -- popped off a queue, one entry per
    /// call, so a test can script a sequence (two surfaces succeed, a third hangs; the same
    /// surface_id reported twice; a surface_id never announced by `recv_ready_signal`).
    enum EvidenceOutcome {
        /// Reports evidence for this surface_id.
        Return(String),
        Fail,
        /// Never resolves -- same "wedged Candidate" role as `StepBehavior::Hang`. Also what an
        /// empty queue falls back to, since a test that never expects this call to matter
        /// (evidence collection never reached, e.g. `ready` itself hangs) needn't populate it.
        Hang,
    }

    /// In-memory [`CandidateLink`] fake: every method's behavior is configured up front, and
    /// calls are recorded in order so tests can assert on the sequence observed, not just the
    /// final outcome.
    struct FakeCandidateLink {
        hydration: StepBehavior,
        ready: StepBehavior,
        ready_surfaces: Vec<String>,
        activate: StepBehavior,
        evidence: Mutex<VecDeque<EvidenceOutcome>>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeCandidateLink {
        /// The common case: one expected surface (`"main_bar"`), `push_state_snapshot`/
        /// `send_activate_draw` always succeed immediately, `ready` and the one evidence call
        /// are configured explicitly.
        fn new(ready: StepBehavior, evidence: EvidenceOutcome) -> Self {
            Self::with_steps(StepBehavior::Succeed, ready, vec!["main_bar".to_string()], StepBehavior::Succeed, VecDeque::from([evidence]))
        }

        /// Full control over every step, for tests exercising a hang/failure on hydration or
        /// activate specifically, or more than one expected surface.
        fn with_steps(
            hydration: StepBehavior,
            ready: StepBehavior,
            ready_surfaces: Vec<String>,
            activate: StepBehavior,
            evidence: VecDeque<EvidenceOutcome>,
        ) -> Self {
            Self { hydration, ready, ready_surfaces, activate, evidence: Mutex::new(evidence), calls: Mutex::new(Vec::new()) }
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
                Some(EvidenceOutcome::Hang) | None => std::future::pending().await,
            }
        }
    }

    const SHORT_DEADLINE: Duration = Duration::from_millis(80);
    const GRACE: Duration = Duration::from_millis(200);

    /// `PbaTimings` with `reap_grace` fixed to [`GRACE`] -- what every test below wants, since
    /// none of them are exercising the reap-grace/escalation behavior itself (that's
    /// `process::mod`'s own test coverage).
    fn timings(ready_timeout: Duration, evidence_timeout: Duration) -> PbaTimings {
        PbaTimings { ready_timeout, evidence_timeout, reap_grace: GRACE }
    }

    #[tokio::test]
    async fn run_pba_promotes_the_candidate_on_full_success() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Return("main_bar".to_string()));

        let mut outcome = run_pba(
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
            vec!["push_state_snapshot", "recv_ready_signal", "send_activate_draw", "recv_presentation_evidence"],
            "the handshake must run in § 15.2-15.3's order"
        );

        // Clean up the newly-promoted candidate rather than leaking the sleep.
        process::reap_process_group(&mut outcome.candidate, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_promotes_all_expected_surfaces_in_readysignals_order_regardless_of_arrival_order() {
        let expected = vec!["main_bar".to_string(), "overlay_canvas".to_string(), "wallpaper_layer@DP-1".to_string()];
        // Evidence arrives in a different order than `expected` lists them -- proves the
        // returned `promoted_surfaces` order comes from ReadySignal, not arrival order.
        let arrival_order = VecDeque::from([
            EvidenceOutcome::Return("overlay_canvas".to_string()),
            EvidenceOutcome::Return("wallpaper_layer@DP-1".to_string()),
            EvidenceOutcome::Return("main_bar".to_string()),
        ]);
        let mut link = FakeCandidateLink::with_steps(StepBehavior::Succeed, StepBehavior::Succeed, expected.clone(), StepBehavior::Succeed, arrival_order);

        let mut outcome = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            43,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect("full success across 3 surfaces must promote");

        assert_eq!(outcome.promoted_surfaces, expected);
        process::reap_process_group(&mut outcome.candidate, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_when_ready_signal_times_out() {
        let mut link = FakeCandidateLink::new(StepBehavior::Hang, EvidenceOutcome::Return("main_bar".to_string()));

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            1,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging ready signal must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::NullBufferStaging }));
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_when_evidence_times_out() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Hang);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            2,
            timings(SHORT_DEADLINE, Duration::from_millis(30)),
        )
        .await
        .expect_err("evidence that never arrives must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::EvidenceVerification }));
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_when_state_hydration_times_out() {
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Hang,
            StepBehavior::Succeed,
            vec!["main_bar".to_string()],
            StepBehavior::Succeed,
            VecDeque::from([EvidenceOutcome::Return("main_bar".to_string())]),
        );

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            6,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging state-snapshot push must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::StateHydration }));
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_when_activate_draw_times_out() {
        let mut link = FakeCandidateLink::with_steps(
            StepBehavior::Succeed,
            StepBehavior::Succeed,
            vec!["main_bar".to_string()],
            StepBehavior::Hang,
            VecDeque::from([EvidenceOutcome::Return("main_bar".to_string())]),
        );

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            7,
            timings(SHORT_DEADLINE, Duration::from_millis(30)),
        )
        .await
        .expect_err("a hanging activate-draw send must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::ActivateDraw }));
    }

    /// Polls for `path` to contain a parseable pid, up to `timeout`. Used below to recover the
    /// spawned Candidate's own pid on a failure path, where `run_pba` doesn't return the
    /// `Child` -- the Candidate writes its own `$$` to `path` before blocking, since it's
    /// `exec`'d into the shell's own pid (no fork in between) and thus identical to
    /// `Child::id()`.
    async fn wait_for_pidfile(path: &std::path::Path, timeout: Duration) -> Option<i32> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse()
            {
                return Some(pid);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A pid-per-test-run file rather than matching on a shared command line -- `cargo test`
    /// runs these tests in parallel, and several of them spawn near-identical commands, so a
    /// name-based check (e.g. `pgrep -f`) would false-positive on a sibling test's own live
    /// child.
    fn unique_pidfile() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("system clock").as_nanos();
        std::env::temp_dir().join(format!("oblisk-reload-test-{}-{unique}.pid", std::process::id()))
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_process_group_on_a_ready_timeout() {
        let mut link = FakeCandidateLink::new(StepBehavior::Hang, EvidenceOutcome::Return("main_bar".to_string()));
        let pidfile = unique_pidfile();

        run_pba(
            "sh",
            &sh_args(&format!("echo $$ > {}; exec sleep 30", pidfile.display())),
            &[],
            &mut link,
            &[sample_snapshot()],
            3,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging ready signal must not promote");

        let candidate_pid = wait_for_pidfile(&pidfile, Duration::from_millis(300))
            .await
            .expect("candidate should have written its own pid before hanging on the ready signal");
        let _ = std::fs::remove_file(&pidfile);

        let gone = wait_until(Duration::from_millis(500), || !proc_exists(candidate_pid)).await;
        assert!(gone, "the aborted candidate (pid {candidate_pid}) must have been reaped, not leaked");
    }

    #[tokio::test]
    async fn run_pba_times_out_and_aborts_the_candidate_when_one_of_three_expected_surfaces_never_reports_evidence() {
        let expected = vec!["main_bar".to_string(), "overlay_canvas".to_string(), "wallpaper_layer@DP-1".to_string()];
        // Only two of the three surfaces "would have" succeeded -- the third's queue entry is
        // simply absent, which `recv_presentation_evidence` treats as a hang (see
        // `EvidenceOutcome::Hang`'s doc comment).
        let evidence = VecDeque::from([
            EvidenceOutcome::Return("main_bar".to_string()),
            EvidenceOutcome::Return("overlay_canvas".to_string()),
        ]);
        let mut link = FakeCandidateLink::with_steps(StepBehavior::Succeed, StepBehavior::Succeed, expected, StepBehavior::Succeed, evidence);
        let pidfile = unique_pidfile();

        let failure = run_pba(
            "sh",
            &sh_args(&format!("echo $$ > {}; exec sleep 30", pidfile.display())),
            &[],
            &mut link,
            &[sample_snapshot()],
            8,
            timings(SHORT_DEADLINE, Duration::from_millis(60)),
        )
        .await
        .expect_err("2 of 3 expected surfaces reporting evidence must not promote -- all-or-nothing gating");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::EvidenceVerification }));

        let candidate_pid = wait_for_pidfile(&pidfile, Duration::from_millis(300))
            .await
            .expect("candidate should have written its own pid before its evidence loop timed out");
        let _ = std::fs::remove_file(&pidfile);

        let gone = wait_until(Duration::from_millis(500), || !proc_exists(candidate_pid)).await;
        assert!(gone, "the aborted candidate (pid {candidate_pid}) must have been reaped even though 2 of 3 surfaces 'succeeded'");
    }

    #[tokio::test]
    async fn run_pba_fails_with_unexpected_evidence_for_a_surface_id_never_announced() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Return("never_announced".to_string()));

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            9,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("evidence for an unannounced surface_id must not promote");

        match failure {
            PbaFailure::UnexpectedEvidence { stage: Stage::EvidenceVerification, surface_id } => {
                assert_eq!(surface_id, "never_announced");
            }
            other => panic!("expected UnexpectedEvidence, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_pba_fails_with_unexpected_evidence_when_the_same_surface_id_is_reported_twice() {
        let expected = vec!["main_bar".to_string(), "overlay_canvas".to_string()];
        let evidence = VecDeque::from([EvidenceOutcome::Return("main_bar".to_string()), EvidenceOutcome::Return("main_bar".to_string())]);
        let mut link = FakeCandidateLink::with_steps(StepBehavior::Succeed, StepBehavior::Succeed, expected, StepBehavior::Succeed, evidence);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            10,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("a duplicate surface_id must not promote");

        match failure {
            PbaFailure::UnexpectedEvidence { stage: Stage::EvidenceVerification, surface_id } => {
                assert_eq!(surface_id, "main_bar");
            }
            other => panic!("expected UnexpectedEvidence, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_on_a_link_error() {
        let mut link = FakeCandidateLink::new(StepBehavior::Fail, EvidenceOutcome::Return("main_bar".to_string()));

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            4,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("a link error must not promote");

        match failure {
            PbaFailure::Link { stage: Stage::NullBufferStaging, source } => {
                assert_eq!(source, FakeLinkError("candidate link failed".to_string()))
            }
            other => panic!("expected a NullBufferStaging link error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_on_a_link_error_during_evidence_collection() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Fail);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &[],
            &mut link,
            &[sample_snapshot()],
            11,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("a link error during evidence collection must not promote");

        match failure {
            PbaFailure::Link { stage: Stage::EvidenceVerification, source } => {
                assert_eq!(source, FakeLinkError("candidate link failed".to_string()))
            }
            other => panic!("expected an EvidenceVerification link error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_pba_reports_spawn_failure_without_spawning_a_candidate() {
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, EvidenceOutcome::Return("main_bar".to_string()));

        let failure = run_pba(
            "/no/such/binary-oblisk-reload-test",
            &[],
            &[],
            &mut link,
            &[sample_snapshot()],
            5,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("spawning a nonexistent binary must fail");

        assert!(matches!(failure, PbaFailure::SpawnFailed(_)));
        assert!(link.calls.lock().unwrap().is_empty(), "no handshake call should happen if the spawn itself failed");
    }

    #[tokio::test]
    async fn wait_until_helper_reports_false_on_a_condition_that_never_becomes_true() {
        // Self-check for the polling helper above, mirroring process::mod's own test.
        let became_true = wait_until(Duration::from_millis(30), || false).await;
        assert!(!became_true);
    }
}
