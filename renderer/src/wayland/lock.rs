//! The session lock: `ext_session_lock_v1` (ADR-0042, ADR-0052), including acquire/refuse/release
//! decisions, rescue messages, surfaces, and callbacks. Shared bind/paint/(un)map logic is in
//! `surface`.

use super::*;
use crate::layout::secure_submit::tree_can_authenticate;
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

/// ADR-0052 decision 3's refusal for a config with no `lock` node: acquiring would paint nothing,
/// and the compositor does not unlock on client death (ADR-0042), leaving only a VT switch. That
/// is a denial of service, not fail-secure.
const NO_LOCK_DECLARED: &str = "this config declares no `lock` surface, so locking the session would leave a black screen with no password field and no way back \
     in short of a VT switch; the lock was refused (ADR-0052 decision 3)";
/// ADR-0052 decision 3's other refusal. `lock { id = "x" }` is legal because `child` is optional,
/// so a tracked node can still resolve to a black screen. Check for exactly one
/// `layout::secure_submit` `UNLOCK_TARGET`, the only source of an unlock `SecureSubmit`; distinct
/// from [`NO_LOCK_DECLARED`], which means no `lock` node at all.
const LOCK_CANNOT_AUTHENTICATE: &str = "this config's `lock` surface does not hold exactly one `textfield` with `secure_submit = { capability = \"lock\", action = \"authenticate\" }` \
     and nothing else, so the compositor handing it keyboard focus would arm no field, nothing on it could ever authenticate, and the only way back in \
     would be a VT switch; the lock was refused (ADR-0052 decision 3)";
/// A release request for a lock the compositor never acknowledged with `locked`; nothing was
/// released (see [`App::release_session_lock`]).
const LOCK_NEVER_GRANTED: &str = "the session lock was given up before the compositor ever granted it (no `ext_session_lock_v1::locked` arrived), so nothing was unlocked";
/// `finished` before `locked`: the compositor refused the request, usually because another lock
/// client holds the session; the wire cannot say more.
const LOCK_DENIED: &str = "the compositor denied the session lock; another lock client most likely holds it already (`ext_session_lock_v1::finished` arrived in place \
     of `locked`)";
/// `obelisk.rescue` after the compositor tears down a live lock. ADR-0052 decision 4 uses rescue,
/// not `obelisk.lock.error`, because no lock screen remains to display it.
const LOCK_TORN_DOWN: &str = "the compositor ended the session lock through its own mechanism; the session is unlocked and the lock screen is gone \
     (`ext_session_lock_v1::finished` after `locked`)";
/// Exit code for a gone Supervisor socket (ADR-0059 decision 1), for journals or `$status`; it is
/// distinct from `0` (unclean exit) and `1` (request failure).
pub(super) const EXIT_SUPERVISOR_GONE: i32 = 70;
/// Exit text for a gone Supervisor socket, split by whether this process holds
/// `ext_session_lock_v1` (ADR-0059 decisions 1-2). The locked message must not claim the lock
/// screen stands, which ADR-0058 decision 4 distinguishes from a vetoed reload.
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
/// One `SetSessionLock` decision before touching Wayland (ADR-0042, ADR-0052 decisions 3-4), kept
/// pure because its refusal cases must be testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockCommand {
    /// Call `SessionLockState::lock` and create surfaces.
    Acquire,
    /// Call `SessionLock::unlock`, then tear surfaces down. **The only unlocking value.**
    Release,
    /// Touch no protocol object; report `LockOutcome::Refused` and set `rescue`.
    Refuse(&'static str),
    /// The command already matches the lock state.
    Nothing,
}
/// One `SetSessionLock` decision. `locked = true` refuses before `SessionLockState::lock` when the
/// tracked set has no lock or its resolved tree cannot authenticate (a declared empty node would
/// otherwise evade decision 3). Missing `ext_session_lock_manager_v1` arrives separately as
/// `GlobalError::MissingGlobal`. `locked = false` with no held lock is `Nothing`, not `Release`:
/// `unlock_and_destroy` before `locked` is protocol error `invalid_unlock`.
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
/// Distinguishes `finished` before `locked` (denial) from `finished` after it (compositor teardown)
/// (ADR-0042). `lock::apply` routes them differently. SCTK's `Dispatch2` sets `is_locked()` on
/// `Locked` and never clears it on `Finished`, so its flag is authoritative.
fn finished_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Finished } else { LockOutcome::Refused(LOCK_DENIED.to_string()) }
}

