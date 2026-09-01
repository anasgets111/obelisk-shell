//! `oblisk.lock`: the session-lock command and the state a lock screen is built from
//! (ADR-0042, ADR-0052 decisions 1 and 4).
//!
//! The Renderer holds `ext_session_lock_v1` and paints it; this owns the decision to take it, the
//! record of what became of it, and the one call site allowed to order an unlock. Every state
//! change arrives as a [`LockEvent`] and is applied by [`apply`], a pure function, so the whole
//! transition table is testable without a socket or a PAM stack.

use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

/// `oblisk.lock`'s payload (ADR-0052 decision 4). `attempts` counts failed authentications
/// since acquisition, and exists because Lua can't rebuild it: capability state is sampled at
/// layout time (ADR-0044), not evented, so two identical consecutive failures are one
/// unchanged `error` string. `error`'s "nothing went wrong" value is the empty string, the same
/// convention `keyboard`'s `active_layout` uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct LockState {
    /// The session is locked and the Renderer has confirmed it. Never optimistic: a lock that has
    /// been asked for but not yet confirmed still reads `false`, so a config cannot draw an
    /// unlocked screen over a locked session or the reverse.
    pub active: bool,
    /// A password is with PAM and no answer has come back. `pam_unix` takes about a second, so this
    /// is what a spinner reads. `lock:authenticate` is refused while it is true.
    pub authenticating: bool,
    /// Authentication attempts against the lock currently held. Counts every answer PAM returns,
    /// success included, and resets to `0` only when the Renderer confirms a *new* lock. So it is
    /// per-acquisition rather than per-failure: a lockout rule reads it together with
    /// [`LockState::error`], which is empty after the attempt that succeeded.
    pub attempts: u32,
    /// Why the last attempt failed, in words fit to draw, e.g. `"too many attempts"`. Empty string
    /// when the last attempt succeeded and when none has been made. Rewritten on every PAM answer
    /// and cleared when a new lock is confirmed, so it always describes the lock now on screen.
    pub error: String,
    /// A `SetSessionLock { locked: true }` is out and the Renderer has not said what became of it
    /// yet. `#[serde(skip)]`: this is a fact about the swap gate, not part of a lock screen's
    /// payload. It exists because `active` must keep meaning only "the Renderer confirmed it"
    /// ([`apply`]'s invariant), while the swap gate has to shut a round trip earlier:
    /// `ext_session_lock_v1` lets the compositor withhold `locked` until the client has presented
    /// on every output, and a swap started in that window reaps the process holding the lock
    /// object and locks the user out for good (ADR-0042).
    #[serde(skip)]
    pub requested: bool,
    /// Which acquisition of the lock this state describes: bumped only when the Renderer reports
    /// `Locked`. `#[serde(skip)]` for the same reason as `requested`.
    ///
    /// Exists because a PAM outcome outlives the lock it was started for: `pam_unix` takes about
    /// a second and `PAM_EXCHANGE_TIMEOUT` allows thirty, and inside that window the compositor
    /// can end the lock (`finished` after `locked`, what `loginctl unlock-session` produces) and
    /// an idle timer can take a new one. Without a number tying an answer to its question, a
    /// stale success releases a lock nobody authenticated against. See [`accepts_outcome`].
    #[serde(skip)]
    pub acquisition: u64,
}

/// Everything that can move a [`LockState`]. The Supervisor learns of the lock from three
/// unrelated places -- a Lua `lock()` call, its own PAM worker, and the Renderer holding the
/// protocol object -- and this enum lets all three land in one pure [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// `lock.lock()` reached [`dispatch`] and the `SetSessionLock` is on its way out.
    LockRequested,
    /// A `secure_submit(lock, authenticate)` arrived and its PAM conversation is starting.
    AuthenticationStarted,
    /// That conversation's answer, straight from the re-exec'd worker (ADR-0028). Reached
    /// only through [`LockController::record_authentication`], which checks the answer against
    /// the lock it was started for.
    Authenticated(shared::PamOutcome),
    /// The Renderer holding the lock said what became of it.
    Reported(shared::LockOutcome),
    /// The process holding `ext_session_lock_v1` died without reporting anything (ADR-0058
    /// decision 4). Not a [`Self::Reported`]: there is no holder left to describe anything. The
    /// session is still locked at the compositor, which does not unlock on client death; what
    /// ended is this shell's ability to speak for it.
    RendererLost,
}

