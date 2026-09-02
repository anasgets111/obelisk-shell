//! The session lock: `ext_session_lock_v1` (ADR-0042, ADR-0052). Holds the acquire/refuse/release
//! decision tables and the `oblisk.rescue` messages shown when no lock screen is left to read one
//! on. Lock surface creation/teardown and the `SessionLockHandler` callbacks live here; the
//! generic bind/paint/(un)map machinery shared with every role stays in `surface`.

use super::*;
use crate::layout::secure_submit::tree_can_authenticate;
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

/// ADR-0052 decision 3's refusal, as the sentence the user reads. A config with no `lock` node
/// cannot be locked: acquiring anyway paints nothing, the compositor never unlocks on client death
/// (ADR-0042), and the only way out is a VT switch. That is a denial of service, not fail-secure.
const NO_LOCK_DECLARED: &str = "this config declares no `lock` surface (§ 6.4), so locking the session would leave a black screen with no password field and no way back \
     in short of a VT switch; the lock was refused (ADR-0052 decision 3)";
/// The other half of decision 3's refusal. `node::lock_spec` requires only an `id`; `child` is
/// optional, so `lock { id = "x" }` legally resolves to the same black screen, so counting tracked
/// `lock` instances isn't enough: what matters is whether the tree holds a
/// `layout::secure_submit`'s `UNLOCK_TARGET` field, the only source of the `SecureSubmit` an unlock
/// answers. Distinct from [`NO_LOCK_DECLARED`]: that's a missing `lock` node, this a missing
/// `textfield` inside the one it has.
const LOCK_CANNOT_AUTHENTICATE: &str = "this config's `lock` surface (§ 6.4) does not hold exactly one `textfield` with `secure_submit = { capability = \"lock\", action = \"authenticate\" }` \
     and nothing else, so the compositor handing it keyboard focus would arm no field, nothing on it could ever authenticate, and the only way back in \
     would be a VT switch; the lock was refused (ADR-0052 decision 3)";
/// A `SetSessionLock { locked: false }` that reached a lock object the compositor never answered
/// with `locked`. See [`App::release_session_lock`]: nothing was released, there was nothing to.
const LOCK_NEVER_GRANTED: &str = "the session lock was given up before the compositor ever granted it (no `ext_session_lock_v1::locked` arrived), so nothing was unlocked";
/// The other half of `finished`: the compositor refused `lock` immediately instead of sending
/// `locked`. Almost always another lock client already holds the session, but the wire can't narrow
/// it down further, so the message says only what is known.
const LOCK_DENIED: &str = "the compositor denied the session lock; another lock client most likely holds it already (`ext_session_lock_v1::finished` arrived in place \
     of `locked`)";
/// What `oblisk.rescue` says when the compositor tore down a lock that really was up. Not a failure
/// of anything this process did: ADR-0052 decision 4 routes it here, not to `oblisk.lock`'s
/// `error`, since there is no lock screen left on the glass to read a message on.
const LOCK_TORN_DOWN: &str = "the compositor ended the session lock through its own mechanism; the session is unlocked and the lock screen is gone \
     (`ext_session_lock_v1::finished` after `locked`)";
