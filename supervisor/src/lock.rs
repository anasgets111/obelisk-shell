//! `oblisk.lock`: the session-lock command and the state a lock screen is built from
//! (docs/adr/0042, docs/adr/0052 decisions 1 and 4).
//!
//! The Renderer holds `ext_session_lock_v1` and paints it; this owns the decision to take it, the
//! record of what became of it, and the one call site allowed to order an unlock. Every state
//! change arrives as a [`LockEvent`] and is applied by [`apply`], a pure function, so the whole
//! transition table is testable without a socket or a PAM stack.

use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

/// `oblisk.lock`'s payload (docs/adr/0052 decision 4). `attempts` counts failed authentications
/// since the lock was acquired, and exists because the config cannot reconstruct it: capability
/// state is sampled at layout time (docs/adr/0044), not evented, so two consecutive identical
/// failures are one unchanged `error` string and a counter built in Lua would miss the second.
/// `error` is a plain string whose "nothing went wrong" value is the empty string -- the same
/// convention `keyboard`'s `active_layout` already uses, rather than a nullable the IDL has no
/// shape for.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct LockState {
    pub active: bool,
    pub authenticating: bool,
    pub attempts: u32,
    pub error: String,
    /// A `SetSessionLock { locked: true }` is out and the Renderer has not said what became of
    /// it yet. Not part of `oblisk.lock`'s payload -- `#[serde(skip)]` keeps docs/adr/0052
    /// decision 4's four fields the only thing a lock screen sees, because this is a fact about
    /// the Supervisor's own swap gate, not about anything a config could paint. It exists
    /// because `active` must keep meaning exactly what [`apply`]'s invariant says (only the
    /// Renderer's report moves it), while the swap gate has to shut a round trip earlier:
    /// `ext_session_lock_v1` lets the compositor withhold `locked` until the client has
    /// presented a lock surface on every output, and a swap started in that window reaps the
    /// process holding the lock object, locking the user out for good (docs/adr/0042).
    #[serde(skip)]
    pub requested: bool,
    /// Which acquisition of the lock this state describes: bumped every time the Renderer reports
    /// a `Locked`, never otherwise. `#[serde(skip)]` for the same reason as `requested` -- it is a
    /// fact about which lock the Supervisor is talking to, not a field docs/adr/0052 decision 4
    /// gives a lock screen.
    ///
    /// It exists because a PAM outcome outlives the lock it was started for. `pam_unix` takes
    /// about a second and `PAM_EXCHANGE_TIMEOUT` allows thirty, and inside that window the
    /// compositor can end the lock through its own mechanism (`finished` after `locked`, which is
    /// what `loginctl unlock-session` produces) and an idle timer can take a *new* one. Without a
    /// number tying an answer to the question it answered, that stale success releases a lock
    /// nobody authenticated against -- an authentication bypass, not the dropped frame the
    /// `pam_outcomes` arm used to assume. See [`accepts_outcome`].
    #[serde(skip)]
    pub acquisition: u64,
}

/// Everything that can move a [`LockState`]. The Supervisor learns of the lock from three
/// unrelated places -- a Lua `lock()` call, its own PAM worker, and the Renderer that holds the
/// protocol object -- and this enum is what lets all three land in one pure [`apply`] instead of
/// three hand-written state mutations scattered across `main.rs`'s `select!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// `lock.lock()` reached [`dispatch`] and the `SetSessionLock` is on its way out.
    LockRequested,
    /// A `secure_submit(lock, authenticate)` arrived and its PAM conversation is starting.
    AuthenticationStarted,
    /// That conversation's answer, straight from the re-exec'd worker (docs/adr/0028). Nothing
    /// outside this module builds one: `main.rs` reaches [`apply`] only through
    /// [`LockController::record_authentication`], which is where the answer is checked against the
    /// lock it was started for. That is the same "one call site by construction" argument
    /// [`LockController::unlock`] makes, and for the same reason -- a binding enforced in one
    /// method is checkable by reading that method.
    Authenticated(shared::PamOutcome),
    /// The Renderer holding the lock said what became of it.
    Reported(shared::LockOutcome),
    /// The process holding `ext_session_lock_v1` died without reporting anything (docs/adr/0058
    /// decision 4). Not a [`Self::Reported`] variant, because every one of those is the holder
    /// describing its own lock, and the whole problem here is that there is no holder left to
    /// describe anything. The *session* is still locked at the compositor, which is required not
    /// to unlock on client death; what ended is this shell's ability to speak for it.
    RendererLost,
}