/// The whole transition table, pure and synchronous so every case is unit-testable without a
/// socket or a real PAM stack.
///
/// The invariant worth stating once: only the Renderer's own report moves `active`. Neither the
/// request to lock nor a successful password moves it -- both are orders the compositor has not
/// confirmed yet, and a lock screen that believed either would paint the wrong thing.
pub fn apply(state: &mut LockState, event: LockEvent) {
    match event {
        // Drop the previous attempt's refusal reason: a config fixed by an in-place reload must
        // not keep showing why the old config failed (ADR-0052 decision 3).
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
        // The one place `acquisition` moves: a confirmed lock is a different lock from the one
        // before it, and every answer still in flight from the previous one is about a lock
        // that's gone (see [`accepts_outcome`]).
        LockEvent::Reported(shared::LockOutcome::Locked) => {
            state.active = true;
            state.requested = false;
            state.authenticating = false;
            state.attempts = 0;
            state.error.clear();
            state.acquisition += 1;
        }
        // `active` stays false: nothing was ever taken, so nothing was protected a moment
        // earlier either (ADR-0052 decision 3).
        LockEvent::Reported(shared::LockOutcome::Refused(reason)) => {
            state.requested = false;
            state.authenticating = false;
            state.error = reason;
        }
        // `Finished` after `Locked` is a compositor teardown, not a failure this capability
        // reports: with the lock surfaces gone, the Renderer sets `oblisk.rescue` itself
        // (ADR-0052 decision 4). `attempts` is left alone -- it resets on the next
        // acquisition, the only point a count "since acquired" means anything.
        LockEvent::Reported(shared::LockOutcome::Finished | shared::LockOutcome::Unlocked) => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
            state.error.clear();
        }
        // `active` means "this shell holds the lock", false after a crash even though the
        // session is still locked. Clearing it lets a replacement re-acquire at all
        // ([`LockController::lock`] drops a request while `active`). `authenticating` clears in
        // the same arm (see [`accepts_outcome`]): every transition clearing `active` releases the
        // conversation slot too. `acquisition` does not move -- only a confirmed `Locked` numbers
        // a lock. `error` is left alone so a refusal's reason survives.
        LockEvent::RendererLost => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
        }
    }
}

/// ADR-0042: what defers a generation swap. Only one client may hold a session lock, so
/// candidate `N+1` cannot acquire the one generation `N` holds or is mid-acquiring. `requested`
/// is the half a report hasn't resolved yet: ignoring it would reap the lock holder mid-handshake
/// and leave the compositor locked with nobody able to unlock it.
pub fn defers_swap(state: &LockState) -> bool {
    state.active || state.requested
}

/// What one [`shared::LockOutcome`] says about the compositor's session lock, a different
/// question from the one [`apply`] answers (ADR-0060).
///
/// `LockState.active` means "this shell holds the lock". The compositor's lock outlives that:
/// `RendererLost` clears `active` while the session stays locked (the compositor does not unlock
/// on client death), so reading the marker off `active` would erase the fact a restarted
/// Supervisor needs. Read off the outcome instead, where `RendererLost` cannot reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLock {
    Taken,
    Released,
    Unchanged,
}

/// Matched exhaustively: a new [`shared::LockOutcome`] variant is a new answer to "is the session
/// locked", and the compiler should force a decision rather than defaulting to
/// [`SessionLock::Unchanged`] on a path this quiet.
pub fn compositor_lock_change(outcome: &shared::LockOutcome) -> SessionLock {
    match outcome {
        shared::LockOutcome::Locked => SessionLock::Taken,
        shared::LockOutcome::Unlocked | shared::LockOutcome::Finished => SessionLock::Released,
        // Nothing was taken, so nothing was released. A refusal must leave an earlier
        // generation's marker alone: the session it describes is still locked.
        shared::LockOutcome::Refused(_) => SessionLock::Unchanged,
    }
}