impl App {
    /// [`App::create_surfaces`]'s `lock` arm: always track the instance, but create no
    /// `ext_session_lock_surface_v1` until the compositor grants the lock (ADR-0052 decision 2).
    /// The entry supports scene reloads and is how decision 3 detects a declared lock. `visible`
    /// is forbidden by `lock_spec`; retain the `wl_output` proxy because `get_lock_surface` needs
    /// it.
    pub(super) fn create_lock(&mut self, instance: &SurfaceInstance, outputs: &HashMap<String, wl_output::WlOutput>) {
        let Some(output) = outputs.get(&instance.output) else {
            eprintln!(
                "[obelisk-renderer] instance {:?} names an output that has since gone; skipping",
                instance.instance_id
            );
            return;
        };
        self.surfaces.push(TrackedSurface::new(
            TrackedRole::Lock { output: output.clone(), surface: None },
            instance.instance_id.clone(),
        ));
    }

    /// Creates one lock surface per declared instance lacking one, only while a lock is held. It is
    /// idempotent per output because a second surface is `duplicate_output`, killing the connection
    /// with the session still locked. `expand_instances`
    /// supplies one instance per output and `surface_specs` rejects multiple lock specs, so the
    /// per-instance `surface: None` guard matches the protocol invariant. Called after `lock`, on
    /// `locked`, and from the hotplug path. The protocol wants surfaces immediately; otherwise a
    /// waiting compositor holds a blank frame for its own time limit. No commit: lock surfaces
    /// must wait for their first ack.
    pub(super) fn ensure_lock_surfaces(&mut self, qh: &QueueHandle<App>) {
        // Clone the `Arc` handle so the loop can call through `&mut self` without a second lock.
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
                "[obelisk-renderer] {}: lock surface created, awaiting its configure",
                self.surfaces[index].surface_id
            );
        }
    }

    /// Destroys live lock surfaces but keeps tracking entries. Ordered unlock calls this after
    /// `unlock_and_destroy`: destroying while the output is still locked causes a solid-color
    /// flash. The next lock rebuilds its surfaces through [`App::ensure_lock_surfaces`]. `finished`
    /// has no such window because the compositor already ended the lock.
    fn teardown_lock_surfaces(&mut self) {
        for index in 0..self.surfaces.len() {
            if matches!(self.surfaces[index].role, TrackedRole::Lock { surface: Some(_), .. }) {
                self.drop_role_object(index);
            }
        }
    }

    /// Applies one Supervisor `SetSessionLock` (ADR-0042, ADR-0052 decision 1). Declaration comes
    /// from tracked lock instances; authentication comes from their resolved trees using the same
    /// [`tree_can_authenticate`] predicate keyboard focus uses. `any`, not `all`, is enough across
    /// outputs. Protocol failure is reported as refusal so the shell keeps painting.
    pub(super) fn set_session_lock(&mut self, qh: &QueueHandle<App>, locked: bool) {
        let lock_instances: Vec<&str> = self
            .surfaces
            .iter()
            .filter(|tracked| matches!(tracked.role, TrackedRole::Lock { .. }))
            .map(|tracked| tracked.surface_id.as_str())
            .collect();
        let can_authenticate =
            lock_instances.iter().filter_map(|id| self.client.scene().surface(id)).any(tree_can_authenticate);
        match lock_command(locked, !lock_instances.is_empty(), can_authenticate, self.session_lock.is_some()) {
            LockCommand::Nothing => {}
            LockCommand::Refuse(reason) => self.refuse_lock(reason),
            LockCommand::Acquire => match self.session_lock_state.lock(qh) {
                Ok(lock) => {
                    self.session_lock = Some(lock);
                    // Arm before `locked`: a reload in between must not strip the password field
                    // from the tree the compositor is about to show. Send only the lock fact;
                    // hotplug retires and replaces instance ids.
                    self.client.set_session_locked(true);
                    self.ensure_lock_surfaces(qh);
                    eprintln!(
                        "[obelisk-renderer] session lock requested; waiting for the compositor's `locked` or `finished`"
                    );
                }
                // Preserve `GlobalError`'s own missing-global diagnosis.
                Err(err) => {
                    let reason = format!("this compositor cannot lock the session: {err} (ADR-0042)");
                    self.refuse_lock(&reason);
                }
            },
            LockCommand::Release => self.release_session_lock(),
        }
    }

    /// The only `unlock_and_destroy` path (ADR-0042, ADR-0052):
    /// `SetSessionLock { locked: false }`
    /// sent by Supervisor only from the `pam_outcomes` arm of its `select!` loop, on
    /// `PamOutcome::Success`, making the rule structural, not trust-based. No shutdown, `finished`,
    /// or reload shortcut; SCTK `Drop` deliberately leaves a dying locked Renderer locked. `unlock`
    /// is a no-op before `is_locked()`, and `run` round-trips first because the flag flips on
    /// dispatch; otherwise `Drop` could send forbidden `destroy` and trigger `invalid_destroy`.
    /// Report what happened, not what was requested, via [`release_outcome`].
    fn release_session_lock(&mut self) {
        let Some(lock) = self.session_lock.take() else {
            return;
        };
        // Read before the destructor consumes the flag being reported.
        let was_locked = lock.is_locked();
        lock.unlock();
        // If `unlock` did not destroy it, `Drop` sends `destroy`; SCTK requires locked objects to
        // be unlocked first.
        drop(lock);
        self.teardown_lock_surfaces();
        // The in-place reload may now reshape the lock screen.
        self.client.set_session_locked(false);
        let outcome = release_outcome(was_locked);
        match &outcome {
            LockOutcome::Unlocked => eprintln!("[obelisk-renderer] the session lock was released"),
            _ => eprintln!("[obelisk-renderer] {LOCK_NEVER_GRANTED}"),
        }
        self.report_lock(outcome);
    }

    /// Logs, rescues, and reports a refused lock (ADR-0052 decision 4). Refusal leaves the normal
    /// scene visible, so `rescue` can display the message; wrong passwords reach `obelisk.lock`
    /// while lock surfaces are mapped and everything else is hidden. A later successful
    /// `RendererClient::reevaluate`
    /// clears rescue.
    fn refuse_lock(&mut self, reason: &str) {
        eprintln!("[obelisk-renderer] the session lock was refused: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(LockOutcome::Refused(reason.to_string()));
    }

    /// Queues one `LockReport`; `lock::apply` sees every state transition exactly once and moves
    /// `active` only from these reports.
    fn report_lock(&mut self, outcome: LockOutcome) {
        if let Err(e) = self.outbound_tx.send(RendererFrame::LockReport(LockReport { outcome })) {
            eprintln!("[obelisk-renderer] failed to queue a LockReport for the socket thread: {e}");
        }
    }
}

