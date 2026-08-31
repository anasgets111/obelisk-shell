//! The session lock: `ext_session_lock_v1` (docs/adr/0042, docs/adr/0052), including the
//! acquire/refuse/release decision tables and the rescue messages `oblisk.rescue` shows once no
//! lock screen is left on the glass to read one on.
//!
//! Creating and tearing down lock surfaces, and the `SessionLockHandler` callbacks, live here; the
//! generic bind/paint/(un)map machinery a lock surface shares with every other role stays in
//! `surface`.

use super::*;
use crate::layout::secure_submit::tree_can_authenticate;
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

/// docs/adr/0052 decision 3's refusal, as the sentence the user reads. A config that declares no
/// `lock` node cannot be locked: acquiring anyway paints nothing, the compositor never unlocks on
/// client death (docs/adr/0042), and the only way out is a VT switch. Locking a user out over a
/// config omission is not fail-secure, it is a denial of service spelled the same way.
const NO_LOCK_DECLARED: &str = "this config declares no `lock` surface (§ 6.4), so locking the session would leave a black screen with no password field and no way back \
     in short of a VT switch; the lock was refused (docs/adr/0052 decision 3)";
/// The second half of docs/adr/0052 decision 3's refusal, and the one the guard was missing.
///
/// `node::lock_spec` requires only an `id` -- `child` is optional -- so `lock { id = "x" }` is a
/// legal declaration that resolves to a surface with no password field, an empty input region and
/// a transparent buffer. Counting tracked `lock` instances said "a lock screen exists" for exactly
/// the black screen the decision refuses to allow, reached through the guard rather than around
/// it. What matters is not whether a `lock` node was written but whether the tree under it holds
/// a `layout::secure_submit`'s `UNLOCK_TARGET` field, the only thing that can produce the
/// `SecureSubmit` an unlock answers.
///
/// A separate sentence from [`NO_LOCK_DECLARED`]: one config is missing a `lock` node, the other
/// is missing a `textfield` inside the one it has.
const LOCK_CANNOT_AUTHENTICATE: &str = "this config's `lock` surface (§ 6.4) does not hold exactly one `textfield` with `secure_submit = { capability = \"lock\", action = \"authenticate\" }` \
     and nothing else, so the compositor handing it keyboard focus would arm no field, nothing on it could ever authenticate, and the only way back in \
     would be a VT switch; the lock was refused (docs/adr/0052 decision 3)";
/// A `SetSessionLock { locked: false }` that reached a lock object the compositor never answered
/// with `locked`. See [`App::release_session_lock`]: nothing was released, because there was
/// nothing up to release.
const LOCK_NEVER_GRANTED: &str = "the session lock was given up before the compositor ever granted it (no `ext_session_lock_v1::locked` arrived), so nothing was unlocked";
/// The other half of `finished`: the compositor answered the `lock` request with an immediate
/// refusal instead of `locked`. Almost always another lock client already holds the session,
/// but it is compositor policy that this side of the wire cannot narrow down further, so the
/// message says what is known and does not guess.
const LOCK_DENIED: &str = "the compositor denied the session lock; another lock client most likely holds it already (`ext_session_lock_v1::finished` arrived in place \
     of `locked`)";
/// What `oblisk.rescue` says when the compositor tore down a lock that really was up. Not a
/// failure of anything this process did: docs/adr/0052 decision 4 routes it here, not to
/// `oblisk.lock`'s `error`, because there is no lock screen left on the glass to read a message on.
const LOCK_TORN_DOWN: &str = "the compositor ended the session lock through its own mechanism; the session is unlocked and the lock screen is gone \
     (`ext_session_lock_v1::finished` after `locked`)";