/// The one piece of lock state that outlives the Supervisor process (ADR-0060): a file in
/// `$XDG_RUNTIME_DIR` that exists exactly while the compositor is locked.
///
/// Exists because a restarted Supervisor's [`LockState`] is `Default`, so `active` reads false
/// while the compositor is still locked from before, and the new shell paints its bar behind a
/// lock fallback nothing can see (ADR-0058, 0059).
///
/// A file, not anything richer: the question is a boolean and the storage must survive
/// `SIGKILL`. `$XDG_RUNTIME_DIR`, not config/state: it goes away with the user's last session,
/// bounding how stale the answer can get.
pub struct SessionLockedFlag {
    path: std::path::PathBuf,
}

impl SessionLockedFlag {
    /// The marker at an explicit path. `main.rs` builds one from
    /// [`shared::session_locked_flag_path`]; tests build one in a temp directory.
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// Whether the compositor was locked when whoever wrote this last spoke.
    ///
    /// A read error reads as "not locked", the wrong direction on purpose: the alternative is a
    /// Supervisor that can't read one file and locks the screen at every boot until someone works
    /// out why.
    pub fn is_set(&self) -> bool {
        self.path.exists()
    }

    /// Idempotent in both directions -- both repeat in ordinary use: two `Locked` reports across
    /// two acquisitions, and a `Finished` landing after an `Unlocked` already cleared it.
    ///
    /// Failures are logged and swallowed: a marker that failed to appear costs a relock after a
    /// crash that may never happen, and one that failed to clear costs one password prompt.
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

/// Whether a `secure_submit(lock, authenticate)` may start a PAM conversation. Two independent
/// refusals:
///
/// - Without `active`, the capability is an unbounded password oracle: nothing stops a config
///   textfield calling it, and build-steps.md Phase 23 item 3 scopes authentication to the lock
///   screen, which is exactly `active`.
/// - Without `!authenticating`, a held-down Enter key spawns one re-exec'd PAM worker per
///   keypress, each holding a plaintext secret copy and burning `pam_unix`'s failure delay.
pub fn may_authenticate(state: &LockState) -> bool {
    state.active && !state.authenticating
}

/// Whether an answer tagged `acquisition` (from [`LockController::try_begin_authentication`]) is
/// still about the lock on the glass. `active` catches an answer arriving after the compositor
/// tore the lock down with nothing taking a new one. `acquisition` catches the bypass: between
/// the worker starting and answering, a `Finished` can end lock N and something else take lock
/// N+1, so `active` is true again by the time the answer lands and only the number says the
/// password was typed against a lock that's gone.
///
/// This also bounds the hole `!authenticating` alone can't close: a teardown clears
/// `authenticating` while the first worker still runs, admitting a second against the new lock
/// with two plaintext copies live at once, bound to different acquisitions -- at most one answer
/// is ever applied, and the older worker's copy dies with it inside `PAM_EXCHANGE_TIMEOUT`.
///
/// Dropping the event on a mismatch cannot strand `authenticating` (what `pam_worker::
/// ReportOnDrop` guards against): every [`apply`] arm clearing `active` or moving `acquisition`
/// clears `authenticating` in the same arm.
pub fn accepts_outcome(state: &LockState, acquisition: u64) -> bool {
    state.active && state.acquisition == acquisition
}

/// The line a lock screen renders for a failed authentication, not `oblisk.rescue`: rescue is
/// drawn by the config's ordinary surfaces, exactly what the lock is hiding (ADR-0052
/// decision 4). `Success` is not a failure and [`apply`] never asks for its message.
fn error_for_outcome(outcome: &shared::PamOutcome) -> String {
    match outcome {
        shared::PamOutcome::Success => String::new(),
        shared::PamOutcome::AuthFailed => "authentication failed".to_string(),
        shared::PamOutcome::MaxTries => "too many attempts".to_string(),
        shared::PamOutcome::StartFailed(err) => format!("could not start authentication: {err}"),
        shared::PamOutcome::PamError(err) => format!("authentication error: {err}"),
    }
}

/// Owns [`LockState`] and the outbound `SetSessionLock` queue. Not `Clone` (unlike
/// `KeyboardController`): every mutation happens inline in `main.rs`'s `select!`, so no spawned
/// task needs a copy.
pub struct LockController {
    state: Mutex<LockState>,
    commands_tx: UnboundedSender<shared::SetSessionLock>,
}

impl LockController {
    /// `commands_tx` carries each command back to `main.rs`, not to a socket directly: `main.rs`
    /// is the only holder of the authoritative generation id, and a swap reassigns it.
    pub fn new(commands_tx: UnboundedSender<shared::SetSessionLock>) -> Self {
        Self { state: Mutex::new(LockState::default()), commands_tx }
    }

