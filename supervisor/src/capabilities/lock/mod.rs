//! `oblisk.lock`: session-lock commands and the lock-screen state (ADR-0042, ADR-0052 decisions 1
//! and 4).
//!
//! The Renderer holds `ext_session_lock_v1` and paints it. This owns acquisition, outcome state,
//! and the only unlock call site. [`LockEvent`]s go through pure [`apply`], testable without socket
//! or PAM.

use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

pub mod logind;

/// `oblisk.lock`'s payload (ADR-0052 decision 4). `attempts` counts failed authentications since
/// acquisition. Lua cannot rebuild it from layout-time state (ADR-0044), so identical failures
/// leave one `error` string; empty `error` means no failure, like `keyboard.active_layout`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct LockState {
    /// The Renderer confirmed the session locked. A requested but unconfirmed lock remains `false`;
    /// [`apply`] changes this only from the Renderer report.
    pub active: bool,
    /// A password is with PAM and unanswered. `pam_unix` takes about a second, so this drives a
    /// spinner; `lock:authenticate` is refused while true.
    pub authenticating: bool,
    /// PAM answers against the held lock, including success. Resets to `0` only on a new confirmed
    /// lock, so it is per-acquisition, not per-failure; lockout rules read it with `error`.
    pub attempts: u32,
    /// Drawable reason for the last failure, e.g. `"too many attempts"`. Empty before attempts or
    /// after success; rewritten on every PAM answer and cleared on a new lock.
    pub error: String,
    /// `SetSessionLock { locked: true }` is in flight before Renderer confirmation.
    /// `#[serde(skip)]`:
    /// swap-gate state, not payload. `active` must mean only Renderer confirmation, but the gate
    /// shuts earlier because `ext_session_lock_v1` withholds `locked` until every output presents;
    /// swapping then reaps the holder and locks the user out (ADR-0042).
    #[serde(skip)]
    pub requested: bool,
    /// Acquisition number, bumped only on Renderer `Locked`. `#[serde(skip)]` like `requested`.
    /// PAM can outlive its lock (`pam_unix` ~1s, `PAM_EXCHANGE_TIMEOUT` 30s): `finished` after
    /// `locked` (also `loginctl unlock-session`) can end N while an idle timer takes N+1. The
    /// number binds an answer to its question and blocks stale success (`accepts_outcome`).
    #[serde(skip)]
    pub acquisition: u64,
}

/// Events from Lua `lock()`, the PAM worker, or the Renderer, all applied by pure [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// `lock.lock()` reached [`dispatch`]; `SetSessionLock` is on its way out.
    LockRequested,
    /// `secure_submit(lock, authenticate)` arrived and PAM is starting.
    AuthenticationStarted,
    /// Answer from the re-exec'd worker (ADR-0028), checked by
    /// [`LockController::record_authentication`] against its acquisition.
    Authenticated(shared::PamOutcome),
    /// Renderer report of the lock outcome.
    Reported(shared::LockOutcome),
    /// The `ext_session_lock_v1` holder died without a report (ADR-0058 decision 4). The compositor
    /// keeps the session locked; only this shell's ability to speak for it ended.
    RendererLost,
}