/// The exit code this process uses when the Supervisor's control socket is gone (docs/adr/0059
/// decision 1). Nobody is left to read it -- the process that classifies exit codes just died --
/// so this is for a journal and `$status`, not a handshake. Distinct from `0` (not a clean exit)
/// and from `1` (not a failure of anything this process was asked to do).
pub(super) const EXIT_SUPERVISOR_GONE: i32 = 70;
/// What this process says on its way out when the Supervisor's control socket is gone, split on
/// whether it holds `ext_session_lock_v1` at that moment (docs/adr/0059 decisions 1 and 2).
///
/// Pure and split out because the locked half can mislead into an unrecoverable state:
/// docs/adr/0058 decision 4 already caught the neighbouring mistake, where a refusal ending "the
/// lock screen that is on screen still stands" is true of a vetoed reload and false of a process
/// that is exiting.
pub(super) fn supervisor_gone_report(holds_session_lock: bool) -> &'static str {
    if holds_session_lock {
        "the Supervisor's control socket is gone while this Renderer holds the session lock. PAM runs in the Supervisor (docs/adr/0028), so this \
         lock screen can no longer authenticate anyone, and exiting without unlocking is what keeps a `kill` from being a way past a lock screen. \
         The session stays locked behind whatever the compositor puts up for a lock client that died, and the way back in is a VT switch \
         (docs/adr/0059 decision 2)"
    } else {
        "the Supervisor's control socket is gone, so this Renderer has no capability data, no `process.run` and no PAM left to serve. Exiting \
         rather than painting a shell that still takes clicks and answers none of them (docs/adr/0059 decision 1)"
    }
}
/// What one `SetSessionLock` asks this process to do, decided before any Wayland object is touched
/// (docs/adr/0042, docs/adr/0052 decisions 3 and 4).
///
/// Pure and separate because the two interesting answers are refusals, and a refusal that only
/// exists inside a `&mut self` method that also talks to the compositor is untestable. See
/// [`lock_command`].
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
/// One `SetSessionLock`, resolved against what this process is already holding.
///
/// `locked = true` has four answers and only one is "take the lock". Two of the other three are
/// refusals, and docs/adr/0052 decision 3 is both: the lock must be refused here, before
/// `SessionLockState::lock` is called, because a lock granted and then found unusable is exactly
/// the black screen the decision exists to prevent.
///
/// The two refusals ask different questions, and both have to be asked. `declares_lock` asks about
/// the tracked surface set. `can_authenticate` asks about that instance's resolved tree, and the
/// first check cannot stand in for it: `lock { id = "x" }` declares an instance that resolves to
/// nothing typable (see [`LOCK_CANNOT_AUTHENTICATE`]). Refusing on the first and granting on the
/// second would waive decision 3 for a config that forgot the node's contents while enforcing it
/// for one that forgot the node.
///
/// The compositor-cannot-lock case is deliberately not an input: `SessionLockState` keeps
/// `ext_session_lock_manager_v1` in a `GlobalProxy`, and `lock` answers `GlobalError::MissingGlobal`
/// when there is none, so the caller maps that `Err` to a `Refuse` with the error's own words.
///
/// `locked = false` against nothing held is `Nothing`, not `Release`: `unlock_and_destroy` on a
/// lock that never got `locked` is the protocol's own `invalid_unlock` error, and this is the
/// guard that keeps the unlock path from sending one.
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
/// dispatched on the lock object being given up (docs/adr/0052 decision 4).
///
/// `Unlocked` is a state transition the Supervisor's `lock::apply` moves its `active` flag on, so
/// reporting one for a lock never granted would tell the Supervisor the session went from locked
/// to unlocked when it was never locked. SCTK's `SessionLock::unlock` is a no-op below
/// `is_locked()`, so nothing was sent and [`LOCK_NEVER_GRANTED`] says so instead.
fn release_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Unlocked } else { LockOutcome::Refused(LOCK_NEVER_GRANTED.to_string()) }
}
/// Which of `ext_session_lock_v1::finished`'s two events this one is (docs/adr/0042), decided by
/// the fact that separates them: whether `locked` was ever sent on this lock object.
///
/// The protocol puts both on one event. "The finished event should be sent immediately on
/// creation of this object if the compositor decides that the locked event will not be sent" is a
/// denial, typically because another lock client already holds it. "If the locked event is sent on
/// creation of this object the finished event may still be sent at some later time" is a lock that
/// was really up and that the compositor then ended through its own secure mechanism.
///
/// They must not collapse into one report: the Supervisor routes them differently (`lock::apply`),
/// a denial is a failure the user has to see while a teardown is a state change already lived
/// through.
///
/// `was_locked` is SCTK's own flag: its `Dispatch2` for `ext_session_lock_v1` sets it on `Locked`
/// and never clears it, including not on `Finished`, so no bookkeeping of ours can drift from it.
fn finished_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Finished } else { LockOutcome::Refused(LOCK_DENIED.to_string()) }
}