/// The whole transition table, pure and synchronous so every case is unit-testable without a
/// socket or a real PAM stack -- the same seam `hardware::idle::notify::register_threshold_entry`
/// cuts for the idle fan-out.
///
/// The invariant worth stating once: only the Renderer's own report moves `active`. Neither the
/// request to lock nor a successful password moves it, because both are orders whose effect the
/// compositor has not confirmed yet, and a lock screen that believed either would paint the wrong
/// thing for the round trip.
pub fn apply(state: &mut LockState, event: LockEvent) {
    match event {
        // Drop the previous attempt's refusal reason. A config that declared no `lock` node
        // (docs/adr/0052 decision 3), got refused, and was then fixed by an in-place reload must
        // not keep showing why the *old* config failed.
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
        // The one place `acquisition` moves. A lock the compositor confirmed is a different lock
        // from the one before it, even when the user cannot tell them apart, and every answer
        // still in flight from the previous one is about a lock that is gone (see
        // [`accepts_outcome`]).
        LockEvent::Reported(shared::LockOutcome::Locked) => {
            state.active = true;
            state.requested = false;
            state.authenticating = false;
            state.attempts = 0;
            state.error.clear();
            state.acquisition += 1;
        }
        // `active` stays false: nothing was ever taken, so nothing was protected a moment
        // earlier either (docs/adr/0052 decision 3).
        LockEvent::Reported(shared::LockOutcome::Refused(reason)) => {
            state.requested = false;
            state.authenticating = false;
            state.error = reason;
        }
        // `Finished` after a `Locked` is a compositor teardown, not a failure this capability
        // reports: with the lock surfaces gone the ordinary scene is back on the glass, so
        // `oblisk.rescue` is the channel that reaches the user and the Renderer sets it itself
        // (docs/adr/0052 decision 4). `attempts` is deliberately left alone -- it resets on the
        // next acquisition, which is the only point a count "since the lock was acquired" means
        // anything.
        LockEvent::Reported(shared::LockOutcome::Finished | shared::LockOutcome::Unlocked) => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
            state.error.clear();
        }
        // `active` means "this shell holds the lock", and after a crash it does not, even though
        // the session is still locked. Clearing it is what lets a replacement re-acquire at all:
        // [`LockController::lock`] drops a request while `active`, on the correct assumption that a
        // live Renderer already holds one.
        //
        // `authenticating` clears in the same arm, which is not tidiness but the invariant
        // [`accepts_outcome`] documents: every transition that clears `active` releases the
        // conversation slot too, so an answer this drops is never one whose flag stays stuck. The
        // answer itself is refused by `accepts_outcome`'s `state.active` term, so a password typed
        // into a lock screen whose process then died cannot order an unlock nobody authenticated.
        //
        // `acquisition` deliberately does not move. Only a confirmed `Locked` numbers a lock, and a
        // replacement that re-acquires goes through exactly that; numbering one here would name a
        // lock that was never taken. `error` is left alone so a refusal's reason survives to be
        // read, and `LockEvent::LockRequested` clears it when the next attempt starts.
        LockEvent::RendererLost => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
        }
    }
}

/// docs/adr/0042: what defers a generation swap. Only one client may hold a session lock, so
/// candidate `N+1` cannot acquire the one generation `N` holds -- or is in the middle of
/// acquiring. Both facts gate, and `requested` is the half a report has not resolved yet: the
/// `TopologyChanged` arm that ignored it would `reap_process_group` the lock holder mid-handshake
/// and leave the compositor locked with nobody able to unlock it.
pub fn defers_swap(state: &LockState) -> bool {
    state.active || state.requested
}

/// What one [`shared::LockOutcome`] says about the *compositor's* session lock, which is a
/// different question from the one [`apply`] answers (docs/adr/0060).
///
/// `LockState.active` means "this shell holds the lock". The compositor's lock outlives that:
/// [`LockEvent::RendererLost`] clears `active` while the session stays locked, because the protocol
/// requires a compositor not to unlock when a lock client dies. Reading the marker off `active`
/// would therefore erase the one fact a restarted Supervisor needs, so it is read off the outcome
/// instead, where `RendererLost` cannot reach it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLock {
    Taken,
    Released,
    Unchanged,
}