/// Pure synchronous transition table, testable without socket or PAM. Only the Renderer's report
/// moves `active`; a request or successful password is still unconfirmed.
pub fn apply(state: &mut LockState, event: LockEvent) {
    match event {
        // An in-place reload must not retain the old config's refusal reason (ADR-0052 decision 3).
        LockEvent::LockRequested => {
            state.requested = true;
            state.error.clear();
        }
        LockEvent::AuthenticationStarted => state.authenticating = true,
        LockEvent::Authenticated(shared::PamOutcome::Success) => {
            state.authenticating = false;
            state.error.clear();
        }
        LockEvent::Authenticated(outcome) => {
            state.authenticating = false;
            state.attempts += 1;
            state.error = error_for_outcome(&outcome);
        }
        // Only confirmed locks advance acquisition, invalidating answers for the prior lock.
        LockEvent::Reported(shared::LockOutcome::Locked) => {
            state.active = true;
            state.requested = false;
            state.authenticating = false;
            state.attempts = 0;
            state.error.clear();
            state.acquisition += 1;
        }
        // Nothing was taken or protected (ADR-0052 decision 3).
        LockEvent::Reported(shared::LockOutcome::Refused(reason)) => {
            state.requested = false;
            state.authenticating = false;
            state.error = reason;
        }
        // `Finished` after `Locked` is teardown, not failure; Renderer sets `oblisk.rescue` after
        // lock surfaces are gone (ADR-0052 decision 4). Keep `attempts`.
        LockEvent::Reported(shared::LockOutcome::Finished | shared::LockOutcome::Unlocked) => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
            state.error.clear();
        }
        // Clear `active` so a replacement can re-acquire (`lock` drops requests while active), and
        // clear `authenticating` (see [`accepts_outcome`]); keep acquisition and error.
        LockEvent::RendererLost => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
        }
    }
}

/// ADR-0042 swap gate. One client may hold a session lock, so candidate N+1 cannot acquire N while
/// it is held or mid-acquisition. `requested` is the unresolved half; ignoring it can reap the
/// holder mid-handshake with nobody able to unlock the compositor.
pub fn defers_swap(state: &LockState) -> bool {
    state.active || state.requested
}

/// What [`shared::LockOutcome`] says about the compositor's lock, distinct from [`apply`]
/// (ADR-0060). `LockState.active` means this shell holds it, but the compositor lock outlives
/// that: `RendererLost` clears `active` while the session stays locked. A restarted Supervisor
/// reads this outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLock {
    Taken,
    Released,
    Unchanged,
}

/// Exhaustive mapping: every new [`shared::LockOutcome`] must decide whether the session is locked,
/// rather than defaulting to [`SessionLock::Unchanged`].
pub fn compositor_lock_change(outcome: &shared::LockOutcome) -> SessionLock {
    match outcome {
        shared::LockOutcome::Locked => SessionLock::Taken,
        shared::LockOutcome::Unlocked | shared::LockOutcome::Finished => SessionLock::Released,
        // A refusal took and released nothing; preserve an earlier generation's marker.
        shared::LockOutcome::Refused(_) => SessionLock::Unchanged,
    }
}

/// The one lock fact outliving Supervisor (ADR-0060): a file in `$XDG_RUNTIME_DIR` exists exactly
/// while the compositor is locked. After restart `LockState::default()` would make `active` false
/// and paint behind an invisible fallback (ADR-0058, 0059). A file suffices for this boolean and
/// survives `SIGKILL`; runtime dir bounds staleness to the last session.
pub struct SessionLockedFlag {
    path: std::path::PathBuf,
}

impl SessionLockedFlag {
    /// Marker at an explicit path. `main.rs` uses [`shared::session_locked_flag_path`]; tests use a
    /// temporary directory.
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// Whether the compositor was locked at the last write. Read errors mean "not locked" on
    /// purpose; the alternative relocks at every boot when one file cannot be read.
    pub fn is_set(&self) -> bool {
        self.path.exists()
    }

    /// Idempotent both ways: repeated `Locked` reports and `Finished` after `Unlocked` are normal.
    /// Log and swallow failures; a missed set costs a possible relock, a missed clear one password
    /// prompt.
    pub fn apply(&self, change: SessionLock) {
        match change {
            SessionLock::Taken => {
                if let Err(err) = std::fs::File::create(&self.path) {
                    eprintln!(
                        "lock: could not write {} ; a Supervisor restart will not know the session is locked: {err}",
                        self.path.display()
                    );
                }
            }
            SessionLock::Released => {
                if let Err(err) = std::fs::remove_file(&self.path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!(
                        "lock: could not remove {} ; the next Supervisor start will lock the screen: {err}",
                        self.path.display()
                    );
                }
            }
            SessionLock::Unchanged => {}
        }
    }
}