impl App {
    /// [`App::create_surfaces`]'s `lock` arm: the tracking entry always, the
    /// `ext_session_lock_surface_v1` never from here (docs/adr/0052 decision 2).
    ///
    /// The entry is what makes the retained scene resolve this instance's tree at all, letting an
    /// in-place reload restyle a live lock screen, and it is the only record that this config
    /// declares a lock screen -- the fact docs/adr/0052 decision 3 refuses a lock on the absence of.
    /// [`App::set_session_lock`] asks that question by looking for these entries.
    ///
    /// No `visible` is consulted and there is none to consult: `layout::node::lock_spec` refuses the
    /// property outright, since the compositor owns this surface's lifetime end to end.
    ///
    /// The `wl_output` is kept rather than the output's name, for the reason [`TrackedRole::Lock`]
    /// gives: `get_lock_surface` takes the proxy, and this is the one place it is already in hand.
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

    /// One `ext_session_lock_surface_v1` for every declared `lock` instance that does not have one
    /// yet, or nothing at all if this process holds no lock (build-steps.md Phase 23 item 1).
    ///
    /// Idempotent per output, a protocol requirement, not tidiness: a second lock surface on one
    /// output is a `duplicate_output` error, killing the connection with the session still locked.
    /// `expand_instances` produces one `lock` instance per output per declared lock spec
    /// (docs/adr/0052 decision 2), so "this instance already has a surface" and "this output already
    /// has one" agree only while a config declares at most one `lock` -- which `crate::socket`'s
    /// `surface_specs` now refuses to let through. The `surface: None` pattern below is the
    /// per-instance half of that invariant; the refusal is the other half.
    ///
    /// Three callers, one job: "make the set of lock surfaces match the set of outputs" is the same
    /// job whenever either set moves. Right after `lock` succeeds, since the protocol asks clients to
    /// immediately create lock surfaces for all outputs present -- the compositor may wait for them
    /// before sending `locked`, and a client that waits for `locked` first guarantees a blank frame
    /// for however long the compositor's time limit is. Again on `locked` itself, for an output
    /// advertised inside that window. And from [`App::create_surfaces`], the hotplug path.
    ///
    /// No commit here, and this is the one role where that is not an oversight: every other create
    /// path ends in the initial commit its shell protocol requires, but "committing the surface
    /// before acking the first configure is a protocol error" here, and the compositor sends that
    /// first configure immediately on `get_lock_surface`.
    pub(super) fn ensure_lock_surfaces(&mut self, qh: &QueueHandle<App>) {
        // Cloned out of `self` so the `&mut self` calls in the loop are free to run; `SessionLock`
        // is an `Arc` handle, so this is a refcount bump and not a second lock.
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
    /// the tracking entries where they are.
    ///
    /// The order is [`App::destroy_surface_by_id`]'s minus its last step: `eglDestroySurface` and
    /// `wl_egl_window_destroy` first ([`App::release_bound`]), then the `SessionLockSurface` handle,
    /// whose `Drop` sends `destroy` and then destroys the `wl_surface`. A `wl_egl_window` still
    /// pointing at a destroyed `wl_surface` is the failure that order prevents.
    ///
    /// The entries survive because the declarations did: a `lock` instance is one retained node for
    /// as long as the config declares it, and the next lock builds its surfaces again through
    /// [`App::ensure_lock_surfaces`].
    ///
    /// When this runs relative to the unlock is load-bearing. On the ordered-unlock path it runs
    /// after `unlock_and_destroy`: destroying a lock surface whose output is still active while the
    /// session is still locked makes the compositor fall back to rendering a solid color, a visible
    /// flash between the password being accepted and the desktop coming back. On the `finished` path
    /// there is no such window: the compositor has already ended the lock.
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

    /// One `SetSessionLock` from the Supervisor (docs/adr/0042, docs/adr/0052 decision 1). The
    /// decision is [`lock_command`], which is pure and tested; this is the protocol traffic it does
    /// not do.
    ///
    /// `declares_lock` is counted off the tracked surface set, not the roster or the scene: that set
    /// is the one place the question has a single answer, since `create_lock` pushes an entry per
    /// `lock` instance and `destroy_surface_by_id` removes it with its output.
    ///
    /// `can_authenticate` goes to the scene: a tracked instance says a `lock` node was written, and
    /// only its resolved tree says whether anything under it could ever produce the `SecureSubmit`
    /// that unlocks (see [`LOCK_CANNOT_AUTHENTICATE`]). The per-tree answer is
    /// [`tree_can_authenticate`], the same predicate keyboard focus arms on. `any`, not `all`, across
    /// instances: one lock surface per output is the protocol's requirement and they all resolve
    /// from the same declaration, so a single typable one is the config being correct.
    ///
    /// A `lock` that fails at the protocol level is a refusal, not a crash, the same tolerance
    /// [`App::show_window`] applies to a missing `xdg_wm_base`: the shell keeps painting, and the one
    /// thing that did not happen says so through the channel docs/adr/0052 decision 4 named for it.
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
                    // Armed here rather than on `locked`. A reload landing between the request and
                    // the grant would otherwise strip the password field out of the very tree the
                    // compositor is about to put on screen -- see `crate::socket`'s
                    // `lock_stays_authenticatable`, which is also why nothing but the fact of the
                    // lock is handed over: the ids the guard above answered on are the ids that
                    // existed *now*, and a monitor hotplug retires and replaces them.
                    self.client.set_session_locked(true);
                    self.ensure_lock_surfaces(qh);
                    eprintln!(
                        "[oblisk-renderer] session lock requested; waiting for the compositor's `locked` or `finished`"
                    );
                }
                // The compositor advertises no `ext_session_lock_manager_v1`. Carried as the
                // error's own words rather than a constant beside the other two, because this is
                // the one refusal whose cause is outside both this shell and its config, and
                // `GlobalError` already says which global is missing.
                Err(err) => {
                    let reason = format!("this compositor cannot lock the session: {err} (docs/adr/0042)");
                    self.refuse_lock(&reason);
                }
            },
            LockCommand::Release => self.release_session_lock(),
        }
    }

    /// `unlock_and_destroy`, and the only path in this process that performs one (docs/adr/0042,
    /// docs/adr/0052's consequences).
    ///
    /// Reachable from exactly one place: a `SetSessionLock { locked: false }`, which the Supervisor
    /// sends only from the `pam_outcomes` arm of its `select!` loop, on a `PamOutcome::Success`.
    /// That makes "never unlock except on a successful authentication" a property of one call site
    /// in the Supervisor rather than a rule the Renderer has to be trusted with. No convenience
    /// path may be added here -- not on shutdown, not on a `finished`, not on a config reload.
    /// SCTK's `Drop` deliberately does not unlock, and the reason is the whole security model: a
    /// Renderer that dies while locked leaves the session locked, and anything in this file that
    /// unlocked on its own initiative would be the one way to turn a crash into an unlocked desktop.
    ///
    /// `SessionLock::unlock` is itself a no-op unless `is_locked()`, so the in-flight case -- a
    /// `lock` request whose `locked` has not arrived -- sends nothing and the `Drop` below sends the
    /// plain `destroy` the protocol requires there instead. That is also why [`run`] round-trips
    /// before calling this: `is_locked()` is set when `locked` is dispatched, not when the
    /// compositor sends it, so without that round trip an unread `locked` would make this send a
    /// plain `destroy` that the compositor answers with `invalid_destroy`.
    ///
    /// The report matches what happened rather than what was asked for ([`release_outcome`]): a
    /// no-op `unlock` released nothing, and the Supervisor's `active` flag moves on these reports.
    fn release_session_lock(&mut self) {
        let Some(lock) = self.session_lock.take() else {
            return;
        };
        // Read before the unlock, because `unlock_and_destroy` is a destructor and the flag it is
        // gated on is the same one being reported here.
        let was_locked = lock.is_locked();
        lock.unlock();
        // Sends `ext_session_lock_v1.destroy` if `unlock` did not already destroy the object, which
        // is SCTK's sanctioned sequence: its own `SessionLock` doc says a locked object must be
        // `unlock`ed before it is dropped.
        drop(lock);
        // After the unlock, per [`App::teardown_lock_surfaces`]'s last paragraph.
        self.teardown_lock_surfaces();
        // Nothing is locked any more, so an in-place reload is free to reshape the lock screen
        // however it likes again, including out of existence.
        self.client.set_session_locked(false);
        let outcome = release_outcome(was_locked);
        match &outcome {
            LockOutcome::Unlocked => eprintln!("[oblisk-renderer] the session lock was released"),
            _ => eprintln!("[oblisk-renderer] {LOCK_NEVER_GRANTED}"),
        }
        self.report_lock(outcome);
    }

    /// A lock that was asked for and did not happen: logged, pushed to `oblisk.rescue`, and reported
    /// (docs/adr/0052 decision 4).
    ///
    /// `rescue` is the right channel: a refused lock leaves the ordinary scene on the glass, so there
    /// is no lock screen for the message to appear on, and `rescue` is rendered by the config's own
    /// surfaces. The wrong-password case is the opposite and does not come here: it happens with the
    /// lock surfaces mapped and everything else hidden, reaching the config as `oblisk.lock` instead.
    ///
    /// Nothing clears this again on purpose: a later successful evaluation clears `rescue` on its own
    /// success path (`RendererClient::handle_reevaluate`), the event that matters -- the ordinary way
    /// out of `NO_LOCK_DECLARED` is editing the config to declare a lock screen, which is itself a
    /// re-evaluation.
    fn refuse_lock(&mut self, reason: &str) {
        eprintln!("[oblisk-renderer] the session lock was refused: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(LockOutcome::Refused(reason.to_string()));
    }

    /// Queues one `LockReport` on the outbound channel, the way `ReadySignal` and
    /// `PresentationEvidence` are queued. Every lock state change goes through here, so the
    /// Supervisor's `lock::apply` sees each transition exactly once -- its `active` flag moves on
    /// these reports alone and on nothing it ordered itself.
    fn report_lock(&mut self, outcome: LockOutcome) {
        if let Err(e) = self.outbound_tx.send(RendererFrame::LockReport(LockReport { outcome })) {
            eprintln!("[oblisk-renderer] failed to queue a LockReport for the socket thread: {e}");
        }
    }
}