    /// `lock.lock()`.
    ///
    /// A `lock()` against a lock already on the glass is dropped here, not recorded and sent:
    /// `requested` must never be set by a request nothing will resolve. The Renderer's
    /// `lock_command` answers `Nothing` for `(locked: true, lock_held: true)`, and `Nothing`
    /// emits no `LockReport` -- recording the request would shut the swap gate on an event that
    /// never comes.
    ///
    /// ponytail: this reads `active` to predict what the Renderer already holds, and the two
    /// disagree for as long as a `Finished` report is in flight -- the compositor ended the lock,
    /// the Renderer holds nothing, and a `lock()` landing in that window is dropped instead of
    /// taking a new lock. The window is one socket hop, and the failure is a lost lock request,
    /// not a wrongly released lock, which is why this is the direction to be wrong in. Upgrade
    /// path: have the Renderer report `Nothing` as a `LockOutcome` too, so every request has a
    /// resolving event and this guard goes away.
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

    /// Orders the unlock. Not reachable from Lua, deliberately absent from [`dispatch`]: its only
    /// caller is `main.rs`'s `secure_submit(lock, authenticate)` arm on a `PamOutcome::Success`,
    /// making ADR-0042's "never unlock except on a successful authentication" checkable by
    /// reading one arm.
    ///
    /// Records no event: `active` clears when the Renderer reports `Unlocked`, not when the order
    /// goes out, per [`apply`]'s invariant.
    pub fn unlock(&self) {
        self.send(shared::SetSessionLock { locked: false });
    }

    pub fn record(&self, event: LockEvent) {
        apply(&mut self.state.lock().unwrap(), event);
    }

    pub fn snapshot(&self) -> LockState {
        self.state.lock().unwrap().clone()
    }

    /// [`defers_swap`] against the live state -- `main.rs`'s `TopologyChanged` gate.
    pub fn defers_swap(&self) -> bool {
        defers_swap(&self.state.lock().unwrap())
    }

    /// Admits at most one PAM conversation at a time and marks it started, or refuses. One
    /// method, not a `may_authenticate` getter followed by a `record`: the check and the mark
    /// must happen under the same lock, since `main.rs` spawns the conversation rather than
    /// awaiting it inline.
    ///
    /// The `Some` carries the acquisition the conversation is about, to be carried back to
    /// [`Self::record_authentication`] -- handed out here because a worker outlives the lock it
    /// was started for, and this is the only moment the right number is knowable.
    pub fn try_begin_authentication(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        if !may_authenticate(&state) {
            return None;
        }
        apply(&mut state, LockEvent::AuthenticationStarted);
        Some(state.acquisition)
    }

    /// Applies a PAM answer to the lock it was started for, or refuses it. Returns whether it was
    /// applied, so the caller (`main.rs`'s `pam_outcomes` arm) knows not to order an unlock: a
    /// refused `Success` is a password typed against a lock that no longer exists (see
    /// [`accepts_outcome`]).
    pub fn record_authentication(&self, acquisition: u64, outcome: shared::PamOutcome) -> bool {
        let mut state = self.state.lock().unwrap();
        if !accepts_outcome(&state, acquisition) {
            return false;
        }
        apply(&mut state, LockEvent::Authenticated(outcome));
        true
    }

    /// A closed channel means `main.rs`'s loop is gone -- logged and dropped.
    fn send(&self, command: shared::SetSessionLock) {
        if self.commands_tx.send(command).is_err() {
            eprintln!("lock: the command channel is closed; dropping {command:?}");
        }
    }
}

/// Every action `oblisk.lock:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
///
/// Locking is the one direction a config may command. There is no `unlock` variant: a lock
/// screen's `button` callbacks run while its Lua tree is the only thing on the glass, so an
/// `unlock` action would be a one-click path past PAM -- exactly what ADR-0042 forbids.
/// `"unlock"` names no variant, so it is logged and dropped like any other unanswered name.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LockAction {
    Lock,
}

/// `oblisk.lock`'s action dispatch (ADR-0037). `lock` takes no arguments, so unlike
/// `keyboard` there is no `parse_*_args` sibling.
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