/// `ext_session_lock_v1` handler (ADR-0042, ADR-0052). `SessionLockState` binds once from the
/// `GlobalList` in [`run`], not through `registry_handlers!`; its `GlobalProxy` reports a missing
/// global to [`App::set_session_lock`].
impl SessionLockHandler for App {
    /// The compositor granted the lock (ADR-0042): the session is locked, every other client's
    /// content is hidden, and this process owns what is on screen until unlock. Usually surfaces
    /// already exist because `set_session_lock` creates them immediately; this catches hotplug
    /// during the wait.
    /// Re-store the same SCTK `Arc`, whose `is_locked()` flag was flipped by dispatch.
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, session_lock: SessionLock) {
        self.session_lock = Some(session_lock);
        self.ensure_lock_surfaces(qh);
        let surfaces = self
            .surfaces
            .iter()
            .filter(|tracked| matches!(tracked.role, TrackedRole::Lock { surface: Some(_), .. }))
            .count();
        eprintln!("[obelisk-renderer] the session is locked; {surfaces} lock surface(s) up");
        self.report_lock(LockOutcome::Locked);
    }

    /// `finished` before `locked` denies the request; after `locked`, the compositor ended a live
    /// lock. [`finished_outcome`] distinguishes them; both set `rescue` (ADR-0052 decision 4).
    ///
    /// `is_locked()` selects the only legal teardown verb: post-`locked`, plain `destroy` is
    /// `invalid_destroy`, kills the connection, and leaves the session unlocked with the shell
    /// dead.
    ///
    /// This is compositor-initiated, not a convenience unlock; our only unlock path is
    /// [`App::release_session_lock`] (ADR-0042).
    ///
    /// Both verbs are destructors; `wayland-backend` drops a request on a destroyed object, so
    /// `SessionLockInner::Drop`'s unconditional `destroy` is harmless here.
    fn finished(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        let outcome = finished_outcome(session_lock.is_locked());
        // Teardown before dropping the lock object; the compositor already ended the lock.
        self.teardown_lock_surfaces();
        if session_lock.is_locked() {
            // The only legal verb after `locked`.
            session_lock.unlock();
        }
        // Before `locked`, `Drop` sends plain `destroy`.
        self.session_lock = None;
        // No lock remains, so [`App::release_session_lock`] is disarmed.
        self.client.set_session_locked(false);
        let reason = match &outcome {
            LockOutcome::Finished => LOCK_TORN_DOWN,
            _ => LOCK_DENIED,
        };
        eprintln!("[obelisk-renderer] the session lock ended: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(outcome);
    }

    /// SCTK's `Dispatch2` has already acked the configure before this runs, so nothing here acks,
    /// exactly as `window` and `popup` do not ack theirs. Its compositor-provided size is exact: a
    /// mismatched buffer is `dimensions_mismatch`. This first configure maps the surface because
    /// lock surfaces cannot commit before it; [`App::bind_and_clear`] maps, binds EGL, paints, and
    /// swaps.
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