/// `ext_session_lock_v1` for the session lock (docs/adr/0042, docs/adr/0052). See
/// `delegate_dispatch2!(App)` at the bottom of this file for why no `delegate_session_lock!` call
/// accompanies this.
///
/// `SessionLockState` is also absent from `registry_handlers![OutputState, SeatState]`, correctly:
/// it is not a `RegistryHandler`. It binds from the `GlobalList` once in [`run`], and its
/// `GlobalProxy` carries the "not advertised" case for [`App::set_session_lock`] to report.
impl SessionLockHandler for App {
    /// The compositor granted the lock: the session is now locked, every other client's content is
    /// hidden, and this process is responsible for what is on screen until it unlocks
    /// (docs/adr/0042).
    ///
    /// The surface creation here is normally a no-op, deliberately: [`App::set_session_lock`]
    /// already created one per output the moment `lock` succeeded, since the protocol asks clients
    /// to create them immediately and lets the compositor wait for them before sending this event,
    /// so the user does not see a blank frame first. What this call catches is an output advertised
    /// inside that window, handled idempotently by [`App::ensure_lock_surfaces`].
    ///
    /// The lock handle is re-stored rather than compared against the one `lock` returned: it is the
    /// same `Arc`, SCTK's dispatch flipped `is_locked()` on it before calling in here, and storing
    /// it costs a refcount bump while removing the only way the two could ever disagree.
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