/// Whether `secure_submit(lock, authenticate)` may start PAM. Without `active`, any config
/// textfield gets an unbounded password oracle; without `!authenticating`, held Enter spawns one
/// re-exec'd worker per keypress, each retaining a plaintext secret and paying `pam_unix`'s delay.
pub fn may_authenticate(state: &LockState) -> bool {
    state.active && !state.authenticating
}

/// Whether `acquisition` from [`LockController::try_begin_authentication`] still names the lock on
/// screen. Lock N can finish before a stale answer while N+1 makes `active` true again; only the
/// number binds the password to its lock. It also covers teardown clearing `authenticating` while
/// worker N runs, which could otherwise admit worker N+1 with two plaintext copies (only one can
/// apply; the older dies within `PAM_EXCHANGE_TIMEOUT`). Mismatches drop safely, with
/// `pam_worker::ReportOnDrop` and every [`apply`] arm clearing `active`/`acquisition` also clearing
/// `authenticating`.
pub fn accepts_outcome(state: &LockState, acquisition: u64) -> bool {
    state.active && state.acquisition == acquisition
}

/// The lock screen's failed-authentication line, not `oblisk.rescue`, which ordinary config
/// surfaces draw behind the lock (ADR-0052 decision 4). `Success` has no message. Polkit uses the
/// same words.
pub(crate) fn error_for_outcome(outcome: &shared::PamOutcome) -> String {
    match outcome {
        shared::PamOutcome::Success => String::new(),
        shared::PamOutcome::AuthFailed => "authentication failed".to_string(),
        shared::PamOutcome::MaxTries => "too many attempts".to_string(),
        shared::PamOutcome::StartFailed(err) => format!("could not start authentication: {err}"),
        shared::PamOutcome::PamError(err) => format!("authentication error: {err}"),
    }
}

/// Owns [`LockState`] and the outbound `SetSessionLock` queue. Not `Clone`: `main.rs` mutates it
/// inline in `select!` (unlike `KeyboardController`).
pub struct LockController {
    state: Mutex<LockState>,
    commands_tx: UnboundedSender<shared::SetSessionLock>,
}

impl LockController {
    /// `commands_tx` returns commands to `main.rs`, the only holder of the authoritative generation
    /// id, which a swap reassigns.
    pub fn new(commands_tx: UnboundedSender<shared::SetSessionLock>) -> Self {
        Self { state: Mutex::new(LockState::default()), commands_tx }
    }

    /// Sends `lock()`, unless a lock is already active. Renderer answers `Nothing` for
    /// `(locked: true, lock_held: true)` without `LockReport`, so recording that request would shut
    /// the swap gate forever.
    ///
    /// ponytail: `active` can lag Renderer by one socket hop while `Finished` is in flight, so a
    /// new request can be dropped.
    /// Upgrade by reporting `Nothing` as a `LockOutcome`, giving every request an
    /// event and removing this guard.
    pub fn lock(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if state.active {
                eprintln!("lock: a lock is already held; dropping a lock() that cannot change anything");
                return;
            }
            apply(&mut state, LockEvent::LockRequested);
        }
        self.send(shared::SetSessionLock { locked: true });
    }

    /// Orders unlock only from `main.rs`'s `secure_submit(lock, authenticate)` arm after
    /// `PamOutcome::Success`; it is absent from [`dispatch`] so ADR-0042 is checkable in one arm.
    /// Records no event: Renderer `Unlocked` clears `active`.
    pub fn unlock(&self) {
        self.send(shared::SetSessionLock { locked: false });
    }

    pub fn record(&self, event: LockEvent) {
        apply(&mut self.state.lock().unwrap(), event);
    }

    pub fn snapshot(&self) -> LockState {
        self.state.lock().unwrap().clone()
    }

    /// Reads [`defers_swap`] for `main.rs`'s `TopologyChanged` gate.
    pub fn defers_swap(&self) -> bool {
        defers_swap(&self.state.lock().unwrap())
    }

    /// Atomically admits and marks one PAM conversation, or refuses. `main.rs` spawns it, so a
    /// getter plus record would race. `Some` carries the acquisition for
    /// [`Self::record_authentication`], the only moment the worker's lock number is known.
    pub fn try_begin_authentication(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        if !may_authenticate(&state) {
            return None;
        }
        apply(&mut state, LockEvent::AuthenticationStarted);
        Some(state.acquisition)
    }

    /// Applies a PAM answer to its acquisition or refuses it. `main.rs`'s `pam_outcomes` arm then
    /// avoids unlocking a lock that no longer exists (see [`accepts_outcome`]).
    pub fn record_authentication(&self, acquisition: u64, outcome: shared::PamOutcome) -> bool {
        let mut state = self.state.lock().unwrap();
        if !accepts_outcome(&state, acquisition) {
            return false;
        }
        apply(&mut state, LockEvent::Authenticated(outcome));
        true
    }

    /// A closed channel means `main.rs`'s loop is gone; log and drop.
    fn send(&self, command: shared::SetSessionLock) {
        if self.commands_tx.send(command).is_err() {
            eprintln!("lock: the command channel is closed; dropping {command:?}");
        }
    }
}