/// Matched exhaustively rather than with a wildcard: a new [`shared::LockOutcome`] variant is a new
/// answer to "is the session locked", and the compiler should make someone decide it rather than
/// defaulting it to [`SessionLock::Unchanged`] on a path this quiet.
pub fn compositor_lock_change(outcome: &shared::LockOutcome) -> SessionLock {
    match outcome {
        shared::LockOutcome::Locked => SessionLock::Taken,
        shared::LockOutcome::Unlocked | shared::LockOutcome::Finished => SessionLock::Released,
        // Nothing was taken, so nothing was released either. A refusal must leave a marker an
        // earlier generation set alone: the session it describes is still locked.
        shared::LockOutcome::Refused(_) => SessionLock::Unchanged,
    }
}

/// The one piece of lock state that outlives the Supervisor process (docs/adr/0060): a file in
/// `$XDG_RUNTIME_DIR` that exists exactly while the compositor is locked.
///
/// It exists because a restarted Supervisor's [`LockState`] is `Default`, so `active` is false
/// while the compositor is still locked from before, and the new shell paints its bar behind a lock
/// fallback where nothing can see it. That is the failure docs/adr/0058 measured in quickshell and
/// rejected "let a session manager restart the whole stack" over; docs/adr/0059 made the restart
/// real, so this is what keeps that door shut.
///
/// A file rather than anything richer because the question is a boolean and the storage has to
/// survive `SIGKILL`, which rules out every in-process option. `$XDG_RUNTIME_DIR` rather than a
/// config or state directory because it goes away with the user's last session, which is what
/// bounds how stale the answer can get: within one login a set marker means the compositor really
/// was locked and nothing unlocked it.
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
    /// A read error reads as "not locked" the way a missing file does, and that is the wrong
    /// direction on purpose: the alternative is a Supervisor that cannot read one file and
    /// therefore locks the screen at every boot until someone works out why.
    pub fn is_set(&self) -> bool {
        self.path.exists()
    }

    /// Idempotent in both directions, because both repeat in ordinary use: two `Locked` reports
    /// across two acquisitions, and a `Finished` landing after an `Unlocked` already cleared it.
    ///
    /// Failures are logged and swallowed. Neither direction can be made to matter enough to stop a
    /// shell over: a marker that failed to appear costs a relock after a crash that may never
    /// happen, and one that failed to clear costs one password prompt at the next start.
    pub fn apply(&self, change: SessionLock) {
        match change {
            SessionLock::Taken => {
                if let Err(err) = std::fs::File::create(&self.path) {
                    eprintln!("lock: could not write {} ; a Supervisor restart will not know the session is locked: {err}", self.path.display());
                }
            }
            SessionLock::Released => {
                if let Err(err) = std::fs::remove_file(&self.path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!("lock: could not remove {} ; the next Supervisor start will lock the screen: {err}", self.path.display());
                }
            }
            SessionLock::Unchanged => {}
        }
    }
}

/// Whether a `secure_submit(lock, authenticate)` may start a PAM conversation. Two independent
/// refusals, both about who gets to drive real PAM attempts against the session user:
///
/// - Without `active`, the capability is an unbounded password oracle. Nothing stops a config
///   from putting a `("lock", "authenticate")` textfield on the bar, and build-steps.md Phase 23
///   item 3 scopes authentication to the lock screen, which is exactly `active`.
/// - Without `!authenticating`, a held-down Enter key spawns one re-exec'd PAM worker per
///   keypress, each holding a plaintext copy of the secret and each burning `pam_unix`'s failure
///   delay. `authenticating` already exists to say an attempt is in flight (docs/adr/0052
///   decision 4); this is that fact used, not a second concept beside it.
pub fn may_authenticate(state: &LockState) -> bool {
    state.active && !state.authenticating
}