/// Exit code for a gone Supervisor control socket (ADR-0059 decision 1). Nobody reads it, since the
/// process that classifies exit codes just died: it's for a journal or `$status`, not a handshake.
/// Distinct from `0` (not a clean exit) and `1` (not a failure of a request).
pub(super) const EXIT_SUPERVISOR_GONE: i32 = 70;
/// What this process says on its way out when the Supervisor's control socket is gone, split on
/// whether it holds `ext_session_lock_v1` at that moment (ADR-0059 decisions 1 and 2). Split out
/// because the locked half can mislead: ADR-0058 decision 4 caught the neighbouring mistake, where
/// a refusal ending "the lock screen that is on screen still stands" is true of a vetoed reload but
/// false of a process that is exiting.
pub(super) fn supervisor_gone_report(holds_session_lock: bool) -> &'static str {
    if holds_session_lock {
        "the Supervisor's control socket is gone while this Renderer holds the session lock. PAM runs in the Supervisor (ADR-0028), so this \
         lock screen can no longer authenticate anyone, and exiting without unlocking is what keeps a `kill` from being a way past a lock screen. \
         The session stays locked behind whatever the compositor puts up for a lock client that died, and the way back in is a VT switch \
         (ADR-0059 decision 2)"
    } else {
        "the Supervisor's control socket is gone, so this Renderer has no capability data, no `process.run` and no PAM left to serve. Exiting \
         rather than painting a shell that still takes clicks and answers none of them (ADR-0059 decision 1)"
    }
}
/// What one `SetSessionLock` asks this process to do, decided before any Wayland object is touched
/// (ADR-0042, ADR-0052 decisions 3 and 4). Pure and separate because the two interesting answers
/// are refusals, and a refusal living inside a `&mut self` method that also talks to the compositor
/// would be untestable. See [`lock_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockCommand {
    /// Call `SessionLockState::lock` and create the surfaces.
    Acquire,
    /// Call `SessionLock::unlock`, then tear the surfaces down. **The only value that unlocks.**
    Release,
    /// Touch no protocol object at all; report this as `LockOutcome::Refused` and set `rescue`.
    Refuse(&'static str),
    /// The command asks for the state the lock is already in.
    Nothing,
}
/// One `SetSessionLock`, resolved against what this process already holds. `locked = true` has four
/// answers and only one takes the lock: ADR-0052 decision 3 must refuse before
/// `SessionLockState::lock` runs, since a lock granted and found unusable is the black screen it
/// exists to prevent. `declares_lock` checks the tracked surface set; `can_authenticate` checks the
/// resolved tree, since `lock { id = "x" }` resolves to nothing typable (see
/// [`LOCK_CANNOT_AUTHENTICATE`]) and `declares_lock` alone would waive decision 3 for a declared,
/// empty node. The compositor-cannot-lock case isn't an input here: `lock` answers
/// `GlobalError::MissingGlobal` when `ext_session_lock_manager_v1` was never advertised, and the
/// caller turns that `Err` into a `Refuse` with its own words. `locked = false` against nothing
/// held is `Nothing`, not `Release`: `unlock_and_destroy` on a lock never `locked` is the
/// protocol's `invalid_unlock`.
fn lock_command(locked: bool, declares_lock: bool, can_authenticate: bool, lock_held: bool) -> LockCommand {
    match (locked, lock_held) {
        (false, true) => LockCommand::Release,
        (false, false) | (true, true) => LockCommand::Nothing,
        (true, false) if !declares_lock => LockCommand::Refuse(NO_LOCK_DECLARED),
        (true, false) if !can_authenticate => LockCommand::Refuse(LOCK_CANNOT_AUTHENTICATE),
        (true, false) => LockCommand::Acquire,
    }
}
/// What one ordered release actually did, given whether `ext_session_lock_v1::locked` had been
/// dispatched on the lock object being given up (ADR-0052 decision 4). The Supervisor's
/// `lock::apply` moves its `active` flag on `Unlocked`, so reporting one for a lock never granted
/// would claim a locked-to-unlocked transition that never happened. SCTK's `SessionLock::unlock` is
/// a no-op below `is_locked()`, so nothing was sent, and [`LOCK_NEVER_GRANTED`] says so instead.
fn release_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Unlocked } else { LockOutcome::Refused(LOCK_NEVER_GRANTED.to_string()) }
}
/// Which of `ext_session_lock_v1::finished`'s two events this one is (ADR-0042), decided by whether
/// `locked` was ever sent on this lock object. The protocol puts both on one event: `finished`
/// fires immediately if the compositor decides `locked` won't be sent, typically because another
/// client already holds the lock, or later, after `locked` fired, when a live lock gets ended
/// through the compositor's own secure mechanism. They must not collapse into one report:
/// `lock::apply` routes them differently, a denial being a failure the user has to see, a teardown
/// a state change already lived through. `was_locked` is SCTK's own flag: its `Dispatch2` for
/// `ext_session_lock_v1` sets it on `Locked` and never clears it, not even on `Finished`, so no
/// bookkeeping of ours can drift from it.
fn finished_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Finished } else { LockOutcome::Refused(LOCK_DENIED.to_string()) }
}