    /// Two different events, told apart by [`finished_outcome`] and never swallowed. Arriving
    /// before any `locked`, the compositor denied the request. Arriving after one, it ended a lock
    /// that was really up, through its own secure mechanism.
    ///
    /// Both set `rescue` (docs/adr/0052 decision 4), and the test is not severity but whether there
    /// is a lock screen left to read a message on. There is not: a denial never put one up, and a
    /// teardown took the one that was up away, so in both cases the ordinary scene is what the user
    /// is looking at and `rescue` is what it renders.
    ///
    /// Which teardown verb to send is decided by `is_locked()`, and the protocol leaves no choice.
    /// `ext-session-lock-v1` says of `finished`: "the client should make either the destroy request
    /// or the unlock_and_destroy request, depending on whether or not the locked event was received
    /// on this object", and of `ext_session_lock_v1.destroy`: "it is a protocol error to make this
    /// request if the locked event was sent, the unlock_and_destroy request must be used instead".
    /// So a post-`locked` `finished` answered with a plain `destroy` is `invalid_destroy` every
    /// time, and losing the connection here is the worst outcome available: the session ends up
    /// unlocked and the shell is dead, with the `rescue` message set below never reaching a surface.
    ///
    /// This is not the convenience path docs/adr/0042 forbids: that rule is about initiating an
    /// unlock, and the compositor initiated this one through its own secure mechanism --
    /// `finished` is documented as "the compositor has decided that the session lock should be
    /// destroyed". The one path that ends a live lock is still [`App::release_session_lock`],
    /// reached only from a `SetSessionLock { locked: false }`.
    ///
    /// Both verbs are `type="destructor"`, and `wayland-backend` refuses a request on an
    /// already-destroyed object client-side rather than putting it on the wire, so
    /// `SessionLockInner::Drop`'s unconditional `destroy` after this `unlock` is a no-op, not a
    /// second teardown.
    fn finished(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        let outcome = finished_outcome(session_lock.is_locked());
        // Ahead of dropping the lock object, unlike the ordered-unlock path: there is no
        // fall-back-to-solid-colour window to worry about here, because the compositor has already
        // decided the lock is over, and the surfaces are the thing whose `wl_surface`s the EGL side
        // still points at.
        self.teardown_lock_surfaces();
        if session_lock.is_locked() {
            // `unlock_and_destroy`, which is the only verb the protocol accepts once `locked` has
            // been sent. See this method's doc comment: this ends an object, not a session.
            session_lock.unlock();
        }
        // For a denial (no `locked`), `SessionLockInner::Drop`'s plain `destroy` is the correct
        // verb and this is what sends it.
        self.session_lock = None;
        // Whichever of the two events this was, no lock is held now -- disarmed for
        // [`App::release_session_lock`]'s reason.
        self.client.set_session_locked(false);
        let reason = match &outcome {
            LockOutcome::Finished => LOCK_TORN_DOWN,
            _ => LOCK_DENIED,
        };
        eprintln!("[oblisk-renderer] the session lock ended: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(outcome);
    }

    /// One `ext_session_lock_surface_v1.configure`, already acked by SCTK's own `Dispatch2` before
    /// this runs -- so nothing here acks, exactly as the `window` and `popup` paths don't ack their
    /// `xdg_surface`.
    ///
    /// Everything after the lookup is [`App::bind_and_clear`], shared verbatim with the other three
    /// roles. The size is taken as given with no negotiation: a lock surface covers its output, the
    /// compositor knows that output's size, and committing a buffer that does not match the acked
    /// size is the protocol's own `dimensions_mismatch` error.
    ///
    /// This is also the event that maps the surface. `ensure_lock_surfaces` left it in
    /// `MapState::AwaitingConfigure` and performed no initial commit, since
    /// `ext_session_lock_surface_v1` forbids one before the first ack; `bind_and_clear` flips that
    /// to `Mapped`, binds EGL, paints the resolved tree, and `eglSwapBuffers` is the commit that
    /// carries the first buffer.
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
        // docs/adr/0059 decision 2: this process is about to exit, so the message must send the
        // reader to a VT, not to a password field that no longer exists (docs/adr/0058 decision 4's
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
        // docs/adr/0052 decision 3: the refusal must happen before `SessionLockState::lock` is
        // called, since a lock granted and then painted nothing is a black screen with no way out
        // but a VT switch.
        assert_eq!(lock_command(true, false, false, false), LockCommand::Refuse(NO_LOCK_DECLARED));
        assert_eq!(lock_command(true, true, true, false), LockCommand::Acquire);
    }

    #[test]
    fn a_lock_screen_with_no_password_field_is_refused_as_loudly_as_no_lock_screen_at_all() {
        // `lock { id = "x" }` is a legal declaration that resolves to an empty tree, reaching
        // docs/adr/0052 decision 3's black screen through the guard instead of around it, so the
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
        // in this process, reached only on a `PamOutcome::Success` (docs/adr/0042).
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