/// Whether an answer tagged `acquisition` -- the value [`LockController::try_begin_authentication`]
/// handed out when the conversation started -- is still about the lock on the glass. Both halves
/// are load-bearing, and each catches a case the other does not:
///
/// - `active` catches the answer that comes back after the compositor tore the lock down and
///   nothing has taken a new one. Applying it would order an unlock of a lock nobody holds and
///   count a failure against a session that is not locked.
/// - The `acquisition` match catches the bypass. Between the worker starting and answering, a
///   `Finished` can end lock N and an idle timer or a lock button can take lock N+1; `active` is
///   true again by the time the answer lands, so only the number says the password was typed
///   against a lock that no longer exists. Applying a `Success` there releases N+1 with nobody
///   having authenticated against it.
///
/// This is also what bounds the one hole [`may_authenticate`]'s `!authenticating` refusal cannot
/// close by itself: a teardown clears `authenticating` while the first worker is still running, so
/// a second one can be admitted against the new lock and two plaintext copies exist at once. They
/// are bound to different acquisitions, so at most one answer can ever be applied, and the older
/// worker's copy dies with it inside `PAM_EXCHANGE_TIMEOUT`. Refusing the second submission
/// instead would mean refusing the new lock's *first* real attempt, which is worse.
///
/// Dropping the whole event on a mismatch cannot strand `authenticating` the way the
/// `pam_worker::ReportOnDrop` guard exists to prevent: `authenticating` is only ever set
/// against an `active` lock, and every report that clears `active` or moves `acquisition` clears
/// `authenticating` in the same arm of [`apply`]. So a dropped answer is always an answer whose
/// own flag some other transition already released.
pub fn accepts_outcome(state: &LockState, acquisition: u64) -> bool {
    state.active && state.acquisition == acquisition
}

/// The line a lock screen renders for a failed authentication. It goes here rather than to
/// `oblisk.rescue` because rescue is drawn by the config's ordinary surfaces, which are exactly
/// what the lock is hiding (docs/adr/0052 decision 4). `Success` is not a failure and [`apply`]
/// never asks for its message.
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
    /// `commands_tx` carries each command back to `main.rs` rather than to a socket directly,
    /// because `main.rs` is the only holder of the authoritative generation id and a swap
    /// reassigns it -- a controller that cached one would keep commanding a reaped generation.
    pub fn new(commands_tx: UnboundedSender<shared::SetSessionLock>) -> Self {
        Self { state: Mutex::new(LockState::default()), commands_tx }
    }

    /// `lock.lock()`.
    ///
    /// A `lock()` against a lock that is already on the glass is dropped here rather than recorded
    /// and sent, because `requested` must never be set by a request nothing will resolve. The
    /// Renderer's `lock_command` answers `LockCommand::Nothing` for `(locked: true, lock_held:
    /// true)` and `Nothing` touches no protocol object and emits no `LockReport` -- so recording
    /// the request would shut the swap gate ([`defers_swap`]) on an event that is never coming.
    /// That is safe to rely on today only because `active` shuts the same gate and every eventual
    /// report clears both flags, which is a coincidence of the two flags overlapping, not a design.
    ///
    /// ponytail: this reads `active` to predict what the Renderer already holds, and the two
    /// disagree for exactly as long as a `Finished` report is in flight -- the compositor ended
    /// the lock, the Renderer holds nothing, and a `lock()` landing in that window is dropped here
    /// instead of taking a new lock. The window is one socket hop and the failure is a lock request
    /// lost, not a lock wrongly released, which is why this is the direction to be wrong in. The
    /// upgrade path is on the Renderer's side: report `Nothing` as a `LockOutcome` too, and this
    /// whole guard and its prediction go away, because then every request has a resolving event.
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

    /// Orders the unlock. Not reachable from Lua and deliberately absent from [`dispatch`]: its
    /// only caller is `main.rs`'s `secure_submit(lock, authenticate)` arm, on a `PamOutcome::
    /// Success`. That is what makes docs/adr/0042's "never call `unlock_and_destroy` except on a
    /// successful authentication" checkable by reading one arm instead of trusting every path
    /// that could reach this method.
    ///
    /// Records no event: `active` clears when the Renderer reports `Unlocked`, not when the
    /// order goes out, per [`apply`]'s invariant.
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
    /// method rather than a `may_authenticate` getter followed by a `record`, because the check
    /// and the mark have to happen under the same lock: `main.rs` now spawns the conversation
    /// instead of `.await`ing it inline, so two submissions can reach this between the answer
    /// coming back and anything else running.
    ///
    /// The `Some` carries the acquisition the conversation is about, which the caller must carry
    /// back to [`Self::record_authentication`]. Handed out here rather than read again later
    /// because "later" is the whole problem: a worker outlives the lock it was started for, and
    /// the only moment the right number is knowable is the moment the question is asked.
    pub fn try_begin_authentication(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        if !may_authenticate(&state) {
            return None;
        }
        apply(&mut state, LockEvent::AuthenticationStarted);
        Some(state.acquisition)
    }

    /// Applies a PAM answer to the lock it was started for, or refuses it. Returns whether it was
    /// applied, so the one caller (`main.rs`'s `pam_outcomes` arm) can log the refusal and, more
    /// importantly, know not to order an unlock: a `Success` this method refused is a password
    /// typed against a lock that no longer exists (see [`accepts_outcome`]).
    ///
    /// The check and the record are one method under one lock for the same reason
    /// [`Self::try_begin_authentication`] is: the acquisition read must be the acquisition
    /// recorded against.
    pub fn record_authentication(&self, acquisition: u64, outcome: shared::PamOutcome) -> bool {
        let mut state = self.state.lock().unwrap();
        if !accepts_outcome(&state, acquisition) {
            return false;
        }
        apply(&mut state, LockEvent::Authenticated(outcome));
        true
    }

    /// A closed channel means `main.rs`'s loop is gone, i.e. the Supervisor is shutting down --
    /// logged and dropped, the same posture `send_frame_logged` takes for a `NoConnection`.
    fn send(&self, command: shared::SetSessionLock) {
        if self.commands_tx.send(command).is_err() {
            eprintln!("lock: the command channel is closed; dropping {command:?}");
        }
    }
}