impl App {
    /// [`App::create_surfaces`]'s `lock` arm: pushes the tracking entry always, never the
    /// `ext_session_lock_surface_v1` (ADR-0052 decision 2). The entry lets the retained scene
    /// resolve this instance's tree, so an in-place reload can restyle a live lock screen, and it's
    /// the only record that this config declares a lock screen, the fact decision 3 refuses a lock
    /// on the absence of ([`App::set_session_lock`] looks for these entries). No `visible` is
    /// consulted: `layout::node::lock_spec` refuses the property outright, since the compositor
    /// owns this surface's lifetime end to end. The `wl_output` is kept rather than the output's
    /// name because `get_lock_surface` takes the proxy, and this is the one place it's in hand.
    pub(super) fn create_lock(&mut self, instance: &SurfaceInstance, outputs: &HashMap<String, wl_output::WlOutput>) {
        let Some(output) = outputs.get(&instance.output) else {
            eprintln!(
                "[oblisk-renderer] instance {:?} names an output that has since gone; skipping",
                instance.instance_id
            );
            return;
        };
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Lock { output: output.clone(), surface: None },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
            last_painted: None,
        });
    }

    /// One `ext_session_lock_surface_v1` for every declared `lock` instance without one yet, or
    /// nothing if this process holds no lock. Idempotent per output, a protocol requirement: a
    /// second lock surface on one output is a `duplicate_output` error, killing the connection with
    /// the session still locked. `expand_instances` makes one `lock` instance per output per
    /// declared spec (ADR-0052 decision 2), and `crate::socket`'s `surface_specs` refuses configs
    /// with more than one `lock`, so "instance already has a surface" and "output already has one"
    /// always agree; the `surface: None` pattern below is that invariant's per-instance half. Three
    /// callers keep the set matching outputs whenever either moves: right after `lock` succeeds
    /// (the protocol wants surfaces immediately, else a waiting compositor holds a blank frame for
    /// its own time limit), again on `locked` for an output advertised inside that window, and from
    /// [`App::create_surfaces`], the hotplug path. No commit here: every other create path ends in
    /// its required initial commit, but that's a protocol error before the first configure is
    /// acked, which the compositor sends immediately on `get_lock_surface`.
    pub(super) fn ensure_lock_surfaces(&mut self, qh: &QueueHandle<App>) {
        // Cloned out of `self` so the loop's `&mut self` calls are free to run; `SessionLock` is
        // an `Arc` handle, so this is a refcount bump, not a second lock.
        let Some(lock) = self.session_lock.clone() else {
            return;
        };
        for index in 0..self.surfaces.len() {
            let TrackedRole::Lock { output, surface: None } = &self.surfaces[index].role else {
                continue;
            };
            let output = output.clone();
            let wl_surface = self.compositor_state.create_surface(qh);
            let lock_surface = lock.create_lock_surface(wl_surface, &output, qh);
            if let TrackedRole::Lock { surface, .. } = &mut self.surfaces[index].role {
                *surface = Some(lock_surface);
            }
            self.surfaces[index].map_state = MapState::AwaitingConfigure;
            eprintln!(
                "[oblisk-renderer] {}: lock surface created, awaiting its configure",
                self.surfaces[index].surface_id
            );
        }
    }

    /// Destroys every live `ext_session_lock_surface_v1` and frees the EGL side behind it, leaving
    /// the tracking entries in place: a `lock` instance stays retained while the config declares
    /// it, and the next lock rebuilds its surfaces through [`App::ensure_lock_surfaces`]. The order
    /// is [`App::destroy_surface_by_id`]'s minus its last step: `eglDestroySurface` and
    /// `wl_egl_window_destroy` first ([`App::release_bound`]), then the `SessionLockSurface`
    /// handle, whose `Drop` sends `destroy` and then destroys the `wl_surface`, keeping a
    /// `wl_egl_window` from ever pointing at an already-destroyed one. Timing relative to the
    /// unlock is load-bearing: the ordered-unlock path runs this after `unlock_and_destroy`, since
    /// destroying a lock surface whose output is still active while locked makes the compositor
    /// fall back to a solid color, a visible flash before the desktop returns. `finished` has no
    /// such window: the compositor already ended the lock.
    fn teardown_lock_surfaces(&mut self) {
        for index in 0..self.surfaces.len() {
            if !matches!(self.surfaces[index].role, TrackedRole::Lock { surface: Some(_), .. }) {
                continue;
            }
            self.release_bound(index);
            if let TrackedRole::Lock { surface, .. } = &mut self.surfaces[index].role {
                *surface = None;
            }
            self.surfaces[index].map_state = MapState::Unmapped;
            eprintln!("[oblisk-renderer] {}: lock surface destroyed", self.surfaces[index].surface_id);
        }
    }

    /// One `SetSessionLock` from the Supervisor (ADR-0042, ADR-0052 decision 1). The decision is
    /// [`lock_command`], pure and tested; this is the protocol traffic it does not do.
    /// `declares_lock` is counted off the tracked surface set, not the roster or the scene, the one
    /// place the question has a single answer: `create_lock` pushes an entry per `lock` instance
    /// and `destroy_surface_by_id` removes it with its output. `can_authenticate` goes to the scene
    /// instead: a tracked instance says a `lock` node was written, but only its resolved tree says
    /// whether anything under it could produce the `SecureSubmit` that unlocks (see
    /// [`LOCK_CANNOT_AUTHENTICATE`]), via [`tree_can_authenticate`], the same predicate keyboard
    /// focus arms on. `any`, not `all`, across instances: a single typable one means the config is
    /// correct. A `lock` that fails at the protocol level is a refusal, not a crash, the tolerance
    /// [`App::show_window`] applies to a missing `xdg_wm_base`: the shell keeps painting, and the
    /// channel ADR-0052 decision 4 named says what did not happen.
    pub(super) fn set_session_lock(&mut self, qh: &QueueHandle<App>, locked: bool) {
        let lock_instances: Vec<String> = self
            .surfaces
            .iter()
            .filter(|tracked| matches!(tracked.role, TrackedRole::Lock { .. }))
            .map(|tracked| tracked.surface_id.clone())
            .collect();
        let can_authenticate = lock_instances
            .iter()
            .filter_map(|id| self.client.scene().surface(id))
            .any(|tree| tree_can_authenticate(&tree));
        match lock_command(locked, !lock_instances.is_empty(), can_authenticate, self.session_lock.is_some()) {
            LockCommand::Nothing => {}
            LockCommand::Refuse(reason) => self.refuse_lock(reason),
            LockCommand::Acquire => match self.session_lock_state.lock(qh) {
                Ok(lock) => {
                    self.session_lock = Some(lock);
                    // Armed here, not on `locked`: a reload landing in between would otherwise
                    // strip the password field from the tree the compositor is about to show
                    // (`crate::socket`'s `lock_stays_authenticatable`). Only the fact of the lock
                    // is handed over, not the ids: a monitor hotplug retires and replaces them.
                    self.client.set_session_locked(true);
                    self.ensure_lock_surfaces(qh);
                    eprintln!(
                        "[oblisk-renderer] session lock requested; waiting for the compositor's `locked` or `finished`"
                    );
                }
                // The compositor advertises no `ext_session_lock_manager_v1`. Carried as the
                // error's own words, not a constant beside the other two, since `GlobalError`
                // already says which global is missing and the cause is outside shell and config.
                Err(err) => {
                    let reason = format!("this compositor cannot lock the session: {err} (ADR-0042)");
                    self.refuse_lock(&reason);
                }
            },
            LockCommand::Release => self.release_session_lock(),
        }
    }

    /// `unlock_and_destroy`, and the only path in this process that performs one (ADR-0042,
    /// ADR-0052's consequences). Reachable from exactly one place: a `SetSessionLock { locked:
    /// false }`, sent by the Supervisor only from the `pam_outcomes` arm of its `select!` loop, on
    /// a `PamOutcome::Success`, making the rule structural, not trust-based. No convenience path
    /// here either, not on shutdown, `finished`, or a config reload: SCTK's `Drop` deliberately
    /// does not unlock, since a Renderer that dies locked must leave the session locked, and a
    /// self-initiated unlock would turn a crash into an unlocked desktop. `SessionLock::unlock` is
    /// a no-op unless `is_locked()`; the in-flight case, a `lock` request whose `locked` hasn't
    /// arrived, sends nothing, and the `Drop` below sends the plain `destroy` instead.
    /// `is_locked()` flips on dispatch, not on the compositor's send, so [`run`] round-trips before
    /// calling this, since an unread `locked` would otherwise make this send a `destroy` the
    /// compositor answers with `invalid_destroy`. The report matches what happened, not what was
    /// asked for ([`release_outcome`]): a no-op `unlock` released nothing, and the Supervisor's
    /// `active` flag moves on these reports.
    fn release_session_lock(&mut self) {
        let Some(lock) = self.session_lock.take() else {
            return;
        };
        // Read before the unlock: `unlock_and_destroy` is a destructor, and the flag it's gated on
        // is the one being reported here.
        let was_locked = lock.is_locked();
        lock.unlock();
        // Sends `ext_session_lock_v1.destroy` if `unlock` didn't already destroy the object: SCTK's
        // own `SessionLock` doc says a locked object must be `unlock`ed before it is dropped.
        drop(lock);
        self.teardown_lock_surfaces();
        // Nothing is locked now, so an in-place reload is free to reshape the lock screen again.
        self.client.set_session_locked(false);
        let outcome = release_outcome(was_locked);
        match &outcome {
            LockOutcome::Unlocked => eprintln!("[oblisk-renderer] the session lock was released"),
            _ => eprintln!("[oblisk-renderer] {LOCK_NEVER_GRANTED}"),
        }
        self.report_lock(outcome);
    }

    /// A lock that was asked for and did not happen: logged, pushed to `oblisk.rescue`, and
    /// reported (ADR-0052 decision 4). `rescue` is the right channel: a refused lock leaves the
    /// ordinary scene on the glass with no lock screen to render a message on, and `rescue` renders
    /// through the config's own surfaces. The wrong-password case is the opposite and never comes
    /// here: it happens with the lock surfaces mapped and everything else hidden, reaching the
    /// config as `oblisk.lock` instead. Nothing clears this here on purpose: a later successful
    /// evaluation clears `rescue` on its own success path (`RendererClient::handle_reevaluate`),
    /// and editing the config to declare a lock screen is itself such a re-evaluation.
    fn refuse_lock(&mut self, reason: &str) {
        eprintln!("[oblisk-renderer] the session lock was refused: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(LockOutcome::Refused(reason.to_string()));
    }

    /// Queues one `LockReport` on the outbound channel, like `ReadySignal` and
    /// `PresentationEvidence`. Every lock state change goes through here, so `lock::apply` sees
    /// each transition exactly once, and its `active` flag moves on these reports alone.
    fn report_lock(&mut self, outcome: LockOutcome) {
        if let Err(e) = self.outbound_tx.send(RendererFrame::LockReport(LockReport { outcome })) {
            eprintln!("[oblisk-renderer] failed to queue a LockReport for the socket thread: {e}");
        }
    }
}