/// Every action `oblisk.lock:invoke(...)` accepts. There is no `unlock`: a lock screen's Lua button
/// callback would make it a one-click path past PAM, forbidden by ADR-0042. Unknown `"unlock"` is
/// logged and dropped.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LockAction {
    Lock,
}

/// `oblisk.lock` action dispatch (ADR-0037). `lock` takes no arguments, so it has no `parse_*_args`
/// sibling.
pub fn dispatch(controller: &LockController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<LockAction>(params) else { return };
    match action {
        LockAction::Lock => controller.lock(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_compositor_confirming_a_lock_sets_the_marker_and_only_a_release_clears_it() {
        // A refusal never took a lock, so it must leave whatever the marker already said alone.
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Locked), SessionLock::Taken);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Unlocked), SessionLock::Released);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Finished), SessionLock::Released);
        assert_eq!(
            compositor_lock_change(&shared::LockOutcome::Refused("no lock node".into())),
            SessionLock::Unchanged
        );
    }

    #[test]
    fn a_renderer_that_died_holding_the_lock_leaves_the_marker_set() {
        // RendererLost clears active because this shell holds nothing, but the compositor stays
        // locked. The marker is driven off LockOutcome, not active, so RendererLost (not a
        // Reported) cannot reach it.
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("oblisk-session-locked"));
        flag.apply(compositor_lock_change(&shared::LockOutcome::Locked));

        let mut state = LockState::default();
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        apply(&mut state, LockEvent::RendererLost);

        assert!(!state.active, "the crash means this shell holds nothing");
        assert!(flag.is_set(), "but the session is still locked, and the marker is what says so");
    }

    #[test]
    fn the_marker_survives_the_process_that_wrote_it_and_reads_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-session-locked");
        assert!(!SessionLockedFlag::at(path.clone()).is_set(), "a fresh login has no marker");

        SessionLockedFlag::at(path.clone()).apply(SessionLock::Taken);
        // A different SessionLockedFlag value entirely -- what a restarted Supervisor is.
        assert!(SessionLockedFlag::at(path.clone()).is_set());

        SessionLockedFlag::at(path.clone()).apply(SessionLock::Released);
        assert!(!SessionLockedFlag::at(path).is_set());
    }

    #[test]
    fn applying_the_same_change_twice_is_not_an_error() {
        // Both directions repeat in ordinary use. Removing a file that isn't there must not be
        // treated as a failure to clear.
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("oblisk-session-locked"));
        flag.apply(SessionLock::Released);
        assert!(!flag.is_set());
        flag.apply(SessionLock::Taken);
        flag.apply(SessionLock::Taken);
        assert!(flag.is_set());
        flag.apply(SessionLock::Released);
        flag.apply(SessionLock::Released);
        assert!(!flag.is_set());
    }

    #[test]
    fn an_unchanged_verdict_touches_nothing_in_either_direction() {
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("oblisk-session-locked"));
        flag.apply(SessionLock::Unchanged);
        assert!(!flag.is_set());
        flag.apply(SessionLock::Taken);
        flag.apply(SessionLock::Unchanged);
        assert!(flag.is_set(), "a refusal after a real lock must not erase it");
    }

    use super::*;

    /// A state mid-session with one failure already recorded, so a transition's effect on
    /// `attempts`/`error` is visible against the default. `acquisition` is non-zero for the same
    /// reason: a transition that renumbered the lock would otherwise look like one that didn't.
    fn locked_with_one_failure() -> LockState {
        LockState {
            active: true,
            authenticating: true,
            attempts: 1,
            error: "authentication failed".to_string(),
            requested: false,
            acquisition: 4,
        }
    }

    #[test]
    fn losing_the_renderer_clears_active_so_a_replacement_may_request_the_lock_again() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        assert!(!state.active, "the lock object died with the process that held it");
        assert!(!state.requested, "a request nothing will answer must not keep the swap gate shut forever");
        // LockController::lock refuses while active. Without clearing it here, the replacement's
        // re-acquisition is dropped before it reaches the wire (ADR-0058 decision 4).
    }

    #[test]
    fn losing_the_renderer_rejects_a_pam_answer_that_was_already_in_flight() {
        let mut state = locked_with_one_failure();
        let in_flight = state.acquisition;

        apply(&mut state, LockEvent::RendererLost);

        assert!(
            !accepts_outcome(&state, in_flight),
            "a password answered against a lock whose holder has since died must not be applied: the unlock it would \
             order is an unlock nobody authenticated for"
        );
    }

    #[test]
    fn losing_the_renderer_releases_the_authenticating_flag_it_was_holding() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        // A stranded authenticating flag would refuse every future attempt via may_authenticate.
        assert!(!state.authenticating, "the conversation's answer can no longer be applied, so its slot must be free");
    }

    #[test]
    fn losing_the_renderer_does_not_renumber_the_acquisition() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        // Only a confirmed Locked moves acquisition; bumping here would number a lock never taken.
        assert_eq!(state.acquisition, 4);
    }

    #[test]
    fn lock_requested_clears_a_previous_attempts_refusal_reason_without_claiming_the_lock() {
        let mut state = LockState { error: "no lock node is declared".to_string(), ..LockState::default() };
        apply(&mut state, LockEvent::LockRequested);
        assert_eq!(
            state,
            LockState { requested: true, ..LockState::default() },
            "a fresh lock() must not show the last attempt's reason, and must not claim active before the Renderer reports it"
        );
    }

    #[test]
    fn a_lock_request_defers_a_swap_before_the_renderer_has_confirmed_it() {
        // ADR-0042: the gate shuts when the order goes out, not when the compositor confirms
        // it -- that window can be long, and a swap inside it reaps the process owning the lock.
        let mut state = LockState::default();
        assert!(!defers_swap(&state), "an untouched session defers nothing");

        apply(&mut state, LockEvent::LockRequested);
        assert!(defers_swap(&state), "the order is out; the holder-to-be must not be reaped now");
        assert!(!state.active, "and `active` still moves only on the Renderer's own report");
    }

    #[test]
    fn every_way_a_lock_request_resolves_reopens_the_swap_gate() {
        // A resolution that forgot to clear `requested` would defer reloads for the rest of the
        // session.
        for outcome in [
            shared::LockOutcome::Refused("no lock node is declared".to_string()),
            shared::LockOutcome::Finished,
            shared::LockOutcome::Unlocked,
        ] {
            let mut state = LockState::default();
            apply(&mut state, LockEvent::LockRequested);
            apply(&mut state, LockEvent::Reported(outcome.clone()));
            assert!(!defers_swap(&state), "{outcome:?} resolved the request, so a swap may run again");
        }

        // `Locked` resolves the request too. The gate stays shut, but on `active` now.
        let mut state = LockState::default();
        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(
            defers_swap(&state) && state.active && !state.requested,
            "a confirmed lock hands the gate over to `active`"
        );

        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Unlocked));
        assert!(!defers_swap(&state));
    }

    #[test]
    fn authentication_needs_a_held_lock_and_no_attempt_already_in_flight() {
        assert!(
            !may_authenticate(&LockState::default()),
            "an unlocked session must not be a PAM oracle any config can drive from a bar textfield"
        );
        assert!(
            !may_authenticate(&LockState { requested: true, ..LockState::default() }),
            "an unconfirmed request is not a lock screen on the glass yet"
        );
        assert!(may_authenticate(&LockState { active: true, ..LockState::default() }));
        assert!(
            !may_authenticate(&LockState { active: true, authenticating: true, ..LockState::default() }),
            "a held-down Enter key must not spawn one PAM worker per keypress"
        );
    }

    #[test]
    fn locked_marks_the_session_active_and_resets_the_attempt_counter() {
        let mut state = LockState { attempts: 3, error: "authentication failed".to_string(), ..LockState::default() };
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: false,
                attempts: 0,
                error: String::new(),
                requested: false,
                acquisition: 1
            }
        );
    }

    #[test]
    fn refused_records_the_reason_and_leaves_the_session_unlocked() {
        let mut state = LockState::default();
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Refused("no lock node is declared".to_string())));
        assert_eq!(
            state,
            LockState {
                active: false,
                authenticating: false,
                attempts: 0,
                error: "no lock node is declared".to_string(),
                requested: false,
                acquisition: 0
            }
        );
    }

    #[test]
    fn authentication_started_sets_authenticating() {
        let mut state = LockState { active: true, ..LockState::default() };
        apply(&mut state, LockEvent::AuthenticationStarted);
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: true,
                attempts: 0,
                error: String::new(),
                requested: false,
                acquisition: 0
            }
        );
    }

    #[test]
    fn a_failed_authentication_counts_an_attempt_and_keeps_the_session_locked() {
        let mut state = LockState { active: true, authenticating: true, ..LockState::default() };
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: false,
                attempts: 1,
                error: "authentication failed".to_string(),
                requested: false,
                acquisition: 0
            }
        );

        // Two identical consecutive failures are one unchanged error string -- the counter is the
        // only thing that tells the config the second one happened.
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(state.attempts, 2);
    }

    #[test]
    fn a_successful_authentication_stops_authenticating_but_does_not_itself_unlock() {
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: false,
                attempts: 1,
                error: String::new(),
                requested: false,
                acquisition: 4
            },
            "active clears only when the Renderer reports Unlocked -- the lock is on the glass until unlock_and_destroy actually runs"
        );
    }

    #[test]
    fn unlocked_and_finished_both_clear_the_session() {
        for outcome in [shared::LockOutcome::Unlocked, shared::LockOutcome::Finished] {
            let mut state = locked_with_one_failure();
            apply(&mut state, LockEvent::Reported(outcome.clone()));
            assert_eq!(
                state,
                LockState {
                    active: false,
                    authenticating: false,
                    attempts: 1,
                    error: String::new(),
                    requested: false,
                    acquisition: 4
                },
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn lock_queues_the_set_session_lock_command_and_records_the_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        controller.lock();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(!controller.snapshot().active, "the lock is not active until the Renderer reports Locked");
        assert!(controller.defers_swap(), "but the swap gate is already shut -- the order is in flight");

        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(controller.snapshot().active);

        // main.rs's PAM-outcome arm is this method's only caller.
        controller.unlock();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: false }));
        assert!(
            controller.snapshot().active,
            "active clears on the Renderer's Unlocked report, not on the order going out"
        );
    }

    #[test]
    fn try_begin_authentication_admits_exactly_one_attempt_at_a_time() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        assert!(
            controller.try_begin_authentication().is_none(),
            "no lock is held, so there is nothing to authenticate against"
        );
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let acquisition = controller.try_begin_authentication().expect("a held lock admits the first attempt");
        assert!(controller.snapshot().authenticating);
        assert!(
            controller.try_begin_authentication().is_none(),
            "the second submission must find the first still in flight"
        );

        assert!(controller.record_authentication(acquisition, shared::PamOutcome::AuthFailed));
        assert!(controller.try_begin_authentication().is_some(), "the worker's answer released it");
    }

    #[test]
    fn a_stale_pam_outcome_cannot_release_the_lock_that_replaced_the_one_it_authenticated_against() {
        // The bypass, in order: a worker started against lock N is still running (pam_unix ~1s,
        // PAM_EXCHANGE_TIMEOUT allows 30) when the compositor ends lock N and something else
        // takes lock N+1. Nothing about active/authenticating distinguishes the two by then; only
        // the acquisition number does.
        let mut state = LockState::default();
        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        apply(&mut state, LockEvent::AuthenticationStarted);
        let acquisition = state.acquisition;
        assert!(
            accepts_outcome(&state, acquisition),
            "the ordinary case: the answer is about the lock still on the glass"
        );

        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Finished));
        assert!(
            !accepts_outcome(&state, acquisition),
            "the lock the password was typed against is gone; the answer is about nothing"
        );

        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(
            !accepts_outcome(&state, acquisition),
            "a new lock is on the glass and nobody has authenticated against it -- accepting the old worker's Success here is the bypass"
        );
        assert!(accepts_outcome(&state, state.acquisition), "and the new lock's own attempt is still accepted");
    }

    #[test]
    fn a_stale_failure_neither_counts_against_the_new_lock_nor_overwrites_its_message() {
        // The same binding, on the other outcome: a stale failure landing after the new
        // acquisition's reset would show the user a failure they never made.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let stale = controller.try_begin_authentication().expect("a held lock admits the attempt");
        controller.record(LockEvent::Reported(shared::LockOutcome::Finished));
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));

        assert!(
            !controller.record_authentication(stale, shared::PamOutcome::AuthFailed),
            "the refusal is reported so main.rs can log it"
        );

        let state = controller.snapshot();
        assert_eq!(state.attempts, 0, "the new lock has seen no attempts");
        assert_eq!(state.error, "", "and nothing to say about one");
    }

    #[test]
    fn a_second_lock_while_one_is_held_is_dropped_rather_than_left_unresolved() {
        // The Renderer answers Nothing to a locked:true it already holds, with no LockReport --
        // so a requested set here would have no event to clear it.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        controller.lock();
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        controller.lock();

        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(rx.try_recv().is_err(), "the second lock() cannot change anything, so it is not sent either");
        assert!(
            !controller.snapshot().requested,
            "and above all it does not shut the swap gate on an event that is never coming"
        );
    }

    #[test]
    fn dispatch_routes_lock_and_ignores_an_unknown_action() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        for action in ["lock", "nonsense"] {
            dispatch(&controller, &envelope(action));
        }

        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(rx.try_recv().is_err(), "an unknown action must be logged, not turned into a command");
    }

    #[test]
    fn dispatch_refuses_to_unlock() {
        // ADR-0042: an unlock action would make walking past PAM one mouse click. The
        // asymmetry with lock is deliberate, pinned so it isn't re-added as an oversight.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        dispatch(&controller, &envelope("unlock"));

        assert!(rx.try_recv().is_err(), "unlock must not be commandable from Lua");
    }

    fn envelope(action: &str) -> shared::CommandEnvelope {
        shared::CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "command".to_string(),
            params: shared::CommandParams {
                generation_id: 0,
                capability: "lock".to_string(),
                action: action.to_string(),
                arguments: Vec::new(),
                expected_revision: 0,
            },
            id: 1,
        }
    }
}