/// `oblisk.lock`'s action dispatch (docs/adr/0037). Neither action takes arguments, so unlike
/// `keyboard` there is no `parse_*_args` sibling and no malformed-arguments path.
///
/// Locking is the one direction a config may command. There is no `unlock` action, and the
/// asymmetry is the point: a lock screen's node tree is Lua too, it is the only thing on the
/// glass, and its `button` callbacks run, so an `unlock` action would be a one-click path past
/// PAM -- exactly the convenience path docs/adr/0042 forbids. [`LockController::unlock`] instead
/// has exactly one call site by construction, `main.rs`'s `secure_submit(lock, authenticate)`
/// arm on a `PamOutcome::Success`, which is what makes that rule checkable by reading one arm.
/// `"unlock"` therefore falls through to [`crate::log_unknown_action`] like any other name this
/// capability does not answer to.
pub fn dispatch(controller: &LockController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "lock" => controller.lock(),
        _ => crate::log_unknown_action(params),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_compositor_confirming_a_lock_sets_the_marker_and_only_a_release_clears_it() {
        // The marker answers a different question from `LockState.active`. `active` means "this
        // shell holds the lock" and a refusal never took one, so a `Refused` must leave whatever
        // the file already said alone rather than clearing a lock some earlier generation really
        // did take.
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Locked), SessionLock::Taken);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Unlocked), SessionLock::Released);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Finished), SessionLock::Released);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Refused("no lock node".into())), SessionLock::Unchanged);
    }

    #[test]
    fn a_renderer_that_died_holding_the_lock_leaves_the_marker_set() {
        // The whole point of the file. `LockEvent::RendererLost` clears `LockState.active` because
        // this shell no longer holds anything, but the compositor is still locked and is required
        // not to unlock on client death -- so a marker driven off `active` would erase the one fact
        // a restarted Supervisor needs. Driving it off `LockOutcome` instead means `RendererLost`
        // cannot reach it at all, which is the property this asserts: it is not a `Reported`.
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
        // A different `SessionLockedFlag` value entirely, which is what a restarted Supervisor is.
        assert!(SessionLockedFlag::at(path.clone()).is_set());

        SessionLockedFlag::at(path.clone()).apply(SessionLock::Released);
        assert!(!SessionLockedFlag::at(path).is_set());
    }

    #[test]
    fn applying_the_same_change_twice_is_not_an_error() {
        // Both directions repeat in ordinary use: two `Locked` reports across two acquisitions, and
        // a `Finished` arriving after an `Unlocked` already cleared it. Removing a file that is not
        // there must not be treated as a failure to clear.
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
    /// `attempts`/`error` is visible rather than indistinguishable from the default. `acquisition`
    /// is likewise deliberately not 0: a transition that renumbered the lock it was handed would
    /// otherwise be indistinguishable from one that left it alone.
    fn locked_with_one_failure() -> LockState {
        LockState { active: true, authenticating: true, attempts: 1, error: "authentication failed".to_string(), requested: false, acquisition: 4 }
    }

    #[test]
    fn losing_the_renderer_clears_active_so_a_replacement_may_request_the_lock_again() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        assert!(!state.active, "the lock object died with the process that held it");
        assert!(!state.requested, "a request nothing will answer must not keep the swap gate shut forever");
        // `LockController::lock` refuses while `active`, on the correct assumption that a live
        // Renderer already holds one. Clearing it is the whole point: without this the replacement's
        // re-acquisition is dropped before it reaches the wire (docs/adr/0058 decision 4).
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

        // `accepts_outcome`'s doc states the invariant this keeps: every transition that clears
        // `active` clears `authenticating` in the same arm, so a dropped answer is always one whose
        // own flag some other transition already released. A stranded `authenticating` would refuse
        // every future attempt through `may_authenticate`.
        assert!(!state.authenticating, "the conversation's answer can no longer be applied, so its slot must be free");
    }

    #[test]
    fn losing_the_renderer_does_not_renumber_the_acquisition() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        // Only a confirmed `Locked` moves `acquisition`, and a replacement that re-acquires goes
        // through exactly that. Bumping here too would number a lock that was never taken.
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
        // docs/adr/0042: the swap gate has to shut when the order goes out, not when the
        // compositor gets round to confirming it. `ext_session_lock_v1` lets the compositor
        // withhold `locked` until the client has presented a lock surface on every output, so
        // that window is neither short nor rare -- and a swap started inside it reaps the very
        // process that owns the lock object, leaving the session locked with no holder.
        let mut state = LockState::default();
        assert!(!defers_swap(&state), "an untouched session defers nothing");

        apply(&mut state, LockEvent::LockRequested);
        assert!(defers_swap(&state), "the order is out; the holder-to-be must not be reaped now");
        assert!(!state.active, "and `active` still moves only on the Renderer's own report");
    }

    #[test]
    fn every_way_a_lock_request_resolves_reopens_the_swap_gate() {
        // A resolution that forgot to clear `requested` would defer reloads for the rest of the
        // session: only a `Finished`/`Unlocked` report redeems `swap_owed_on_unlock`, so a
        // refused lock that left the gate shut would make the config permanently unreloadable.
        for outcome in [shared::LockOutcome::Refused("no lock node is declared".to_string()), shared::LockOutcome::Finished, shared::LockOutcome::Unlocked] {
            let mut state = LockState::default();
            apply(&mut state, LockEvent::LockRequested);
            apply(&mut state, LockEvent::Reported(outcome.clone()));
            assert!(!defers_swap(&state), "{outcome:?} resolved the request, so a swap may run again");
        }

        // `Locked` resolves the request too. The gate stays shut, but on `active` now.
        let mut state = LockState::default();
        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(defers_swap(&state) && state.active && !state.requested, "a confirmed lock hands the gate over to `active`");

        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Unlocked));
        assert!(!defers_swap(&state));
    }

    #[test]
    fn authentication_needs_a_held_lock_and_no_attempt_already_in_flight() {
        assert!(!may_authenticate(&LockState::default()), "an unlocked session must not be a PAM oracle any config can drive from a bar textfield");
        assert!(!may_authenticate(&LockState { requested: true, ..LockState::default() }), "an unconfirmed request is not a lock screen on the glass yet");
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
        assert_eq!(state, LockState { active: true, authenticating: false, attempts: 0, error: String::new(), requested: false, acquisition: 1 });
    }

    #[test]
    fn refused_records_the_reason_and_leaves_the_session_unlocked() {
        let mut state = LockState::default();
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Refused("no lock node is declared".to_string())));
        assert_eq!(state, LockState { active: false, authenticating: false, attempts: 0, error: "no lock node is declared".to_string(), requested: false, acquisition: 0 });
    }

    #[test]
    fn authentication_started_sets_authenticating() {
        let mut state = LockState { active: true, ..LockState::default() };
        apply(&mut state, LockEvent::AuthenticationStarted);
        assert_eq!(state, LockState { active: true, authenticating: true, attempts: 0, error: String::new(), requested: false, acquisition: 0 });
    }

    #[test]
    fn a_failed_authentication_counts_an_attempt_and_keeps_the_session_locked() {
        let mut state = LockState { active: true, authenticating: true, ..LockState::default() };
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(state, LockState { active: true, authenticating: false, attempts: 1, error: "authentication failed".to_string(), requested: false, acquisition: 0 });

        // docs/adr/0052 decision 4: capability state is sampled at layout time, so two identical
        // consecutive failures are one unchanged `error` string -- the counter is the only thing
        // that tells the config the second one happened.
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(state.attempts, 2);
    }

    #[test]
    fn a_successful_authentication_stops_authenticating_but_does_not_itself_unlock() {
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        assert_eq!(state, LockState { active: true, authenticating: false, attempts: 1, error: String::new(), requested: false, acquisition: 4 }, "active clears only when the Renderer reports Unlocked -- the lock is on the glass until unlock_and_destroy actually runs");
    }

    #[test]
    fn unlocked_and_finished_both_clear_the_session() {
        for outcome in [shared::LockOutcome::Unlocked, shared::LockOutcome::Finished] {
            let mut state = locked_with_one_failure();
            apply(&mut state, LockEvent::Reported(outcome.clone()));
            assert_eq!(state, LockState { active: false, authenticating: false, attempts: 1, error: String::new(), requested: false, acquisition: 4 }, "{outcome:?}");
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

        // `main.rs`'s PAM-outcome arm is this method's only caller.
        controller.unlock();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: false }));
        assert!(controller.snapshot().active, "active clears on the Renderer's Unlocked report, not on the order going out");
    }

    #[test]
    fn try_begin_authentication_admits_exactly_one_attempt_at_a_time() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        assert!(controller.try_begin_authentication().is_none(), "no lock is held, so there is nothing to authenticate against");
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let acquisition = controller.try_begin_authentication().expect("a held lock admits the first attempt");
        assert!(controller.snapshot().authenticating);
        assert!(controller.try_begin_authentication().is_none(), "the second submission must find the first still in flight");

        assert!(controller.record_authentication(acquisition, shared::PamOutcome::AuthFailed));
        assert!(controller.try_begin_authentication().is_some(), "the worker's answer released it");
    }

    #[test]
    fn a_stale_pam_outcome_cannot_release_the_lock_that_replaced_the_one_it_authenticated_against() {
        // The bypass, in the order it happens. A worker started against lock N is still running
        // -- `pam_unix` takes about a second and PAM_EXCHANGE_TIMEOUT allows thirty -- when the
        // compositor ends lock N through its own mechanism (`finished` after `locked`, which is
        // what `loginctl unlock-session` produces) and an idle timer takes lock N+1. Nothing about
        // `active`/`authenticating` distinguishes the two locks by then; only the number does.
        let mut state = LockState::default();
        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        apply(&mut state, LockEvent::AuthenticationStarted);
        let acquisition = state.acquisition;
        assert!(accepts_outcome(&state, acquisition), "the ordinary case: the answer is about the lock still on the glass");

        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Finished));
        assert!(!accepts_outcome(&state, acquisition), "the lock the password was typed against is gone; the answer is about nothing");

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
        // The same binding, on the other outcome. `Reported(Locked)` resets `attempts` and clears
        // `error` for the new acquisition, so a stale failure landing afterwards would show the
        // user a failure they never made -- and, with `attempts` driving a lockout in a config,
        // one they cannot clear.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let stale = controller.try_begin_authentication().expect("a held lock admits the attempt");
        controller.record(LockEvent::Reported(shared::LockOutcome::Finished));
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));

        assert!(!controller.record_authentication(stale, shared::PamOutcome::AuthFailed), "the refusal is reported so main.rs can log it");

        let state = controller.snapshot();
        assert_eq!(state.attempts, 0, "the new lock has seen no attempts");
        assert_eq!(state.error, "", "and nothing to say about one");
    }

    #[test]
    fn a_second_lock_while_one_is_held_is_dropped_rather_than_left_unresolved() {
        // docs/adr/0052 decision 4: the Renderer's `lock_command` answers `Nothing` to a
        // `locked: true` it is already holding, and `Nothing` emits no `LockReport` -- so a
        // `requested` set here would be a flag with no event coming to clear it, and
        // `defers_swap` would refuse every reload for the rest of the session.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        controller.lock();
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        controller.lock();

        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(rx.try_recv().is_err(), "the second lock() cannot change anything, so it is not sent either");
        assert!(!controller.snapshot().requested, "and above all it does not shut the swap gate on an event that is never coming");
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
        // docs/adr/0042: a lock screen's own `button` callbacks are Lua, they run while the lock
        // surfaces are the only thing on the glass, and an `unlock` action would make walking
        // past PAM one mouse click. The asymmetry with `lock` is deliberate, so it is pinned
        // rather than left to be re-added as an oversight.
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