/// `ext_session_lock_v1` for the session lock (ADR-0042, ADR-0052). See `delegate_dispatch2!(App)`
/// at the bottom of this file for why no `delegate_session_lock!` call accompanies this.
/// `SessionLockState` is also absent from `registry_handlers![OutputState, SeatState]`: it is not a
/// `RegistryHandler`. It binds from the `GlobalList` once in [`run`], and its `GlobalProxy` carries
/// the "not advertised" case for [`App::set_session_lock`] to report.
impl SessionLockHandler for App {
    /// The compositor granted the lock: the session is now locked, every other client's content is
    /// hidden, and this process is responsible for what is on screen until it unlocks (ADR-0042).
    /// The surface creation here is normally a no-op: [`App::set_session_lock`] already made one
    /// per output the moment `lock` succeeded, since the protocol asks for them immediately and
    /// lets the compositor wait before sending this event, avoiding a blank frame. This call only
    /// catches an output advertised inside that window, idempotently
    /// ([`App::ensure_lock_surfaces`]). The lock handle is re-stored rather than compared against
    /// the one `lock` returned: it's the same `Arc` with `is_locked()` already flipped by SCTK's
    /// dispatch, so storing it costs a refcount bump while removing the only way the two could ever
    /// disagree.
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, session_lock: SessionLock) {
        self.session_lock = Some(session_lock);
        self.ensure_lock_surfaces(qh);
        let surfaces = self
            .surfaces
            .iter()
            .filter(|tracked| matches!(tracked.role, TrackedRole::Lock { surface: Some(_), .. }))
            .count();
        eprintln!("[oblisk-renderer] the session is locked; {surfaces} lock surface(s) up");
        self.report_lock(LockOutcome::Locked);
    }

    /// Two events, told apart by [`finished_outcome`] and never swallowed. Before any `locked`, the
    /// compositor denied the request. After one, it ended a live lock through its own secure
    /// mechanism. Both set `rescue` (ADR-0052 decision 4): either way no lock screen is left to
    /// read a message on, so the ordinary scene renders it.
    ///
    /// `is_locked()` picks the teardown verb, because the protocol leaves no choice.
    /// `ext-session-lock-v1`'s `destroy` says "it is a protocol error to make this request if the
    /// locked event was sent, the unlock_and_destroy request must be used instead". Answering a
    /// post-`locked` `finished` with plain `destroy` is `invalid_destroy`, which kills the
    /// connection and leaves the session unlocked with the shell dead.
    ///
    /// ADR-0042 forbids a convenience unlock path. This is not one: the compositor initiated it.
    /// The only path that ends a live lock from our side is [`App::release_session_lock`].
    ///
    /// Both verbs are `type="destructor"` and `wayland-backend` drops a request on a destroyed
    /// object client-side, so `SessionLockInner::Drop`'s unconditional `destroy` is a no-op.
    fn finished(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        let outcome = finished_outcome(session_lock.is_locked());
        // Ahead of dropping the lock object, unlike the ordered-unlock path: no
        // fall-back-to-solid-colour window here, since the compositor already ended the lock.
        self.teardown_lock_surfaces();
        if session_lock.is_locked() {
            // The only verb the protocol accepts once `locked` has been sent (see doc comment).
            session_lock.unlock();
        }
        // For a denial (no `locked`), `SessionLockInner::Drop`'s plain `destroy` sends itself.
        self.session_lock = None;
        // No lock is held now either way. Disarmed for [`App::release_session_lock`]'s reason.
        self.client.set_session_locked(false);
        let reason = match &outcome {
            LockOutcome::Finished => LOCK_TORN_DOWN,
            _ => LOCK_DENIED,
        };
        eprintln!("[oblisk-renderer] the session lock ended: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(outcome);
    }

    /// One `ext_session_lock_surface_v1.configure`, already acked by SCTK's `Dispatch2` before this
    /// runs, so nothing here acks, exactly as `window` and `popup` don't ack theirs. Everything
    /// after the lookup is [`App::bind_and_clear`], shared verbatim with the other three roles. The
    /// size is taken as given, no negotiation: a lock surface covers its output, the compositor
    /// knows that size, and a mismatched buffer is the protocol's own `dimensions_mismatch` error.
    /// This is also the event that maps the surface: `ensure_lock_surfaces` left it in
    /// `MapState::AwaitingConfigure` with no initial commit, since `ext_session_lock_surface_v1`
    /// forbids one before the first ack. `bind_and_clear` flips that to `Mapped`, binds EGL, paints
    /// the resolved tree, and `eglSwapBuffers` carries the first buffer as the commit.
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.index_of_surface(surface.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        self.bind_and_clear(index, width, height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_supervisor_that_vanished_while_the_lock_was_up_reports_a_locked_session_and_not_a_lock_screen() {
        // ADR-0059 decision 2: this process is about to exit, so the message must send the
        // reader to a VT, not to a password field that no longer exists (ADR-0058 decision 4's
        // "still stands" trap).
        let report = supervisor_gone_report(true);
        assert!(report.contains("VT"), "the locked report must name the only way back in: {report}");
        assert!(!report.contains("still stands"), "nothing this process painted is still on screen: {report}");

        // Must not read as an unlock: exiting while holding the lock is what keeps the session
        // secure (SCTK's own `SessionLockInner::Drop` calls this "failing secure").
        assert!(!report.contains("unlocked"), "the exit does not unlock: {report}");
    }

    #[test]
    fn a_supervisor_that_vanished_with_no_lock_up_says_nothing_about_locks() {
        let report = supervisor_gone_report(false);
        assert!(!report.contains("VT"), "no lock was up, so a VT switch is not the story: {report}");
        assert_ne!(report, supervisor_gone_report(true));
    }

    #[test]
    fn a_lock_is_refused_when_the_config_declares_no_lock_surface() {
        // ADR-0052 decision 3: the refusal must happen before `SessionLockState::lock` is
        // called, since a lock granted and then painted nothing is a black screen with no way out
        // but a VT switch.
        assert_eq!(lock_command(true, false, false, false), LockCommand::Refuse(NO_LOCK_DECLARED));
        assert_eq!(lock_command(true, true, true, false), LockCommand::Acquire);
    }

    #[test]
    fn a_lock_screen_with_no_password_field_is_refused_as_loudly_as_no_lock_screen_at_all() {
        // `lock { id = "x" }` is a legal declaration that resolves to an empty tree, reaching
        // ADR-0052 decision 3's black screen through the guard instead of around it, so the
        // tracked-surface test alone is not enough.
        assert_eq!(lock_command(true, true, false, false), LockCommand::Refuse(LOCK_CANNOT_AUTHENTICATE));
        // The two refusals stay distinct: different edits to make to a config.
        assert_ne!(NO_LOCK_DECLARED, LOCK_CANNOT_AUTHENTICATE);
    }

    #[test]
    fn a_repeated_lock_or_unlock_command_touches_no_protocol_object() {
        // `locked = true` while already holding one would be a second `ext_session_lock_v1`,
        // denied by the compositor. `locked = false` while holding none would be
        // `unlock_and_destroy` on nothing, the protocol's `invalid_unlock` error.
        assert_eq!(lock_command(true, true, true, true), LockCommand::Nothing);
        assert_eq!(lock_command(false, true, true, false), LockCommand::Nothing);
        assert_eq!(lock_command(false, false, false, false), LockCommand::Nothing);
    }

    #[test]
    fn only_a_locked_false_command_against_a_held_lock_releases() {
        // The single `Release` in the table: `unlock_and_destroy` has exactly one reachable caller
        // in this process, reached only on a `PamOutcome::Success` (ADR-0042).
        assert_eq!(lock_command(false, true, true, true), LockCommand::Release);
        assert_eq!(lock_command(false, false, false, true), LockCommand::Release);
    }

    #[test]
    fn releasing_a_lock_the_compositor_never_granted_does_not_report_it_as_unlocked() {
        // The Supervisor's `lock::apply` moves its `active` flag on these reports alone, so an
        // `Unlocked` for a lock that was never `locked` would tell it a transition happened that
        // did not: SCTK's `SessionLock::unlock` is a no-op below `is_locked()`.
        assert_eq!(release_outcome(true), LockOutcome::Unlocked);
        assert_eq!(release_outcome(false), LockOutcome::Refused(LOCK_NEVER_GRANTED.to_string()));
    }

    #[test]
    fn finished_before_a_locked_is_a_denial_and_finished_after_one_is_a_teardown() {
        // One event, two meanings, neither may be swallowed: a denial is a failure the user has to
        // see, a teardown is a state change already lived through, and the Supervisor routes them
        // differently.
        assert_eq!(finished_outcome(false), LockOutcome::Refused(LOCK_DENIED.to_string()));
        assert_eq!(finished_outcome(true), LockOutcome::Finished);
    }
}
