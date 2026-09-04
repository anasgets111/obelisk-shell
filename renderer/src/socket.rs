//! Renderer-side Unix control-socket client, and the `SupervisorFrame` handling hanging off it.
//! Connects to `$XDG_RUNTIME_DIR/oblisk-shell.sock`; the Supervisor listens
//! (`supervisor/src/socket.rs`) and gets a `shared::ConnectionHandshake` as the first frame.
//! Two threads, two channels (ADR-0039): the socket thread only does framed I/O ([`pump`]); Lua
//! state lives on the *Wayland* thread instead, since `mlua::Lua` is `!Send` and the paint pass
//! owns the GL context. A `StateSnapshot` only hydrates a capability's signal and dirties the scene
//! (ADR-0044 decision 2), then runs the config's `on_change` handlers for that capability
//! (ADR-0115); only `Reevaluate` triggers a Lua evaluation, classified as `Unchanged`,
//! `TopologyChanged` or `Failed` against `applied_topology`, `None` meaning "safe to apply", not
//! "empty topology" (misclassifying a startup failure would blank the shell). A dropped
//! connection is not reconnected (ADR-0059 decision 1): the Supervisor holds every capability,
//! `process.run` child and PAM, so nothing here is left to reconnect with.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::framing::{self, write_json_frame};
use shared::{
    ApplyPendingReload, ConnectionHandshake, DeselectInput, IdleEvent, ProcessExited, ProcessOutputLine,
    PromoteGeneration, ReevaluateReport, ReevaluateRequest, RendererFrame, SetSessionLock, StateSnapshot,
    SupervisorFrame, Zeroize,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{SurfaceFingerprint, SurfaceSpec};
use crate::layout::secure_submit::lock_stays_authenticatable;
use crate::layout::{self, Scene};
use crate::lua::capability::{Capability, CapabilityHandle, CommandSender};
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::{DirtyFlag, LiveSignalHandle};
use crate::lua::surfaces::{evaluate_and_specs, surface_specs};
use crate::lua::{self, Loader};
use crate::text::shaping::ShapingHandle;

/// This Renderer's own generation id (`OBLISK_GENERATION_ID`, defaulting to `0`), stamped into
/// the handshake and every outbound `CommandEnvelope`/`SecureSubmit`.
pub fn generation_id_from_env() -> u32 {
    std::env::var(shared::GENERATION_ID_ENV).ok().and_then(|value| value.parse().ok()).unwrap_or(0)
}

/// Connects to `path` and sends the handshake identifying `generation_id`, returning the
/// live stream on success.
async fn connect_and_handshake(
    path: &Path,
    generation_id: u32,
) -> Result<UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = UnixStream::connect(path).await?;
    write_json_frame(&mut stream, &ConnectionHandshake { generation_id }).await?;
    Ok(stream)
}

/// Spawns the dedicated connect-and-hold-open thread. A connection failure logs and drops
/// `inbound_tx`, read by the Wayland thread as `Disconnected` (ADR-0059 decision 1). No startup
/// race: `supervisor/src/main.rs` binds the control socket before spawning the first Renderer.
pub fn spawn_client(
    generation_id: u32,
    inbound_tx: std::sync::mpsc::Sender<SupervisorFrame>,
    outbound_rx: mpsc::UnboundedReceiver<RendererFrame>,
) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("control-socket client: failed to start runtime: {err}");
                return;
            }
        };
        runtime.block_on(run(generation_id, inbound_tx, outbound_rx));
    });
}

/// One connection's reload bookkeeping. `applied_topology` is this generation's surface
/// topology, `None` only when nothing has evaluated yet (module doc comment). `pending` holds
/// the evaluated-but-unapplied output/topology between an `Unchanged` `Reevaluate` and its
/// `ApplyPendingReload`. `applied_output` is ADR-0044 decision 2's re-resolve target (§ 15.2),
/// outliving its evaluation so a later push skips re-running `shell.lua`. mlua 0.12's `ValueRef`
/// holds a `WeakLua`, so a retained `mlua::Value` doesn't keep the VM alive and panics via
/// `ValueRef::to_pointer` on a dead state; `Lua` must outlive it, per Rust's field-drop order
/// (see [`RendererClient`]).
struct ReloadState {
    applied_topology: Option<Vec<SurfaceFingerprint>>,
    applied_output: Option<lua::LoadOutput>,
    pending: Option<(u64, lua::LoadOutput, Vec<SurfaceFingerprint>)>,
}

/// What one inbound [`SupervisorFrame`] still owes the Wayland thread after
/// [`RendererClient::handle_frame`]. One enum, not an `Option<u64>` plus an out-parameter:
/// `ActivateDraw` and `SetSessionLock` are the two frames whose work lives on
/// `crate::wayland::App` (EGL/surface state for the first, SCTK's `SessionLockState` and lock
/// surfaces for the second, ADR-0042), and two `Option`s could let a caller service both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Fully serviced inside [`RendererClient::handle_frame`].
    Handled,
    /// § 15.3's `ActivateDraw`: draw the surface set, request presentation feedback per surface,
    /// tagged with this nonce (`crate::wayland::App::activate_draw`).
    ActivateDraw(u64),
    /// ADR-0042/ADR-0052's `SetSessionLock`: match the session lock to this flag
    /// (`crate::wayland::App::set_session_lock`).
    SetSessionLock(bool),
}

/// One generation's whole Lua side: the VM, the retained scene, the live signals, and reload
/// bookkeeping, grouped to travel as one receiver. `!Send` deliberately: `crate::wayland::App`
/// owns one directly (ADR-0039), so a Lua closure, a scene reconcile, and the EGL context are
/// all reachable without a channel hop. **Field order is load-bearing: `loader` must stay
/// last**, since almost every other field holds `mlua::Value`s that don't keep the VM alive (see
/// [`ReloadState`]'s doc comment on `ValueRef`'s `WeakLua`); `loader` first would drop `Lua`
/// before them and panic.
pub struct RendererClient {
    shell_lua_path: PathBuf,
    scene: Scene,
    /// The `(surface, output)` pairs this generation is resolving (`expand_instances`); shared by
    /// [`Self::apply_instances`], [`Self::handle_apply_pending`], [`Self::re_resolve_if_dirty`].
    instances: Vec<SurfaceInstance>,
    /// Whether this process holds or has asked for a session lock, via
    /// [`Self::set_session_locked`]. Arms [`lock_stays_authenticatable`]:
    /// `SurfaceFingerprint::Lock` carries only the `id`, so a lock's `child` reads `Unchanged`
    /// and reloads *in place*, and deleting the password field while up would leave no way out
    /// but a VT switch. Written only by `crate::wayland::App::set_session_lock` and its teardown.
    /// **A `bool`, not instance ids**: a list couldn't survive a hotplug, so the veto reads
    /// [`Self::instances`] instead.
    holds_session_lock: bool,
    /// A clone of `crate::wayland::App`'s `ShapingHandle`: one worker thread, one `FontSystem`
    /// for the whole process (ADR-0023).
    shaping: ShapingHandle,
    /// One handle per capability, keyed by `StateSnapshot.capability` (ADR-0029), seeded from
    /// `shared::Capability::ALL` at construction (ADR-0037). `RefCell`: reached via `&self`.
    capabilities: RefCell<HashMap<String, CapabilityHandle>>,
    /// Cloned into every [`Capability`] this client builds, including the lazy path's.
    commands: CommandSender,
    rescue_handle: LiveSignalHandle,
    /// `oblisk.screens`'s handle (ADR-0041 decision 2), Renderer-sourced, not in `capabilities`.
    screens_handle: LiveSignalHandle,
    /// What `screens_handle` holds, mirrored as JSON so [`Self::set_screens`] detects real change.
    screens_payload: serde_json::Value,
    /// What `rescue_handle` holds, mirrored so [`Self::set_rescue_state`] can detect a no-op.
    rescue_state: (bool, String),
    process_registry: ProcessRegistry,
    /// `oblisk.idle`'s threshold callbacks (ADR-0032), Renderer-sourced like `screens_handle`.
    idle_registry: crate::lua::idle::IdleRegistry,
    /// The scene-dirty flag (ADR-0044 decision 2), cloned into every `LiveSignalHandle` handed out.
    dirty: DirtyFlag,
    state: ReloadState,
    /// Where a `ReevaluateReport` goes: the socket thread's [`pump`] drains this to the wire.
    outbound_tx: mpsc::UnboundedSender<RendererFrame>,
    /// The `oblisk` table, so [`Self::capability_handle`] can add a member later. Above `loader`
    /// for this struct's own drop-order reason.
    oblisk: mlua::Table,
    /// Last, and that is load-bearing: see this struct's own doc comment.
    loader: Loader,
}

impl RendererClient {
    /// Builds one generation's entire Lua side on the calling thread: the VM, the rescue signal,
    /// the `process` global's registry, and the capability roster's seeded signals. Called from
    /// `crate::wayland::run`, never the socket thread: `mlua::Lua` is `!Send` and must be built
    /// on the thread that runs it (ADR-0039). Fatal on failure: a Renderer with no VM can never
    /// evaluate `shell.lua` or put anything on screen.
    pub fn start(
        shaping: ShapingHandle,
        outbound_tx: mpsc::UnboundedSender<RendererFrame>,
        generation_id: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shell_lua_path =
            shared::shell_lua_path().map_err(|err| format!("failed to resolve shell.lua's path: {err}"))?;
        // One flag for the whole generation (ADR-0044 decision 2), so the rescue signal shares it.
        let dirty = DirtyFlag::new();
        // Where `require` looks and nowhere else (ADR-0047 decision 1); derived here so it can't
        // disagree with a second `shared::config_dir()` call.
        let config_dir = shell_lua_path.parent().ok_or("shell.lua's path has no parent directory")?.to_path_buf();
        let loader =
            Loader::new(dirty.clone(), &config_dir).map_err(|err| format!("failed to start the Lua loader: {err}"))?;
        let process_registry = ProcessRegistry::new(generation_id, outbound_tx.clone());
        loader
            .register_process(process_registry.clone())
            .map_err(|err| format!("failed to register the process global: {err}"))?;
        // Same generation id `ProcessRegistry` stamps, so § 7.3's guard can't disagree with it.
        // § 3.2's commands all take this one write path.
        let commands = CommandSender::new(generation_id, outbound_tx);
        let client = Self::new(loader, shell_lua_path, shaping, commands, process_registry, dirty)
            .map_err(|err| format!("failed to build the `oblisk` namespace: {err}"))?;
        Ok(client)
    }

    /// The `oblisk` namespace is [`lua::namespace::build`]'s, not this function's; construction is
    /// separate from frame handling, and [`Self::capability_handle`] holds the lazy fallback for
    /// an unrostered name since that one *is* reached from a `StateSnapshot`.
    fn new(
        loader: Loader,
        shell_lua_path: PathBuf,
        shaping: ShapingHandle,
        commands: CommandSender,
        process_registry: ProcessRegistry,
        dirty: DirtyFlag,
    ) -> mlua::Result<Self> {
        // Taken off `commands`, not a separate clone that could drift from it.
        let outbound_tx = commands.frames();
        let namespace = lua::namespace::build(&loader, &dirty, &commands, &shell_lua_path)?;
        Ok(Self {
            shell_lua_path,
            scene: Scene::new(),
            instances: Vec::new(),
            holds_session_lock: false,
            shaping,
            capabilities: RefCell::new(namespace.capabilities),
            commands,
            rescue_handle: namespace.rescue,
            screens_handle: namespace.screens,
            screens_payload: namespace.screens_payload,
            // Matches what `lua::namespace::build` already put in the `rescue` signal.
            rescue_state: (false, String::new()),
            process_registry,
            idle_registry: namespace.idle,
            dirty,
            state: ReloadState { applied_topology: None, applied_output: None, pending: None },
            outbound_tx,
            oblisk: namespace.table,
            loader,
        })
    }

    /// Writes the `rescue` global's `{ is_rescue, error_log }` table, but only when it differs.
    /// Not an optimization: writing marks the shared `DirtyFlag` (ADR-0044 decision 2), and a
    /// rewrite could leave it set on a generation that must not be mutated (not the memoization
    /// ADR-0044 decision 3 rejects: a genuine transition still dirties). `pub` for
    /// `crate::wayland::App`'s `SessionLockHandler`, per ADR-0052 decision 4: called on a refused
    /// lock and both `finished` cases, the only channel reaching the user there.
    pub fn set_rescue_state(&mut self, is_rescue: bool, error_log: &str) {
        if self.rescue_state.0 == is_rescue && self.rescue_state.1 == error_log {
            return;
        }
        match lua::namespace::rescue_table(&self.loader, is_rescue, error_log) {
            Ok(table) => {
                self.rescue_handle.set(mlua::Value::Table(table));
                self.rescue_state = (is_rescue, error_log.to_string());
            }
            Err(err) => eprintln!("control-socket client: failed to build rescue state: {err}"),
        }
    }

    /// Only hydrates `snapshot.capability`'s own live signal; no Lua evaluation runs here (module
    /// doc comment). `&self`: the lazy-registration path below needs `capabilities`' `RefCell`
    /// anyway.
    fn apply_state_snapshot(&self, snapshot: StateSnapshot) -> mlua::Result<()> {
        let value = self.loader.to_lua_value(&snapshot.payload)?;
        // The revision is what a later `oblisk.<name>:invoke(...)` stamps for § 7.3's guard.
        let handle = self.capability_handle(&snapshot.capability)?;
        let previous = handle.hydrate(value, snapshot.revision);
        // The `on_change` handlers (ADR-0115) run here, after the value landed and before any
        // layout pass: the only Lua a push runs, and not an evaluation of `shell.lua`.
        handle.notify_change(self.loader.lua(), previous);
        Ok(())
    }

    /// Before every evaluation of `shell.lua`: the evaluation registers its `on_change` handlers
    /// afresh, so the previous evaluation's must go, or a config save would double every side
    /// effect (ADR-0115).
    fn clear_change_handlers(&self) {
        for handle in self.capabilities.borrow().values() {
            handle.clear_handlers();
        }
    }

    /// Looks up `capability`'s handle, adding a fresh `oblisk.<capability>` member (`nil`,
    /// revision `0`) the first time it is seen (ADR-0029); unreachable in a debug build, since
    /// `push_snapshot`'s `debug_assert` rejects an off-roster capability first. **Refuses to
    /// overwrite a held name**: `Table::set` is silent over an existing key, and an off-roster
    /// `rescue` push would replace the config-failure signal (ADR-0052 decision 1's bug).
    fn capability_handle(&self, capability: &str) -> mlua::Result<CapabilityHandle> {
        if let Some(handle) = self.capabilities.borrow().get(capability) {
            return Ok(handle.clone());
        }
        if self.oblisk.contains_key(capability)? {
            return Err(mlua::Error::runtime(format!(
                "a StateSnapshot named the unrostered capability {capability:?}, and `oblisk.{capability}` is already something else; refusing to replace it"
            )));
        }
        let (member, handle) = Capability::new(capability, self.dirty.clone(), self.commands.clone());
        self.oblisk.set(capability, member)?;
        self.capabilities.borrow_mut().insert(capability.to_string(), handle.clone());
        Ok(handle)
    }

    /// Evaluates `shell.lua` once at startup; no Supervisor round trip needed yet (ADR-0024,
    /// "safe to apply" per the module doc comment). `state.applied_topology` stays `None` only
    /// when the *evaluation* fails; a failed *apply* leaves it, since the caller has already
    /// bound the declared surfaces and a later topology change needs a new generation (ADR-0038).
    /// Runs before any layer surface is bound (§ 15.2's order), split so the caller can expand
    /// the returned [`SurfaceSpec`](layout::node::SurfaceSpec)s via [`Self::apply_instances`].
    pub fn run_startup_evaluation(&mut self) -> Option<Vec<SurfaceSpec>> {
        self.clear_change_handlers();
        match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                self.state.applied_topology = Some(specs.iter().map(SurfaceSpec::fingerprint).collect());
                // ADR-0044 decision 2's re-resolve target, held so a later push doesn't need
                // shell.lua re-run.
                self.state.applied_output = Some(output);
                Some(specs)
            }
            Err(err) => {
                eprintln!("control-socket client: startup shell.lua evaluation failed: {err}");
                self.set_rescue_state(true, &err.to_string());
                None
            }
        }
    }

    /// Replaces the `(surface, output)` pairs this generation resolves against, from
    /// `crate::wayland::run` before the first apply and from `OutputHandler` on every hotplug
    /// (ADR-0038 decision 3).
    pub fn set_instances(&mut self, instances: Vec<SurfaceInstance>) {
        self.instances = instances;
    }

    /// Arms or disarms the lock-authentication veto every `Scene::apply` carries (ADR-0052
    /// decision 3), the moment the lock is asked for rather than when `locked` arrives: a reload
    /// landing in that window would strip the field from the tree the compositor is about to show.
    pub fn set_session_locked(&mut self, locked: bool) {
        self.holds_session_lock = locked;
    }

    /// The set [`Self::set_instances`] last stored, so `crate::wayland::App` can diff a fresh
    /// expansion against it without keeping a second copy that could drift from this one.
    pub fn instances(&self) -> &[SurfaceInstance] {
        &self.instances
    }

    /// The whole declared surface roster of the evaluation applied, re-parsed from
    /// `applied_output` rather than re-read from `shell.lua`. What a hotplug expands against
    /// (ADR-0038 decision 3): re-evaluating would race the Supervisor's own `Reevaluate`
    /// (ADR-0041 decision 4). Empty when nothing has ever applied.
    pub fn applied_surface_specs(&self) -> Vec<SurfaceSpec> {
        let Some(output) = self.state.applied_output.as_ref() else {
            return Vec::new();
        };
        match surface_specs(output) {
            Ok(specs) => specs,
            Err(err) => {
                // Unreachable in practice: `surface_specs` already succeeded on `applied_output`.
                // Not a panic, which would take down a shell that is painting fine.
                eprintln!("control-socket client: the applied evaluation's surface specs no longer parse: {err}");
                Vec::new()
            }
        }
    }

    /// Asks the Supervisor to start a reload cycle (ADR-0041 decision 4): a topology change only
    /// it decides (ADR-0041 decision 3). Carries no sequence: `supervisor/src/main.rs` drops any
    /// report not naming its own `next_sequence`, so this only asks it to *begin* a cycle. Sent
    /// when a config that loops over `screens` declares a different surface set.
    pub fn request_reload(&self) {
        if let Err(err) = self.outbound_tx.send(RendererFrame::RequestReload) {
            eprintln!("control-socket client: failed to request a reload after an output change: {err}");
        }
    }

    /// Pushes the `screens` signal's new value (ADR-0041 decision 2), reporting whether it
    /// changed. Same early-return rule as `set_rescue_state`: the caller gates
    /// [`Self::request_reload`] on this, so a re-push would ask for an unjustified reload too.
    pub fn set_screens(&mut self, payload: serde_json::Value) -> bool {
        if self.screens_payload == payload {
            return false;
        }
        match self.loader.to_lua_value(&payload) {
            Ok(value) => {
                self.screens_handle.set(value);
                self.screens_payload = payload;
                true
            }
            Err(err) => {
                eprintln!("control-socket client: failed to convert the screen list to a Lua value: {err}");
                false
            }
        }
    }

    /// One instance's `available` size, replaced by what the compositor configured, dirtying the
    /// scene (ADR-0023) via [`DirtyFlag`] from ADR-0044 decision 2. An unknown `instance_id` is
    /// ignored: doing nothing beats dirtying the whole scene over a surface that shouldn't exist.
    pub fn set_instance_size(&mut self, instance_id: &str, size: layout::LogicalSize) {
        let Some(instance) = self.instances.iter_mut().find(|i| i.instance_id == instance_id) else {
            return;
        };
        if instance.available == size {
            // Same early-return rule as `set_rescue_state`'s.
            return;
        }
        instance.available = size;
        self.dirty.mark();
    }

    /// The second half of [`Self::run_startup_evaluation`]'s split: resolves the last evaluation
    /// against the current instance set, setting rescue on failure. Returns whether it succeeded,
    /// so `crate::wayland::run` can tell a Candidate that must exit from one that may carry on.
    /// Takes no instance argument: [`Self::set_instances`] is the one place the set is written.
    pub fn apply_instances(&mut self) -> bool {
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        let (instances, locked) = (&self.instances, self.holds_session_lock);
        let applied =
            self.scene.apply_admitting(&output.surfaces, instances, &self.shaping, self.loader.lua(), |scene| {
                lock_stays_authenticatable(scene, instances, locked)
            });
        match applied {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &self.instances);
                start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
                // Nothing holds a lease, so nothing can be holding a subtree this apply retired.
                self.scene.release_all_retired();
                self.set_rescue_state(false, "");
                // Accounts for `set_screens`'s seed (ADR-0041 decision 2), which must run before
                // the startup evaluation; only on success, so a
                // failed apply leaves the flag for the next one.
                self.dirty.take();
                true
            }
            Err(err) => {
                eprintln!("control-socket client: startup shell.lua evaluated but failed to apply to the scene: {err}");
                self.set_rescue_state(true, &err.to_string());
                false
            }
        }
    }

    /// The retained scene, for `crate::wayland::App::paint_surface` to look one instance's
    /// resolved tree up in by the same `"{id}@{output}"` id its `TrackedSurface` carries.
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// This generation's `Lua`, for building the one argument `button`'s `on_click` takes
    /// (ADR-0050 decision 3): `crate::wayland::App` holds the `mlua::Function` but no VM.
    /// Callers must not hold the borrow across the Lua call it feeds; see
    /// [`crate::wayland::App::fire_on_click`].
    pub fn lua(&self) -> &mlua::Lua {
        self.loader.lua()
    }

    /// Handles one inbound `SupervisorFrame`, decoded by [`pump`]. Returns [`FrameOutcome`]:
    /// `Handled`, or a hand-back needing `crate::wayland::App` state: EGL/surface for a draw
    /// (§ 15.3), or SCTK's `SessionLockState`/lock surfaces for `SetSessionLock` (ADR-0042).
    #[must_use]
    pub fn handle_frame(&mut self, frame: SupervisorFrame) -> FrameOutcome {
        match frame {
            SupervisorFrame::StateSnapshot(snapshot) => {
                if let Err(err) = self.apply_state_snapshot(snapshot) {
                    eprintln!("control-socket client: failed to convert a pushed StateSnapshot to a Lua value: {err}");
                }
            }
            SupervisorFrame::Reevaluate(request) => self.handle_reevaluate(request),
            SupervisorFrame::ApplyPendingReload(apply) => self.handle_apply_pending(apply),
            SupervisorFrame::ActivateDraw(activate) => return FrameOutcome::ActivateDraw(activate.nonce),
            SupervisorFrame::DeselectInput(DeselectInput { surface_id }) => {
                // No per-surface input-region/focus machinery exists yet (ADR-0025).
                eprintln!(
                    "control-socket client: DeselectInput({surface_id}) received (no real input-region wiring yet)"
                );
            }
            SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id }) => {
                eprintln!("control-socket client: PromoteGeneration({surface_id}) received (no real focus wiring yet)");
            }
            SupervisorFrame::ProcessOutput(ProcessOutputLine { id, stream, line }) => {
                self.process_registry.dispatch_output(id, stream, line);
            }
            SupervisorFrame::ProcessExited(ProcessExited { id, code }) => {
                self.process_registry.dispatch_exit(id, code);
            }
            // `ext_session_lock_v1` is a Wayland object; servicing it lives on
            // `crate::wayland::App` (ADR-0042, ADR-0052 decision 1).
            SupervisorFrame::SetSessionLock(SetSessionLock { locked }) => return FrameOutcome::SetSessionLock(locked),
            // Not checked: the Supervisor addresses this process via its own socket (ADR-0032).
            SupervisorFrame::IdleEvent(IdleEvent { generation_id: _, threshold_sec, state }) => {
                self.idle_registry.dispatch_event(threshold_sec, state);
            }
            // ADR-0112: `oblisk set`/`oblisk toggle`. Refused by name to stderr, the only place a
            // keybind's mistake can be reported; the write itself marks the scene dirty.
            SupervisorFrame::SetState(set) => {
                if let Err(why) = lua::signal::write_state(self.lua(), &set) {
                    eprintln!(
                        "control-socket client: `oblisk` asked to write state {:?} and was refused: {why}",
                        set.name
                    );
                }
            }
        }
        FrameOutcome::Handled
    }

    /// Runs one `Reevaluate` request: evaluates `shell.lua`, classifies against
    /// `state.applied_topology`, updates `state.pending`/rescue, and queues the verdict.
    /// `None` counts as "not changed" (module doc comment). Diffs only each spec's
    /// [`SurfaceFingerprint`](layout::node::SurfaceFingerprint) (ADR-0038 decision 2, ADR-0049
    /// decision 3): a `margin`, `keyboard_interactivity`, `exclusive`, size, or `window` `title`
    /// edit is accepted on a live object, so it reports `Unchanged` and reloads in place, not the
    /// generation swap comparing whole specs would trigger.
    fn handle_reevaluate(&mut self, request: ReevaluateRequest) {
        self.clear_change_handlers();
        let report = match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                let topology: Vec<SurfaceFingerprint> = specs.iter().map(SurfaceSpec::fingerprint).collect();
                self.set_rescue_state(false, "");
                let topology_changed = self.state.applied_topology.as_ref().is_some_and(|applied| applied != &topology);
                if topology_changed {
                    // The swap path is a different generation's job; this one's scene stays put.
                    ReevaluateReport::TopologyChanged { sequence: request.sequence }
                } else {
                    self.state.pending = Some((request.sequence, output, topology));
                    ReevaluateReport::Unchanged { sequence: request.sequence }
                }
            }
            Err(err) => {
                self.set_rescue_state(true, &err.to_string());
                ReevaluateReport::Failed { sequence: request.sequence, error: err.to_string() }
            }
        };

        if let Err(err) = self.outbound_tx.send(RendererFrame::ReevaluateReport(report)) {
            eprintln!("control-socket client: failed to send a ReevaluateReport: {err}");
        }
    }

    /// Applies `state.pending` to the `Scene` only if it's still the evaluation `apply.sequence`
    /// refers to: a mismatch means a newer `Reevaluate` already superseded it, logged and ignored.
    fn handle_apply_pending(&mut self, apply: ApplyPendingReload) {
        if !matches!(&self.state.pending, Some((sequence, _, _)) if *sequence == apply.sequence) {
            eprintln!(
                "control-socket client: ApplyPendingReload({}) doesn't match the currently pending reload; ignoring",
                apply.sequence
            );
            return;
        }
        let (_, output, topology) = self.state.pending.take().expect("just confirmed Some above");
        let (instances, locked) = (&self.instances, self.holds_session_lock);
        match self.scene.apply_admitting(&output.surfaces, instances, &self.shaping, self.loader.lua(), |scene| {
            lock_stays_authenticatable(scene, instances, locked)
        }) {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &self.instances);
                start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
                self.scene.release_all_retired();
                // `ApplyPendingReload` only follows `Unchanged`, so this matches the earlier write.
                self.state.applied_topology = Some(topology);
                // ADR-0044 decision 2's re-resolve target.
                self.state.applied_output = Some(output);
                // Nothing else told the screen; the poll loop repaints on `re_resolve_if_dirty`.
                self.dirty.mark();
            }
            Err(err) => {
                eprintln!("control-socket client: ApplyPendingReload's stored evaluation failed to apply: {err}")
            }
        }
    }

    /// Re-runs `Scene::apply` against `state.applied_output` when a live signal's `set` marked
    /// the scene dirty (ADR-0044 decision 2). Does not touch `shell.lua`: `applied_output`'s
    /// retained tree still holds the `Signal` handles Lua put there, readable via this client's
    /// field ordering through decision 1's resolve-at-layout-time rule. Called once per poll turn
    /// after draining inbound frames: `DirtyFlag::take` collapses many pushes into one `true`.
    /// Returns whether it re-resolved; `false` covers "nothing was dirty" and "failed".
    pub fn re_resolve_if_dirty(&mut self) -> bool {
        // Checked *before* the flag is taken: taking it first would let a config that fails its
        // first apply swallow every push and stay blank until an inotify edit forces a re-eval.
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        if !self.dirty.take() {
            return false;
        }
        let (instances, locked) = (&self.instances, self.holds_session_lock);
        let applied =
            self.scene.apply_admitting(&output.surfaces, instances, &self.shaping, self.loader.lua(), |scene| {
                lock_stays_authenticatable(scene, instances, locked)
            });
        if let Err(err) = applied {
            // Rolls back on error, keeping the prior scene on screen. Not `set_rescue_state`:
            // that's for shell.lua failing to evaluate, not one capability's rejected push.
            //
            // ponytail: logging forever, nothing user-visible. Upgrade: a rescue-adjacent
            // channel for a rejected pushed value.
            eprintln!("control-socket client: dirty-scene re-resolve failed, keeping the prior scene: {err}");
            return false;
        }
        // The one apply site that leaks an undrained lease bag at cadence: a re-resolve that
        // shortens a `children` list retires the tail on every poll turn carrying a push.
        self.scene.release_all_retired();
        dump_layout_if_asked(&self.scene);
        true
    }
}

/// `OBLISK_DUMP_LAYOUT=<instance id>`, e.g. `panel_host@eDP-1`: prints that surface's resolved
/// tree -- every visible node's kind, rect and, for a `text`, its content -- after each pass.
/// Diagnostic only, off unless asked. The one geometry question a screenshot cannot answer is
/// which node came out the wrong size, and the layout that goes wrong in a session is the one
/// the test harness did not think to build (a card placed where the bell is, at the output's
/// scale, with a feed the Supervisor had by then), so this reads the live answer instead.
fn dump_layout_if_asked(scene: &Scene) {
    let Ok(wanted) = std::env::var("OBLISK_DUMP_LAYOUT") else { return };
    let Some(surface) = scene.surface(&wanted) else { return };
    fn walk(node: &crate::layout::ResolvedNode, depth: usize, out: &mut String) {
        if !node.visible {
            return;
        }
        let text = node
            .properties
            .get("content")
            .map(|value| format!(" {}", crate::layout::node::preview_for_error(value)))
            .unwrap_or_default();
        out.push_str(&format!("{}{} {:?}{text}\n", "  ".repeat(depth), node.kind, node.rect));
        for child in &node.children {
            walk(child, depth + 1, out);
        }
    }
    let mut out = format!("layout dump: {wanted}\n");
    walk(&surface, 0, &mut out);
    eprint!("{out}");
}

async fn run(
    generation_id: u32,
    inbound_tx: std::sync::mpsc::Sender<SupervisorFrame>,
    mut outbound_rx: mpsc::UnboundedReceiver<RendererFrame>,
) {
    let path = match shared::control_socket_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("control-socket client: {err}");
            return;
        }
    };

    let stream = match connect_and_handshake(&path, generation_id).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("control-socket client: failed to connect to {}: {err}", path.display());
            return;
        }
    };

    let (mut read_half, mut write_half) = stream.into_split();
    pump(&mut read_half, &mut write_half, &inbound_tx, &mut outbound_rx).await;
}

/// The socket thread's entire job after the handshake: forward every decoded `SupervisorFrame`
/// to the Wayland dispatch thread, and write every queued `RendererFrame` to the wire (ADR-0039).
/// A frame that fails to decode is a transport-level failure (one sender, fixed shapes: a bad
/// frame means desync), unlike an expected, recoverable `ApplyPendingReload` sequence mismatch.
/// Read and write get their own long-lived loop, selected as two whole futures rather than one
/// frame each: `read_json_frame`'s two sequential `read_exact` awaits keep partial progress in
/// that future, so racing it against `outbound_rx.recv()` in one `select!` would drop it whenever
/// outbound won, desyncing the connection; neither loop completes normally, so this never happens.
async fn pump<R, W>(
    read_half: &mut R,
    write_half: &mut W,
    inbound_tx: &std::sync::mpsc::Sender<SupervisorFrame>,
    outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reader = async {
        loop {
            match framing::read_json_frame::<_, SupervisorFrame>(read_half).await {
                Ok(frame) => {
                    if let Err(err) = inbound_tx.send(frame) {
                        eprintln!("control-socket client: the Wayland thread is gone; stopping the socket loop: {err}");
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("control-socket client: connection ended: {err}");
                    break;
                }
            }
        }
    };
    let writer = async {
        while let Some(mut frame) = outbound_rx.recv().await {
            if let Err(err) = write_json_frame(write_half, &frame).await {
                eprintln!("control-socket client: failed to send a {} frame: {err}", frame_label(&frame));
            }
            // Scrubbed the instant the write completes, not left to `Drop` (ADR-0005/ADR-0027).
            //
            // ponytail: covers only copies this site controls; `write_json_frame`'s
            // `serde_json::to_vec` drops its own buffer unscrubbed. Upgrade: non-JSON wire path.
            if let RendererFrame::SecureSubmit(inner) = &mut frame {
                inner.secret.zeroize();
            }
        }
    };
    tokio::pin!(reader, writer);
    tokio::select! {
        _ = &mut reader => {}
        _ = &mut writer => {}
    }
}

/// Names an outbound frame for a write-failure log line. A fixed label per variant, not
/// `{frame:?}`: `RendererFrame`'s derived `Debug` would print a `SecureSubmit`'s `secret` bytes
/// into the log (ADR-0005).
fn frame_label(frame: &RendererFrame) -> &'static str {
    match frame {
        RendererFrame::ReadySignal(_) => "ReadySignal",
        RendererFrame::PresentationEvidence(_) => "PresentationEvidence",
        RendererFrame::ReevaluateReport(_) => "ReevaluateReport",
        RendererFrame::Command(_) => "Command",
        RendererFrame::SecureSubmit(_) => "SecureSubmit",
        RendererFrame::LockReport(_) => "LockReport",
        RendererFrame::RequestReload => "RequestReload",
        RendererFrame::StartCapability { .. } => "StartCapability",
        // Never sent by this process (ADR-0112), but the label costs nothing and a wildcard would
        // let the next variant slip past unnamed.
        RendererFrame::SetState(_) => "SetState",
    }
}

/// Starts every capability an applied tree names in a `textfield`'s `secure_submit` (ADR-0070
/// decision 5), so a password prompt registers its agent even if nothing reads the member.
/// Deduplicated by `CommandSender::start_capability`.
///
/// ponytail: not called from `re_resolve_if_dirty` (`Scene::surface` deep-clones every repaint).
/// Gap: a later-pushed `textfield` waits for the next evaluation. Upgrade: an accumulated roster.
fn start_secure_submit_capabilities(scene: &Scene, instances: &[SurfaceInstance], commands: &CommandSender) {
    for instance in instances {
        let Some(tree) = scene.surface(&instance.instance_id) else { continue };
        for target in crate::layout::secure_submit::secure_submit_targets(&tree) {
            commands.start_capability(&target.capability);
        }
    }
}

/// Logs each surface *instance*'s resolved geometry after a `scene.apply`, diagnostic only.
/// Iterates instances, not declared surfaces: one declaration can be several instances at
/// different sizes.
fn log_applied_surfaces(scene: &Scene, instances: &[SurfaceInstance]) {
    dump_layout_if_asked(scene);
    for instance in instances {
        match scene.surface(&instance.instance_id) {
            Some(r) => eprintln!(
                "layout resolved: surface {:?} on {:?} kind={} rect={:?} visible={} children={} properties={}",
                instance.instance_id,
                instance.output,
                r.kind,
                r.rect,
                r.visible,
                r.children.len(),
                r.properties.len()
            ),
            None => {
                eprintln!("layout resolved but surface {:?} is absent from the applied scene", instance.instance_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::instance::{OutputGeometry, expand_instances};
    use crate::layout::node::LayerKind;
    use shared::framing::read_json_frame;
    use shared::{ActivateDraw, CommandEnvelope, PresentationEvidence, ReadySignal, SecureSubmit};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn connect_and_handshake_sends_a_handshake_the_listener_can_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let client = tokio::spawn({
            let path = path.clone();
            async move { connect_and_handshake(&path, 9).await }
        });

        let (mut server_side, _addr) = listener.accept().await.unwrap();
        let handshake: ConnectionHandshake = read_json_frame(&mut server_side).await.unwrap();
        assert_eq!(handshake.generation_id, 9);

        client.await.unwrap().unwrap();
    }

    fn write_shell_lua(dir: &std::path::Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("shell.lua");
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Evaluates `setup` above a minimal `panel` and reads one of the globals it left behind.
    ///
    /// A global rather than a value smuggled onto the returned node: a node refuses a key no
    /// parser of its kind reads (`lua::nodes::NODE_PROPERTIES`), and a probe name is exactly
    /// such a key.
    fn probe<T: mlua::FromLua>(loader: &Loader, setup: &str, name: &str) -> T {
        loader.evaluate(&format!("{setup}\nreturn panel {{ id = \"_probe\", layer = \"Top\" }}")).unwrap();
        loader.lua().globals().get(name).unwrap()
    }

    /// Reads `rescue:get()`'s current fields back out by evaluating a tiny probe script --
    /// `LiveSignalHandle` only exposes `set`, so this is the only way to observe what a prior
    /// `set_rescue_state` call actually stored.
    fn rescue_state(loader: &Loader) -> (bool, String) {
        let setup = "is_rescue, error_log = oblisk.rescue:get().is_rescue, oblisk.rescue:get().error_log";
        (probe(loader, setup, "is_rescue"), probe(loader, setup, "error_log"))
    }

    /// A client wired to a real outbound channel, whose receiver is handed back so a test can
    /// read whatever the client queued for the socket thread.
    fn test_client(shell_lua_path: &std::path::Path) -> (RendererClient, mpsc::UnboundedReceiver<RendererFrame>) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let dirty = DirtyFlag::new();
        let loader = Loader::new(dirty.clone(), shell_lua_path.parent().unwrap()).unwrap();
        let process_registry = ProcessRegistry::new(0, outbound_tx.clone());
        loader.register_process(process_registry.clone()).unwrap();
        let commands = CommandSender::new(0, outbound_tx);
        let client = RendererClient::new(
            loader,
            shell_lua_path.to_path_buf(),
            ShapingHandle::spawn(),
            commands,
            process_registry,
            dirty,
        )
        .unwrap();
        (client, outbound_rx)
    }

    /// The one output every fixture below resolves against: a single 1920x1080 `"TEST"` monitor
    /// keeps instance ids readable (`"bar@TEST"`).
    fn test_outputs() -> Vec<OutputGeometry> {
        vec![OutputGeometry { name: "TEST".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }]
    }

    /// `crate::wayland::run`'s whole startup sequence in one call (§ 15.2's Candidate order):
    /// evaluate, expand the specs into instances, store them, apply.
    fn run_startup(client: &mut RendererClient) -> bool {
        let Some(specs) = client.run_startup_evaluation() else {
            return false;
        };
        let instances = expand_instances(&specs, &test_outputs());
        client.set_instances(instances);
        client.apply_instances()
    }

    /// The instance set for a config declaring exactly `ids`, for tests that seed `state.pending`
    /// by hand instead of going through [`run_startup`].
    fn instances_for(ids: &[&str]) -> Vec<SurfaceInstance> {
        ids.iter()
            .map(|id| SurfaceInstance {
                instance_id: format!("{id}@TEST"),
                declared_id: (*id).to_string(),
                output: "TEST".to_string(),
                available: layout::LogicalSize { width: 1920.0, height: 1080.0 },
            })
            .collect()
    }

    /// The one frame `client` queued, or a panic naming what was missing.
    /// The next queued frame that is not a capability start.
    ///
    /// Skipping those is not hiding them: reading `oblisk.lock` at all queues one
    /// (ADR-0070 decision 1), so every test that reaches a capability would otherwise have to
    /// step over it before asserting on what it actually queued.
    /// `a_capability_read_asks_the_supervisor_to_start_it` is what holds the starts to account.
    fn queued_frame(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> RendererFrame {
        loop {
            match outbound_rx.try_recv().expect("a frame must have been queued for the socket thread") {
                RendererFrame::StartCapability { .. } => continue,
                frame => return frame,
            }
        }
    }

    /// Every frame queued so far, so a test can assert on the starts `queued_frame` steps over.
    fn queued_starts(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Vec<String> {
        let mut started = Vec::new();
        while let Ok(frame) = outbound_rx.try_recv() {
            if let RendererFrame::StartCapability { capability } = frame {
                started.push(capability);
            }
        }
        started
    }

    /// ADR-0070 decision 1: the read is the start. Nothing else in this process asks the
    /// Supervisor to build a controller, so a capability a config never mentions never runs.
    #[test]
    fn a_capability_read_asks_the_supervisor_to_start_it() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load("local _ = oblisk.audio").exec().unwrap();

        assert_eq!(queued_starts(&mut outbound_rx), vec!["audio".to_string()]);
    }

    /// The whole point of the gate: an evaluation that touches nothing costs nothing.
    #[test]
    fn a_config_that_reads_no_capability_starts_none() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load("local _ = oblisk.version.major").exec().unwrap();

        assert!(queued_starts(&mut outbound_rx).is_empty(), "`version` is off the roster and has nothing behind it");
    }

    /// `__index` fires once per name, because the member is moved onto the table on the way out.
    /// A `computed` inside a `list`'s `itemfn` reads `oblisk.audio` once per row per layout pass,
    /// so a start per read would be a frame per row per frame.
    #[test]
    fn re_reading_a_capability_queues_no_second_start() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load("for _ = 1, 50 do local _ = oblisk.audio end").exec().unwrap();

        assert_eq!(queued_starts(&mut outbound_rx), vec!["audio".to_string()]);
    }

    /// A typo must stay an ordinary nil, so the config's own line is what the error names. The
    /// metamethod answering with anything else would turn `oblisk.audioo:get()` into an error
    /// raised from inside the engine.
    #[test]
    fn a_name_no_capability_owns_reads_nil_and_starts_nothing() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        let is_nil: bool = client.loader.lua().load("return oblisk.audioo == nil").eval().unwrap();

        assert!(is_nil);
        assert!(queued_starts(&mut outbound_rx).is_empty());
    }

    /// ADR-0070 decision 5. polkit has no roster entry and no `oblisk.polkit`, so a
    /// `secure_submit` naming it is the only thing a config can write that asks for the
    /// authentication agent.
    #[test]
    fn a_secure_submit_target_starts_the_capability_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        std::fs::write(
            &path,
            r#"return panel { id = "prompt", layer = "Top", child = textfield {
                   secure_submit = { capability = "polkit", action = "authenticate" } } }"#,
        )
        .unwrap();
        let (mut client, mut outbound_rx) = test_client(&path);

        client.run_startup_evaluation().unwrap();
        client.set_instances(instances_for(&["prompt"]));
        assert!(client.apply_instances());

        assert!(queued_starts(&mut outbound_rx).contains(&"polkit".to_string()));
    }

    #[test]
    fn apply_state_snapshot_updates_the_live_signal_without_evaluating_shell_lua() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let snapshot = StateSnapshot {
            capability: "audio".to_string(),
            revision: 1,
            payload: serde_json::json!({ "app_name": "Zen" }),
        };
        client.apply_state_snapshot(snapshot).unwrap();

        assert_eq!(probe::<String>(&client.loader, "app_name = oblisk.audio:get().app_name", "app_name"), "Zen");
    }

    #[test]
    fn apply_state_snapshot_lazily_registers_an_unrostered_capabilitys_live_signal() {
        // ADR-0029: the first StateSnapshot naming an off-roster capability must create the
        // Lua global on the spot, not error.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        assert!(shared::Capability::from_name("workspace").is_none(), "this test needs a genuinely unrostered name");

        let snapshot = StateSnapshot {
            capability: "workspace".to_string(),
            revision: 1,
            payload: serde_json::json!({ "active": 2 }),
        };
        client.apply_state_snapshot(snapshot).unwrap();

        assert_eq!(probe::<i64>(&client.loader, "active = oblisk.workspace:get().active", "active"), 2);
    }

    /// Every bar-backing capability at or past the width its module can draw, for
    /// [`the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing`]. The strings
    /// run past each module's own `util.truncate` limit deliberately: the module clamps them, and
    /// a test feeding short ones would be measuring the clamp rather than the layout.
    fn widest_bar_snapshots() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("audio", serde_json::json!({ "volume": 1.0, "muted": false, "apps": [] })),
            ("brightness", serde_json::json!({ "percent": 100 })),
            ("battery", serde_json::json!({ "present": true, "percent": 100, "charging": true })),
            ("power", serde_json::json!({ "on_battery": true, "energy_rate": 22.5, "active_profile": "performance" })),
            (
                "updates",
                serde_json::json!({ "count": 0, "installing": true, "install_current_step": 128, "install_total_steps": 512 }),
            ),
            ("keyboard", serde_json::json!({ "active_layout": "English (US, intl.)", "caps_lock": true })),
            ("privacy", serde_json::json!({ "camera_users": [{ "app_name": "A Video Conferencing Application" }] })),
            (
                "network",
                serde_json::json!({ "scanning": false, "available_networks": [{ "ssid": "a-long-access-point-name", "strength": 100, "active": true }] }),
            ),
            (
                "bluetooth",
                serde_json::json!({ "enabled": true, "connected_devices": [{ "name": "A Long Bluetooth Device Name", "battery": 100 }] }),
            ),
            (
                "mpris",
                serde_json::json!({ "players": [{ "title": "A Rather Long Track Title", "artist": "A Rather Long Artist Name", "play_state": "Playing" }] }),
            ),
            (
                "workspaces",
                serde_json::json!({
                    "active_client": { "class": "org.example.LongClass", "title": "A window title long enough to be truncated" },
                    "outputs": [{
                        "name": "TEST",
                        "active_workspace": 1,
                        "focused_workspace": 1,
                        "workspaces": (1..=12).map(|n| serde_json::json!({ "id": n, "idx": n })).collect::<Vec<_>>(),
                    }],
                }),
            ),
            (
                "tray",
                serde_json::json!({
                    "items": (0..6)
                        .map(|n| serde_json::json!({ "id": format!("item-{n}"), "name": format!("Tray Item {n}"), "icon_name": "application-x-executable" }))
                        .collect::<Vec<_>>(),
                }),
            ),
            ("system", serde_json::json!({ "time": 1_700_000_000 })),
        ]
    }

    #[test]
    fn a_hover_bound_by_a_config_survives_resolution_and_drives_what_it_is_bound_to() {
        // The half of ADR-0062 no unit test on either side reaches: that a `hover` handle a
        // config wrote into a node property is still a handle by the time the pointer handler sees
        // the resolved tree (decision 3), and that writing it moves what a second node bound it to.
        // `pointer_frame` itself needs a real compositor, so this drives `layout::hover` against a
        // genuinely resolved tree instead -- everything between the config and the write.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            local hovered = hover("pill")
            return { panel {
                id = "bar", layer = "Top", width = "Fill", height = 40,
                child = row {
                    width = 100, height = 20, hover = hovered,
                    children = { rect { id = "tip", width = 10, height = 10, visible = hovered } },
                },
            } }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "the config must resolve");

        let hover_row =
            |client: &RendererClient| client.scene.surface("bar@TEST").expect("the bar resolves").children[0].clone();
        assert!(!hover_row(&client).children[0].visible, "nothing is hovered before the pointer arrives");

        // The pointer lands inside the row. `hover_writes` is what `App::sync_hover` calls, given
        // the same tree it would be given.
        let tree = client.scene.surface("bar@TEST").unwrap();
        let writes = layout::hover::hover_writes(&tree, Some(layout::hit::LogicalPoint { x: 50.0, y: 10.0 }));
        assert_eq!(writes.len(), 1, "one node declared a hover, so there is one write");
        assert!(writes[0].hovered, "the pointer is inside the row that declared it");
        assert!(writes[0].rect.is_some(), "and it reports where it is, for a tooltip to anchor to");

        for write in &writes {
            write
                .signal
                .hover_handle()
                .expect("a config-authored hover is writable by the engine")
                .set_changed(mlua::Value::Boolean(write.hovered));
        }
        assert!(client.re_resolve_if_dirty(), "a hover that changed has to re-resolve the scene");
        assert!(hover_row(&client).children[0].visible, "the node bound to the hover is showing now");

        // And back off, which is the edge a callback-shaped design drops when a re-resolve replaces
        // the node between the two events (ADR-0062 decision 1).
        let tree = client.scene.surface("bar@TEST").unwrap();
        for write in layout::hover::hover_writes(&tree, None) {
            write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(write.hovered));
        }
        assert!(client.re_resolve_if_dirty());
        assert!(!hover_row(&client).children[0].visible, "the pointer left, so it is hidden again");
    }

    #[test]
    fn the_shipped_dev_config_evaluates_and_declares_every_surface_it_ships() {
        // Against `dev-config/oblisk/shell.lua` itself, not a fixture. That config is this repo's
        // worked example and its live-session fixture, and it is split across thirty-odd files
        // that reach each other through `require`, so a rename or a moved module breaks it in a
        // way no unit test over `components/` can see. Evaluated exactly as `run_startup_
        // evaluation` would: capabilities seeded and reading nil, which is also the state a real
        // boot evaluates in before the first snapshot lands.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (client, _outbound_rx) = test_client(&shell_lua);

        let (_output, specs) = evaluate_and_specs(&client.loader, &shell_lua)
            .unwrap_or_else(|err| panic!("the shipped dev config must evaluate: {err}"));

        // By id and role rather than by count, so this says which surface went missing.
        let declared: Vec<(&str, &str)> = specs
            .iter()
            .map(|spec| {
                let role = match spec {
                    SurfaceSpec::Panel(_) => "panel",
                    SurfaceSpec::Window(_) => "window",
                    SurfaceSpec::Popup(_) => "popup",
                    SurfaceSpec::Lock(_) => "lock",
                };
                (spec.declared_id(), role)
            })
            .collect();
        assert_eq!(
            declared,
            vec![
                ("wallpaper", "panel"),
                ("bar", "panel"),
                ("notification_area", "panel"),
                ("osd", "panel"),
                ("settings", "window"),
                ("panel_host", "panel"),
                ("battery_tooltip", "popup"),
                ("clock_tooltip", "popup"),
                ("launcher_tooltip", "popup"),
                ("network_tooltip", "popup"),
                ("bluetooth_tooltip", "popup"),
                ("launcher", "panel"),
                ("lock_screen", "lock"),
                ("polkit_dialog", "panel"),
            ]
        );
    }

    /// The absolute centre of every node in `node` declaring a `hover` property, accumulating the
    /// parent-relative origins on the way down the way `layout::hit` does.
    fn hover_region_centres(
        node: &crate::layout::ResolvedNode,
        x: f32,
        y: f32,
        out: &mut Vec<layout::hit::LogicalPoint>,
    ) {
        let (x, y) = (x + node.rect.x, y + node.rect.y);
        if node.properties.contains_key("hover") {
            out.push(layout::hit::LogicalPoint { x: x + node.rect.width / 2.0, y: y + node.rect.height / 2.0 });
        }
        for child in &node.children {
            hover_region_centres(child, x, y, out);
        }
    }

    #[test]
    fn every_hover_region_the_shipped_bar_declares_lights_exactly_one_slot() {
        // The wiring check the per-module tooltips need and no unit test reaches: each region names
        // a slot by string, and a typo is a region that lights nothing and a tooltip that never
        // opens. Silent in every other gate, because both halves parse and both resolve.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        for (capability, payload) in widest_bar_snapshots() {
            client
                .apply_state_snapshot(StateSnapshot { capability: capability.to_string(), revision: 1, payload })
                .unwrap();
        }
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let bar = client.scene.surface("bar@TEST").unwrap();
        let mut centres = Vec::new();
        hover_region_centres(&bar, 0.0, 0.0, &mut centres);
        assert!(centres.len() >= 2, "the bar declares more than one hover region, got {}", centres.len());

        for centre in &centres {
            let writes = layout::hover::hover_writes(&bar, Some(*centre));
            let lit: Vec<bool> = writes.iter().map(|write| write.hovered).collect();
            assert_eq!(
                lit.iter().filter(|hovered| **hovered).count(),
                1,
                "a point inside one region must light that region and no other, at {centre:?} got {lit:?}"
            );
        }

        // Distinct slots, not one signal shared by every region. A slot name copy-pasted between
        // two modules reads as a tooltip opening over the wrong one live, and passes every other
        // check here: both regions parse, both resolve, and each lights exactly one *write*.
        let writes = layout::hover::hover_writes(&bar, Some(centres[0]));
        let first = writes.first().expect("the walk found regions above");
        first.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(true));
        let lit_after: Vec<bool> = writes
            .iter()
            .map(|write| write.signal.get_value(client.lua()).unwrap() == mlua::Value::Boolean(true))
            .collect();
        assert_eq!(
            lit_after.iter().filter(|lit| **lit).count(),
            1,
            "writing one region's signal must light one region, got {lit_after:?}"
        );
    }

    #[test]
    fn the_shipped_dev_configs_battery_tooltip_opens_when_its_pill_is_hovered() {
        // ADR-0062 against the config this repo ships, which is the only place the whole chain
        // exists at once: `hover(name)` in `battery.lua`, the `hover` property on the pill,
        // `hover_rect(name)` on the tooltip's `anchor_rect`, and the popup's `visible`.
        //
        // Found by walking for the `hover` property rather than by indexing into the left zone, so
        // reordering the bar does not silently turn this into a test of the wrong module.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        for (capability, payload) in widest_bar_snapshots() {
            client
                .apply_state_snapshot(StateSnapshot { capability: capability.to_string(), revision: 1, payload })
                .unwrap();
        }
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let tooltip_is_up =
            |client: &RendererClient| client.scene.surface("battery_tooltip").expect("the tooltip resolves").visible;
        assert!(!tooltip_is_up(&client), "a tooltip is not up before the pointer has been anywhere");

        // Both of this slice's live bugs were in the state *before* anything is hovered, which the
        // rest of this test walks straight past, so they are pinned here.
        //
        // `grab` is § 6.3's default `true` unless a popup says otherwise, and a grabbing popup
        // needs an armed input serial that a hover cannot produce -- the tooltip resolved
        // `visible = true` on a real pointer and the compositor refused it on every re-resolve.
        // `anchor_rect` is required and non-zero, and the rect signal started nil, which reads as
        // the property being absent rather than as a rect.
        let (_output, specs) = evaluate_and_specs(&client.loader, &shell_lua).expect("the shipped config evaluates");
        let tooltip = specs
            .iter()
            .find_map(|spec| match spec {
                SurfaceSpec::Popup(popup) if popup.id == "battery_tooltip" => Some(popup),
                _ => None,
            })
            .expect("the shipped config declares a battery tooltip");
        assert!(!tooltip.grab, "a hover-opened popup must not ask for a grab; there is no click to arm its serial");
        assert!(
            tooltip.anchor_rect.width > 0.0 && tooltip.anchor_rect.height > 0.0,
            "anchor_rect has to be a real non-zero rect before anything has been hovered, got {:?}",
            tooltip.anchor_rect
        );

        // Every region on the bar is tried, not just the first one the walk finds. `HoverWrite`
        // carries the signal and no name, so there is no way to ask for the battery's region by
        // slot; the first region used to be the battery's only because nothing to its left declared
        // one. It now sits behind four circles that do, and `centres.first()` quietly turned this
        // into a test of the power button.
        //
        // Trying all of them tests the stronger claim anyway: exactly one region on this bar opens
        // the battery tooltip, so a slot renamed on either side fails here rather than passing with
        // the wrong module hovered.
        let bar = client.scene.surface("bar@TEST").unwrap();
        let mut centres = Vec::new();
        hover_region_centres(&bar, 0.0, 0.0, &mut centres);
        assert!(centres.len() > 1, "the shipped bar declares more than one hover region");

        let mut opened_by = 0;
        for centre in centres {
            let writes = layout::hover::hover_writes(&bar, Some(centre));
            // A point inside one region is outside the rest. Every write is applied the way
            // `App::sync_hover` applies them, because turning the others *off* is half of what the
            // walk is for.
            assert_eq!(writes.iter().filter(|write| write.hovered).count(), 1, "a point is inside exactly one region");
            for write in writes {
                write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(write.hovered));
                let Some(rect) = write.rect else {
                    continue;
                };
                // Built here rather than reached for through `crate::wayland::input`, which is
                // private: the shape is § 6.3's `anchor_rect`, and a wrong one fails the re-resolve
                // asserted just below rather than passing quietly.
                let table = client.lua().create_table().unwrap();
                table.set("x", rect.x).unwrap();
                table.set("y", rect.y).unwrap();
                table.set("width", rect.width).unwrap();
                table.set("height", rect.height).unwrap();
                write.signal.hover_rect_handle().unwrap().set_changed(mlua::Value::Table(table));
            }
            client.re_resolve_if_dirty();
            if tooltip_is_up(&client) {
                opened_by += 1;
            }
        }
        assert_eq!(opened_by, 1, "exactly one hover region on the bar opens the battery tooltip");

        // And closes again. `anchor_rect` keeps the rect it was last given rather than clearing,
        // which is what stops § 6.3's non-zero rule failing the evaluation on the way out.
        let bar = client.scene.surface("bar@TEST").unwrap();
        for write in layout::hover::hover_writes(&bar, None) {
            write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(false));
        }
        assert!(client.re_resolve_if_dirty());
        assert!(!tooltip_is_up(&client), "the pointer left the bar, so the tooltip closed");
    }

    #[test]
    fn the_shipped_dev_configs_notification_card_is_up_only_while_something_is_in_the_feed() {
        // The bug this is here for was on screen for the whole slice: the surface had no `visible`
        // binding at all, so a card reading "no notifications" sat in the corner permanently. An
        // empty state is what a *panel* shows; a popup that is always up is not a notification.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let card_is_up = |client: &RendererClient| {
            client.scene.surface("notification_area@TEST").expect("the card resolves").visible
        };
        assert!(!card_is_up(&client), "nothing has been received, so there is nothing to show");

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "notifications".to_string(),
                revision: 1,
                payload: serde_json::json!({ "feed": [{ "id": 7, "app_name": "Zen", "summary": "a thing happened" }] }),
            })
            .unwrap();
        assert!(client.re_resolve_if_dirty());
        assert!(card_is_up(&client), "a notification in the feed puts the card up");

        // And back down when the Supervisor expires it out of the feed (ADR-0033), which is
        // the whole of this config's auto-hide: no timer here, just an empty list.
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "notifications".to_string(),
                revision: 2,
                payload: serde_json::json!({ "feed": [] }),
            })
            .unwrap();
        assert!(client.re_resolve_if_dirty());
        assert!(!card_is_up(&client), "an expired feed takes the card with it");
    }

    #[test]
    fn the_shipped_dev_configs_history_card_is_as_tall_as_the_notifications_in_it() {
        // The last notification in the history was drawn with its bottom edge cut off by the panel
        // card: the card's content-sized height (ADR-0110) came out one line of body text short.
        // The trigger was position, not content -- taffy 0.14 adds a container's own margin to its
        // children's minimum cross size while measuring them (`layout::scene::taffy_style` says
        // how this engine sidesteps that), and the card's margin is what centres it under its
        // indicator, most of the way across the output. So this places the anchor where the bell
        // is and seeds the output the session ran on, and asks that every node in the card holds
        // its children: the card its body, the body its list, the list its cards, a card its rows.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "system".to_string(),
                revision: 1,
                payload: serde_json::json!({ "time": 1_700_000_000 }),
            })
            .unwrap();
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "notifications".to_string(),
                revision: 1,
                payload: serde_json::json!({ "feed": [
                    { "id": 7, "app_name": "notify-send", "summary": "backup finished", "timestamp": 1_699_999_000,
                      "body": [{ "kind": "text", "text": "Oblisk · 1 · Backup" }] },
                    { "id": 8, "app_name": "Telegram Desktop", "summary": "Anas", "timestamp": 1_699_998_000,
                      "body": [{ "kind": "text", "text": "have a look at this: https://github.com/anasgets111/oblisk-shell/pull/12 and tell me what you think." }] }
                ] }),
            })
            .unwrap();
        client.set_screens(
            serde_json::json!([{ "name": "TEST", "width": 1920, "height": 1200, "scale": 1.0, "refresh": 60000 }]),
        );
        let specs = client.run_startup_evaluation().expect("the shipped dev config must evaluate");
        let outputs = vec![OutputGeometry {
            name: "TEST".to_string(),
            size: layout::LogicalSize { width: 1920.0, height: 1200.0 },
        }];
        client.set_instances(expand_instances(&specs, &outputs));
        assert!(client.apply_instances(), "the shipped dev config must resolve into a scene");
        client
            .lua()
            .load(
                r#"state("popup_anchor"):set({ x = 1745, y = 0, width = 24, height = 24 })
                   state("panel_kind"):set("notifications")
                   state("panel_open"):set(true)"#,
            )
            .exec()
            .unwrap();
        assert!(client.re_resolve_if_dirty());

        fn overflowing(node: &crate::layout::ResolvedNode, path: &str, out: &mut Vec<String>) {
            for (index, child) in node.children.iter().filter(|child| child.visible).enumerate() {
                let here = format!("{path}/{}[{index}]", child.kind);
                let bottom = child.rect.y + child.rect.height;
                if bottom > node.rect.height + 0.5 {
                    out.push(format!("{here} ends {bottom:.1}px down a {:.1}px {}", node.rect.height, node.kind));
                }
                overflowing(child, &here, out);
            }
        }
        let host = client.scene.surface("panel_host@TEST").expect("the panel host must resolve");
        let card = &host.children[0].children[1];
        assert!(card.visible, "the notification history is open");
        let mut out = Vec::new();
        overflowing(card, "card", &mut out);
        assert!(out.is_empty(), "a node in the history card is taller than what holds it:\n{}", out.join("\n"));
    }

    #[test]
    fn the_shipped_dev_configs_lock_screen_draws_one_mask_glyph_per_typed_character() {
        // The lock screen is the one surface where drawing nothing is a lockout risk rather than a
        // cosmetic gap: typing blind makes a typo invisible, `pam_unix` answers a wrong password
        // with a two second delay, and `pam_faillock` locks the account after three. Asserted
        // against the shipped `lock.lua` rather than a fixture, because what has to hold is that
        // *this* config's field is the one that fills.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let lock = client.scene.surface("lock_screen@TEST").expect("the lock screen resolves");
        let masked = |focus: Option<&layout::paint::FieldFocus>| -> Vec<String> {
            layout::paint::build(&lock, 1.0, focus)
                .commands
                .iter()
                .filter_map(|command| match &command.draw {
                    layout::paint::Draw::Text { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        };

        let unfocused = masked(None);
        assert!(
            unfocused.iter().any(|drawn| drawn == "password"),
            "an untouched field shows its placeholder: {unfocused:?}"
        );

        // The pair `lock.lua` declares, and the pair the Supervisor's unlock path answers.
        let target =
            layout::node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() };
        let typed = masked(Some(&layout::paint::FieldFocus::Masked { target: &target, filled: 5 }));
        assert!(
            typed.iter().any(|drawn| drawn == "*****"),
            "five keystrokes must draw five of this config's `mask_character`: {typed:?}"
        );
        assert!(
            !typed.iter().any(|drawn| drawn == "password"),
            "the placeholder gives way once something is typed: {typed:?}"
        );
    }

    #[test]
    fn the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing() {
        // The failure this catches has happened twice and is invisible until a screenshot: a zone
        // is a fixed percentage of the bar and `row` does not shrink a child to make its siblings
        // fit, so a zone one module too full silently paints the last one past its own right edge
        // and off the bar. Resolved against a 1920x1080 output, which is what `test_outputs` gives.
        //
        // The zones are a fixed percentage by the config's choice now, not by the engine's limit. A
        // `Fill` child is sized from the remainder its siblings leave, so two `Fill` spacers around
        // a content-sized centre zone would centre it at any module width and retire this whole
        // failure mode. `dev-config/oblisk/modules/bar/init.lua` says why that rewrite is not done
        // yet. Until it is, this test is what stands between a full zone and a clipped module.
        //
        // A loaded bar, not the boot one. Left to itself every capability reads nil and each
        // module draws its shortest placeholder, so an empty bar would pass this and a real
        // session would still clip. The snapshots below are each module at or near its widest:
        // every string that gets truncated is fed past its truncation limit, the tray carries
        // items, and the privacy pill -- normally hidden -- is forced visible with a camera user.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        for (capability, payload) in widest_bar_snapshots() {
            client
                .apply_state_snapshot(StateSnapshot { capability: capability.to_string(), revision: 1, payload })
                .unwrap_or_else(|err| panic!("{capability} snapshot must apply: {err}"));
        }
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let bar = client.scene.surface("bar@TEST").expect("the bar instance must resolve");
        // bar -> the padded row -> the three zones.
        let zones = &bar.children[0].children;
        assert_eq!(zones.len(), 3, "the bar is three zones (`modules/bar/init.lua`)");

        // Every zone reported before any assertion, so one overflow does not hide the next.
        let overflowing: Vec<String> = zones
            .iter()
            .enumerate()
            .filter_map(|(index, zone)| {
                let shown: Vec<&crate::layout::ResolvedNode> =
                    zone.children.iter().filter(|child| child.visible).collect();
                let content: f32 = shown.iter().map(|child| child.rect.width).sum::<f32>()
                    + 6.0 * shown.len().saturating_sub(1) as f32;
                let widths: Vec<String> = shown.iter().map(|child| format!("{:.0}", child.rect.width)).collect();
                (content > zone.rect.width).then(|| {
                    format!(
                        "zone {index}: {:.0}px of modules ({}) in {:.0}px",
                        content,
                        widths.join("+"),
                        zone.rect.width
                    )
                })
            })
            .collect();
        assert!(overflowing.is_empty(), "a bar zone will paint past its own edge -- {}", overflowing.join("; "));

        // The panel host has the other axis to worry about. Its card is as tall as the panel in it
        // (ADR-0110): each body's list is capped and scrolls, so what can still overflow is the
        // card as a whole running off the bottom of the output, and the surface's own height is the
        // room under the bar.
        //
        // `children[0]` is the surface's one root node, holding the click-outside catcher and the
        // card in that order (`modules/shell/panel_host.lua`); the card is the second so that it
        // paints, and hit-tests, over the catcher.
        let host = client.scene.surface("panel_host@TEST").expect("the panel host must resolve");
        let card = &host.children[0].children[1];
        let card_bottom = card.rect.y + card.rect.height;
        assert!(
            card_bottom <= host.rect.height,
            "the panel card ends {card_bottom:.0}px down a {:.0}px surface; it runs off the output",
            host.rect.height
        );
        let widest = card.children.iter().map(|section| section.rect.width).fold(0.0_f32, f32::max);
        // Derived the same way and for the same reason: the hard-coded 24 assumed `spacing.md` was
        // 12px, and it is 11px once the responsive scale has been through it, so this compared a
        // `Fill` section against a card two pixels narrower than the one it was filling.
        let content_width = card.rect.width - 2.0 * card.children.first().map_or(0.0, |first| first.rect.x);
        assert!(
            widest <= content_width,
            "a bar panel is {widest:.0}px in {content_width:.0}px of card; it will paint past the edge"
        );
    }

    #[test]
    fn every_rostered_capability_is_on_the_oblisk_table_and_reads_nil_before_its_first_snapshot() {
        // ADR-0037's uniform contract: a shell.lua reading any rostered capability at boot gets a
        // live signal reading nil, never an index-into-nil error into rescue, under the name
        // `oblisk.<roster name>`.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::Capability::ALL.iter().map(|c| c.as_str()) {
            let setup = format!("is_nil = oblisk.{capability}:get() == nil");
            assert!(
                probe::<bool>(&client.loader, &setup, "is_nil"),
                "oblisk.{capability} should read nil before its first snapshot"
            );
        }
    }

    #[test]
    fn no_rostered_capability_is_left_as_a_bare_global() {
        // `set_global` never removes anything, so a leftover bare seed would keep working until
        // the day the name collided with a node constructor the way `lock` did (ADR-0052
        // decision 1).
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::Capability::ALL.iter().map(|c| c.as_str()) {
            // `lock` is excluded: § 6.4's node constructor legitimately owns that global.
            if capability == "lock" {
                continue;
            }
            let setup = format!("is_nil = {capability} == nil");
            assert!(
                probe::<bool>(&client.loader, &setup, "is_nil"),
                "{capability} is still a bare global; § 2 names it oblisk.{capability}"
            );
        }
    }

    #[test]
    fn an_unrostered_push_refuses_to_replace_a_name_the_oblisk_table_already_holds() {
        // `rescue` reports config failures, so replacing it with an empty capability would make
        // the shell stop reporting its own breakage -- how ADR-0052 decision 1's `lock` bug
        // happened.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let snapshot = StateSnapshot { capability: "rescue".to_string(), revision: 1, payload: serde_json::json!({}) };
        let err = client.apply_state_snapshot(snapshot).unwrap_err().to_string();
        assert!(err.contains("already something else"), "the refusal must say why: {err}");

        // The real `rescue` still reads its own table, not an empty capability.
        assert!(probe::<bool>(&client.loader, "intact = oblisk.rescue:get().is_rescue == false", "intact"));
    }

    #[test]
    fn rescue_and_screens_moved_onto_the_same_table_as_the_roster() {
        // § 2.10 and § 2.15 name both of these `oblisk.*` like every capability.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let setup = r#"
            rescued = oblisk.rescue:get().is_rescue
            screen_count = #oblisk.screens:get()
            bare_rescue_gone = rescue == nil
            bare_screens_gone = screens == nil
        "#;
        assert!(!probe::<bool>(&client.loader, setup, "rescued"));
        assert_eq!(probe::<i64>(&client.loader, setup, "screen_count"), 0);
        assert!(probe::<bool>(&client.loader, setup, "bare_rescue_gone"));
        assert!(probe::<bool>(&client.loader, setup, "bare_screens_gone"));
    }

    #[test]
    fn oblisk_version_is_three_integers_a_config_can_compare() {
        // The value matters less than the shape: a config guards with `if oblisk.version.major >
        // 0 or oblisk.version.minor >= 2 then`, so all three fields must be present and numeric.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let setup = "major, minor, patch = oblisk.version.major, oblisk.version.minor, oblisk.version.patch";
        // Read back through Lua and compared against Cargo's own string, rather than against a
        // second call to the function that built the table: this asserts the number a config
        // actually sees, not that one function agrees with itself. It also reaches
        // `lua::namespace::version_parts`'s `expect`, which is why that function carries no
        // narrower test of its own.
        let field = |name: &str| probe::<i64>(&client.loader, setup, name);
        assert_eq!(format!("{}.{}.{}", field("major"), field("minor"), field("patch")), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn oblisk_config_dir_is_the_directory_shell_lua_was_loaded_from() {
        // Derived from the loaded path rather than re-resolved, so a Renderer started with an
        // explicit `shell.lua` cannot report a directory it is not reading from.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/opt/oblisk-config/shell.lua"));
        let dir = probe::<String>(&client.loader, "dir = oblisk.config_dir", "dir");
        assert_eq!(dir, "/opt/oblisk-config");
    }

    /// `RendererClient::new` seeds the roster *after* `Loader::new` registered § 6.4's node
    /// constructors, so seeding `lock` as a bare global would overwrite the constructor -- and a
    /// `set` over an existing global is silent, so the failure would surface as "attempt to call
    /// a userdata value" from the config's own `lock { ... }` line, pointing at the config rather
    /// than the seed.
    ///
    /// Asserted after a whole generation is built, not after the seed alone, since the ordering
    /// between the two registrations is exactly what is under test.
    #[test]
    fn a_full_generation_keeps_lock_as_the_node_constructor_and_puts_the_capability_on_oblisk() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r##"return {
                panel { id = "bar", layer = "Top" },
                lock {
                    id = "screen",
                    child = text { content = oblisk.lock:map(function(s) return (s and s.error) or "" end) },
                },
            }"##,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "lock".to_string(),
                revision: 1,
                payload: serde_json::json!({ "active": false, "authenticating": false, "attempts": 2, "error": "authentication failed" }),
            })
            .unwrap();

        assert!(run_startup(&mut client), "a config declaring a lock screen must build a scene");

        // The constructor survived: a `lock { ... }` at the root still produced a § 6.4 surface.
        let setup = r#"
            lock_kind = lock { id = "screen" }.kind
            capability_type = type(oblisk.lock)
            attempts = oblisk.lock:get().attempts
        "#;
        assert_eq!(
            probe::<String>(&client.loader, setup, "lock_kind"),
            "lock",
            "the global `lock` must still be § 6.4's node constructor"
        );
        // The capability is reachable, hydrated, under the name § 2 gives it.
        assert_eq!(probe::<String>(&client.loader, setup, "capability_type"), "userdata");
        assert_eq!(
            probe::<i64>(&client.loader, setup, "attempts"),
            2,
            "the `lock` StateSnapshot must reach `oblisk.lock`, not a bare global nothing registered"
        );
    }

    /// The write half of the same object: a config's own `on_click` calling the lock action puts
    /// a real § 7.2 envelope on the outbound channel.
    #[test]
    fn a_config_calling_the_lock_action_queues_a_command_for_the_supervisor() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load(r#"oblisk.lock:invoke("lock")"#).exec().unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("a capability write must be queued as RendererFrame::Command");
        };
        assert_eq!(envelope.params.capability, "lock");
        assert_eq!(envelope.params.action, "lock", "the action `supervisor/src/lock.rs`'s dispatch answers to");
    }

    #[test]
    fn apply_state_snapshot_reuses_the_same_signal_across_repeated_pushes_for_one_capability() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "network".to_string(),
                revision: 1,
                payload: serde_json::json!({ "scanning": true }),
            })
            .unwrap();
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "network".to_string(),
                revision: 2,
                payload: serde_json::json!({ "scanning": false }),
            })
            .unwrap();

        assert!(
            !probe::<bool>(&client.loader, "scanning = oblisk.network:get().scanning", "scanning"),
            "the second push must update the same registered global, not fail or create a second one"
        );
    }

    #[test]
    fn run_startup_evaluation_applies_a_valid_file_and_clears_rescue() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        assert!(client.scene.surface("bar@TEST").is_some());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    /// A `window` and a `popup` declared alongside the panels.
    fn three_roles_config() -> &'static str {
        r#"return {
            panel { id = "bar", layer = "Top" },
            window { id = "settings", title = "Settings", min_size = { width = 320, height = 240 } },
            popup { id = "menu", parent = "bar", width = 200, height = 120,
                    anchor_rect = { x = 12, y = 32, width = 86, height = 24 } },
        }"#
    }

    #[test]
    fn every_declared_role_comes_back_from_the_startup_evaluation_tagged_with_its_own_spec() {
        // The whole roster, not just the panels: `expand_instances` and `create_surfaces` both
        // branch on the variant, so an untagged role could only ever be bound as a layer surface.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), three_roles_config());
        let (mut client, _outbound_rx) = test_client(&path);

        let specs = client.run_startup_evaluation().expect("all three roles must evaluate");

        assert!(matches!(specs.as_slice(), [SurfaceSpec::Panel(_), SurfaceSpec::Window(_), SurfaceSpec::Popup(_)]));
        assert_eq!(specs.iter().map(SurfaceSpec::declared_id).collect::<Vec<_>>(), ["bar", "settings", "menu"]);
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn the_swap_fingerprint_carries_every_role_so_adding_a_window_is_a_topology_change() {
        // ADR-0049 decision 3: a `window`'s Wayland object comes and goes inside one
        // generation, but its *declaration* is fixed for that generation's life, so adding one is
        // a topology change like any other.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(
            surface_specs(&client.loader.evaluate_file(&path).unwrap())
                .unwrap()
                .iter()
                .map(SurfaceSpec::fingerprint)
                .collect(),
        );

        write_shell_lua(dir.path(), three_roles_config());
        client.handle_reevaluate(ReevaluateRequest { sequence: 9 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 9 })
        );
        assert!(client.state.pending.is_none());
    }

    /// A lock screen whose `child` holds the one `secure_submit` field § 6.4 needs.
    fn lock_config(background: &str) -> String {
        format!(
            r##"return {{
                panel {{ id = "bar", layer = "Top" }},
                lock {{ id = "screen", child = column {{ background = "{background}", children = {{
                    textfield {{ mask_character = "*", secure_submit = {{ capability = "lock", action = "authenticate" }} }},
                }} }} }},
            }}"##
        )
    }

    #[test]
    fn an_in_place_reload_may_restyle_a_live_lock_screen_but_not_remove_its_way_out() {
        // `SurfaceFingerprint::Lock` carries only the `id`, so an edit *inside* the lock diffs as
        // `Unchanged` and takes the in-place path, which the generation-swap gate does not
        // police. Both halves are the point: the restyle has to keep landing (ADR-0052
        // decision 2), but the edit that removes the way out must be refused, and `Scene::apply`'s
        // rollback is what leaves the live tree exactly as it was.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        // A restyle: same surfaces, same field, different colour.
        write_shell_lua(dir.path(), &lock_config("#204080FF"));
        client.handle_reevaluate(ReevaluateRequest { sequence: 20 });
        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 20 })
        );
        client.handle_apply_pending(ApplyPendingReload { sequence: 20 });
        let restyled = client.scene.surface("screen@TEST").expect("the lock instance is still resolved");
        assert_eq!(
            restyled.children[0].properties.get("background").unwrap().as_string().unwrap().to_string_lossy(),
            "#204080FF",
            "restyling a live lock screen is the reason it is painted from Lua at all"
        );

        // The edit that must not land: same lock, no `textfield` under it.
        write_shell_lua(
            dir.path(),
            r##"return {
            panel { id = "bar", layer = "Top" },
            lock { id = "screen", child = column { background = "#204080FF", children = {} } },
        }"##,
        );
        client.handle_reevaluate(ReevaluateRequest { sequence: 21 });
        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 21 })
        );
        client.handle_apply_pending(ApplyPendingReload { sequence: 21 });

        let still_up = client.scene.surface("screen@TEST").expect("a refused apply leaves the prior scene standing");
        assert_eq!(
            still_up.children[0].children.len(),
            1,
            "the password field must still be there: a refused apply rolls the whole scene back to what was on screen"
        );
        assert_eq!(still_up.children[0].children[0].kind, "textfield");
    }

    #[test]
    fn an_unlocked_session_may_still_delete_its_lock_screens_password_field() {
        // With no lock held there is nothing to be locked out of, so a config may edit its lock
        // screen down to nothing like any other surface.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        write_shell_lua(
            dir.path(),
            r##"return {
            panel { id = "bar", layer = "Top" },
            lock { id = "screen", child = column { background = "#101010FF", children = {} } },
        }"##,
        );
        client.handle_reevaluate(ReevaluateRequest { sequence: 22 });
        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 22 })
        );
        client.handle_apply_pending(ApplyPendingReload { sequence: 22 });

        assert!(
            client.scene.surface("screen@TEST").unwrap().children[0].children.is_empty(),
            "an unlocked session's lock screen is ordinary"
        );
    }

    #[test]
    fn the_lock_veto_follows_the_outputs_rather_than_the_instances_it_was_armed_with() {
        // The veto used to be armed with the `lock` instance ids that existed at
        // `LockCommand::Acquire`. `handle_output_change` retires instances and creates new ones
        // on every hotplug without revisiting that list, so a lid closing onto a dock left the
        // veto validating a fossil nothing can paint while the live lock screen went unguarded.
        //
        // The apply below is the same sequence `handle_output_change` performs: re-expand the
        // applied specs against the outputs that exist now, store them, resolve.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        let specs = client.applied_surface_specs();
        let docked = vec![OutputGeometry {
            name: "DP-1".to_string(),
            size: layout::LogicalSize { width: 2560.0, height: 1440.0 },
        }];
        client.set_instances(expand_instances(&specs, &docked));
        assert!(client.apply_instances(), "the freshly plugged output resolves its own lock surface");

        write_shell_lua(
            dir.path(),
            r##"return {
            panel { id = "bar", layer = "Top" },
            lock { id = "screen", child = column { background = "#101010FF", children = {} } },
        }"##,
        );
        client.handle_reevaluate(ReevaluateRequest { sequence: 30 });
        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 30 })
        );
        client.handle_apply_pending(ApplyPendingReload { sequence: 30 });

        let live =
            client.scene.surface("screen@DP-1").expect("the lock instance on the output that is actually plugged in");
        assert_eq!(
            live.children[0].children.len(),
            1,
            "the veto must ask what is on the glass now, not what was there when the lock was taken"
        );
    }

    #[test]
    fn a_second_lock_declaration_is_refused_at_evaluation_naming_6_4() {
        // `expand_instances` emits one instance per lock spec per output, so two `lock`
        // declarations make `ensure_lock_surfaces` send two `get_lock_surface` for the same
        // `wl_output`, which `ext-session-lock-v1` calls a `duplicate_output` protocol error. The
        // compositor kills the connection *after* the lock is taken, so the only way back in is a
        // VT switch.
        //
        // Both entry points, since both reach a generation: startup refuses to hand any specs
        // back, and a `Reevaluate` reports `Failed` rather than staging it.
        let dir = tempfile::tempdir().unwrap();
        let two_locks = r#"return { lock { id = "first" }, lock { id = "second" } }"#;
        let path = write_shell_lua(dir.path(), two_locks);
        let (mut client, mut outbound_rx) = test_client(&path);

        assert!(!run_startup(&mut client), "a config with two `lock` surfaces must not produce a generation");
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue, "the refusal has to be visible somewhere, and rescue is where an evaluation failure goes");
        assert!(
            error_log.contains("§ 6.4"),
            "the message must name the section that says one lock surface per output: {error_log}"
        );

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        client.state.applied_topology = Some(
            surface_specs(&client.loader.evaluate_file(&path).unwrap())
                .unwrap()
                .iter()
                .map(SurfaceSpec::fingerprint)
                .collect(),
        );
        write_shell_lua(dir.path(), two_locks);
        client.handle_reevaluate(ReevaluateRequest { sequence: 11 });

        assert!(
            matches!(
                queued_frame(&mut outbound_rx),
                RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence: 11, .. })
            ),
            "an edit that adds a second lock must fail the reevaluation rather than be staged"
        );
        assert!(client.state.pending.is_none());
    }

    #[test]
    fn a_windows_title_is_an_in_place_field_rather_than_a_topology_one() {
        // `xdg-shell.xml` says a `set_title`/`set_app_id` request may be sent after the toplevel
        // is mapped, so a changed title must not respawn the process to deliver it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return window { id = "settings", title = "Settings" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(
            surface_specs(&client.loader.evaluate_file(&path).unwrap())
                .unwrap()
                .iter()
                .map(SurfaceSpec::fingerprint)
                .collect(),
        );

        write_shell_lua(
            dir.path(),
            r#"return window { id = "settings", title = "Oblisk settings", app_id = "oblisk.settings" }"#,
        );
        client.handle_reevaluate(ReevaluateRequest { sequence: 10 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 10 })
        );
        assert!(client.state.pending.is_some());
    }

    #[test]
    fn a_popup_with_a_zero_anchor_rect_fails_the_evaluation_and_names_the_property_in_rescue() {
        // § 6.3's `anchor_rect` feeds `xdg_positioner::set_anchor_rect`, and a zero size leaves
        // the positioner incomplete, raising `invalid_positioner` at `get_popup` and killing the
        // whole Wayland connection. A config typo must be a `LayoutError` at evaluation, never a
        // protocol error at runtime, landing in `rescue`'s `error_log` with the property named.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return {
                panel { id = "bar", layer = "Top" },
                popup { id = "menu", parent = "bar", width = 200, height = 120,
                        anchor_rect = { x = 0, y = 0, width = 0, height = 0 } },
            }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(client.run_startup_evaluation().is_none(), "a malformed popup must fail the whole evaluation");

        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(
            error_log.contains("anchor_rect"),
            "the human reading rescue's error_log needs the property named: {error_log}"
        );
    }

    #[test]
    fn a_window_whose_max_size_is_below_its_min_size_fails_the_evaluation() {
        // The `window` half of the same rule: `set_max_size` raises `invalid_size` on a maximum
        // under the minimum, so the check has to run at evaluation, where a config author sees it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return window { id = "settings", min_size = { width = 800, height = 600 }, max_size = { width = 320, height = 240 } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(client.run_startup_evaluation().is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(error_log.contains("max_size"), "{error_log}");
    }

    #[test]
    fn run_startup_evaluation_on_a_missing_file_sets_rescue_and_leaves_scene_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let (mut client, _outbound_rx) = test_client(&missing);

        run_startup(&mut client);

        assert!(client.state.applied_topology.is_none());
        assert!(client.scene.surface("bar@TEST").is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(!error_log.is_empty());
    }

    #[test]
    fn a_successful_reevaluate_after_a_startup_failure_recovers_instead_of_reporting_topology_changed_forever() {
        // Treating "nothing applied yet" as an empty topology, rather than "no prior state to
        // protect", made every subsequent evaluation permanently misclassify as `TopologyChanged`,
        // which nothing here ever applies, leaving the shell blank forever.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let (mut client, mut outbound_rx) = test_client(&missing);
        run_startup(&mut client);
        assert!(client.state.applied_topology.is_none(), "startup must have failed (no file yet)");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 1 }),
            "the first successful evaluation after a startup failure must be treated as safe to apply, not a topology change"
        );
        assert!(client.state.pending.is_some());
    }

    #[test]
    fn handle_reevaluate_reports_unchanged_and_stores_pending_when_topology_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(
            surface_specs(&client.loader.evaluate_file(&path).unwrap())
                .unwrap()
                .iter()
                .map(SurfaceSpec::fingerprint)
                .collect(),
        );

        client.handle_reevaluate(ReevaluateRequest { sequence: 5 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 5 })
        );
        assert!(matches!(&client.state.pending, Some((sequence, _, _)) if *sequence == 5));
    }

    #[test]
    fn handle_reevaluate_reports_topology_changed_and_does_not_store_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        // Seed a *different* applied topology so the fresh evaluation reads as changed.
        client.state.applied_topology = Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology {
            id: "other".to_string(),
            layer: LayerKind::Top,
            anchor: Default::default(),
            monitor: "All".to_string(),
            namespace: "oblisk-other".to_string(),
        })]);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 })
        );
        assert!(client.state.pending.is_none(), "a topology-changed generation must not stage a pending apply");
    }

    #[test]
    fn editing_only_the_in_place_panel_fields_reports_unchanged_and_reloads_in_place() {
        // ADR-0038 decision 2: `margin`, exclusive zone, `keyboard_interactivity` and size
        // are all requests layer-shell accepts on a live surface, so editing one must reload in
        // place -- comparing whole specs would turn every one of these into a generation swap,
        // respawning the process to nudge a bar 4px sideways.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", margin = { left = 4 }, keyboard_interactivity = "None", exclusive = false, height = 32 }"#,
        );
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(
            surface_specs(&client.loader.evaluate_file(&path).unwrap())
                .unwrap()
                .iter()
                .map(SurfaceSpec::fingerprint)
                .collect(),
        );

        // Every in-place field changed at once; every topology field left alone.
        write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", margin = { left = 40 }, keyboard_interactivity = "Exclusive", exclusive = true, height = 48 }"#,
        );
        client.handle_reevaluate(ReevaluateRequest { sequence: 7 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 7 }),
            "margin/keyboard_interactivity/exclusive/size are in-place fields and must not trigger a generation swap"
        );
        assert!(
            client.state.pending.is_some(),
            "an Unchanged verdict must stage the fresh evaluation for ApplyPendingReload"
        );
    }

    #[test]
    fn editing_only_the_namespace_reports_topology_changed() {
        // `get_layer_surface` fixes the namespace at creation and no request changes it on a live
        // surface, so a namespace edit needs a new surface, hence a new generation.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(
            surface_specs(&client.loader.evaluate_file(&path).unwrap())
                .unwrap()
                .iter()
                .map(SurfaceSpec::fingerprint)
                .collect(),
        );

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", namespace = "my-bar" }"#);
        client.handle_reevaluate(ReevaluateRequest { sequence: 8 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 8 })
        );
        assert!(client.state.pending.is_none());
    }

    #[test]
    fn set_instance_size_replaces_one_instances_available_size_and_marks_the_scene_dirty() {
        // A `configure` reuses ADR-0044 decision 2's one dirty flag rather than adding a second
        // "something changed" mechanism next to it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "startup must have applied");
        assert!(!client.dirty.take(), "a clean startup leaves the flag clear");
        assert_eq!(
            client.scene.surface("bar@TEST").unwrap().rect.height,
            1080.0,
            "the first resolve uses the output's own size"
        );

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 1920.0, height: 32.0 });

        assert!(client.re_resolve_if_dirty(), "a new configured size must mark the scene dirty and re-resolve");
        assert_eq!(
            client.scene.surface("bar@TEST").unwrap().rect.height,
            32.0,
            "the surface must now resolve against the size the compositor actually configured, not the whole output"
        );
    }

    #[test]
    fn set_instance_size_repeating_the_current_size_does_not_mark_the_scene_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 1920.0, height: 1080.0 });
        assert!(
            !client.dirty.take(),
            "a duplicate configure carrying the size already resolved against changes nothing"
        );

        client.set_instance_size("no-such-surface@TEST", layout::LogicalSize { width: 10.0, height: 10.0 });
        assert!(!client.dirty.take(), "a configure for a surface no instance names must not dirty the whole scene");
    }

    #[test]
    fn one_surface_on_two_outputs_resolves_two_trees_against_two_different_sizes() {
        // `monitor = "All"` across a laptop panel and a 4K external is two configured sizes, and
        // one tree per declared surface could only ever serve one of them.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        let specs = client.run_startup_evaluation().expect("the fixture evaluates");
        let outputs = vec![
            OutputGeometry { name: "eDP-1".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } },
            OutputGeometry { name: "DP-1".to_string(), size: layout::LogicalSize { width: 3840.0, height: 2160.0 } },
        ];
        client.set_instances(expand_instances(&specs, &outputs));
        assert!(client.apply_instances());

        assert_eq!(client.scene.surface("bar@eDP-1").unwrap().rect.width, 1920.0);
        assert_eq!(client.scene.surface("bar@DP-1").unwrap().rect.width, 3840.0);
    }

    #[test]
    fn a_panel_naming_an_unplugged_monitor_gets_no_instance_and_no_resolved_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", monitor = "HDMI-A-9" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(
            run_startup(&mut client),
            "an unmatched monitor is not an apply failure -- there is simply nothing to resolve"
        );
        assert!(client.scene.surface("bar@TEST").is_none());
        assert!(client.instances.is_empty());
    }

    #[test]
    fn handle_reevaluate_reports_failed_and_sets_rescue_on_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), "this is not lua");
        let (mut client, mut outbound_rx) = test_client(&path);

        client.handle_reevaluate(ReevaluateRequest { sequence: 2 });

        match queued_frame(&mut outbound_rx) {
            RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                assert_eq!(sequence, 2);
                assert!(!error.is_empty());
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(client.state.pending.is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(!error_log.is_empty());
    }

    #[test]
    fn handle_reevaluate_reports_a_topology_field_error_distinctly_from_a_top_level_return_error() {
        // A topology-field type error (e.g. `anchor.top` not a boolean) used to be folded into
        // `InvalidTopLevelReturn`'s fixed "must be a `panel` node or an array of them" message.
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", anchor = { top = "yes" } }"#);
        let (mut client, mut outbound_rx) = test_client(&path);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        match queued_frame(&mut outbound_rx) {
            RendererFrame::ReevaluateReport(ReevaluateReport::Failed { error, .. }) => {
                assert!(error.contains("topology"), "expected a topology-specific message, got: {error}");
                assert!(
                    !error.contains("top-level return"),
                    "must not reuse the unrelated top-level-return message, got: {error}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn handle_apply_pending_reconciles_the_pending_evaluation_into_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let (output, specs) = evaluate_and_specs(&client.loader, &path).unwrap();
        client.set_instances(instances_for(&["bar"]));
        client.state.pending = Some((3, output, specs.iter().map(SurfaceSpec::fingerprint).collect()));

        client.handle_apply_pending(ApplyPendingReload { sequence: 3 });

        assert!(client.scene.surface("bar@TEST").is_some());
        assert!(client.state.pending.is_none());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
        // The half that reaches the screen: the poll loop only repaints when `re_resolve_if_dirty`
        // reports a change.
        assert!(client.dirty.take(), "an applied in-place reload must mark the scene dirty");
    }

    #[test]
    fn handle_apply_pending_ignores_a_mismatched_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let (output, specs) = evaluate_and_specs(&client.loader, &path).unwrap();
        client.set_instances(instances_for(&["bar"]));
        client.state.pending = Some((3, output, specs.iter().map(SurfaceSpec::fingerprint).collect()));

        client.handle_apply_pending(ApplyPendingReload { sequence: 99 });

        assert!(client.scene.surface("bar@TEST").is_none(), "a stale ApplyPendingReload must not apply");
        assert!(client.state.pending.is_some(), "the still-current pending evaluation must survive a mismatched Apply");
    }

    // ADR-0044 decision 2: a `StateSnapshot` push marks the scene dirty, and a dirty scene
    // re-resolves against the last applied evaluation without running shell.lua again.
    // `workspace` is used as the pushed capability throughout because it isn't in
    // `shared::Capability::ALL`, so pushing it before `run_startup_evaluation` is what makes a
    // `shell.lua` that references it bare (not `:get()`) evaluate at all.

    #[test]
    fn apply_state_snapshot_marks_the_scene_dirty() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        assert!(!client.dirty.take(), "a fresh client must not start dirty");

        let snapshot = StateSnapshot {
            capability: "audio".to_string(),
            revision: 1,
            payload: serde_json::json!({ "app_name": "Zen" }),
        };
        client.apply_state_snapshot(snapshot).unwrap();

        assert!(client.dirty.take(), "LiveSignalHandle::set must mark the shared scene-dirty flag");
    }

    #[test]
    fn re_resolve_if_dirty_applies_a_pushed_value_without_reading_shell_lua_again() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = oblisk.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 1,
                payload: serde_json::json!(true),
            })
            .unwrap();
        run_startup(&mut client);
        assert!(
            client.scene.surface("bar@TEST").unwrap().visible,
            "startup must have applied the pushed initial value"
        );

        // Break the file so a real re-evaluation would fail: the re-resolve below must read the
        // pushed value off the retained tree's live signal, never touching this file again.
        std::fs::write(&path, "this is not lua").unwrap();

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 2,
                payload: serde_json::json!(false),
            })
            .unwrap();
        client.re_resolve_if_dirty();

        assert!(!client.scene.surface("bar@TEST").unwrap().visible, "the re-resolve must reflect the pushed value");
        assert_eq!(
            rescue_state(&client.loader),
            (false, String::new()),
            "no evaluation error occurred -- shell.lua was never re-read, so the broken file on disk is never seen"
        );
    }

    #[test]
    fn re_resolve_if_dirty_clears_the_flag_and_a_second_call_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = oblisk.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 1,
                payload: serde_json::json!(true),
            })
            .unwrap();
        run_startup(&mut client);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 2,
                payload: serde_json::json!(false),
            })
            .unwrap();

        client.re_resolve_if_dirty();
        assert!(!client.scene.surface("bar@TEST").unwrap().visible, "the first re-resolve must apply the pushed value");
        assert!(!client.dirty.take(), "re_resolve_if_dirty must clear the flag it consumed");

        // Replace `applied_output` directly (bypassing the push path, which would re-mark dirty)
        // with an evaluation that resolves `visible` to `true`. A true no-op leaves the scene
        // exactly as the first resolve left it.
        let poisoned_path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = true }"#);
        let (poisoned_output, _) = evaluate_and_specs(&client.loader, &poisoned_path).unwrap();
        client.state.applied_output = Some(poisoned_output);

        client.re_resolve_if_dirty();
        assert!(
            !client.scene.surface("bar@TEST").unwrap().visible,
            "with nothing pushed since, a second re-resolve must do no work at all, even though a different applied_output is now in place"
        );
    }

    #[test]
    fn a_burst_of_pushes_before_one_check_marks_the_flag_only_once() {
        // ADR-0044 decision 2's "drain first, then re-resolve once": several pushes landing
        // before one read must coalesce into a single dirty read, not one per push.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for revision in 1..=5 {
            client
                .apply_state_snapshot(StateSnapshot {
                    capability: "audio".to_string(),
                    revision,
                    payload: serde_json::json!({ "n": revision }),
                })
                .unwrap();
        }

        assert!(client.dirty.take(), "a burst of five pushes must have marked the flag");
        assert!(
            !client.dirty.take(),
            "the flag records only whether a push happened since the last check, not how many, so the burst coalesces into one turn's work"
        );
    }

    #[test]
    fn a_push_that_makes_a_property_invalid_keeps_the_prior_scene_and_does_not_enter_rescue() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = oblisk.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 1,
                payload: serde_json::json!(true),
            })
            .unwrap();
        run_startup(&mut client);
        assert!(client.scene.surface("bar@TEST").unwrap().visible);
        assert_eq!(rescue_state(&client.loader), (false, String::new()));

        // `visible` requires a boolean; a table makes the re-resolve fail.
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 2,
                payload: serde_json::json!({ "not": "a boolean" }),
            })
            .unwrap();

        client.re_resolve_if_dirty();

        assert!(
            client.scene.surface("bar@TEST").unwrap().visible,
            "Scene::apply rolls back to its pre-call state on error, so the prior good scene must survive"
        );
        assert_eq!(
            rescue_state(&client.loader),
            (false, String::new()),
            "a rejected pushed value is not a shell.lua evaluation failure and must not enter rescue"
        );
    }

    #[test]
    fn a_config_binding_a_bare_rostered_signal_applies_at_startup_with_no_push_at_all() {
        // ADR-0044 decision 1's nil rule: `run_startup_evaluation` runs before one inbound frame
        // is drained, so every rostered capability still reads `nil`. A config that binds one bare
        // must still apply, taking each parser's absent-property default.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", visible = oblisk.audio, child = rect { width = oblisk.network, height = 10, children = oblisk.tray } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        let bar = client.scene.surface("bar@TEST").expect("a bare rostered signal must not stop the config applying");
        assert!(bar.visible, "`visible = audio` with audio still nil must take parse_visible's default");
        assert!(
            bar.children[0].children.is_empty(),
            "`children = tray` with tray still nil must take parse_children's default"
        );
        assert_eq!(
            rescue_state(&client.loader),
            (false, String::new()),
            "a startup that applies must not be in rescue"
        );
    }

    #[test]
    fn a_text_node_bound_to_a_bare_rostered_signal_applies_at_startup_with_no_push_at_all() {
        // ADR-0044's headline example: `text { content = oblisk.mpris.title }` must apply at boot
        // even though `title` still reads `nil`, the same rule as `visible`/`children` above.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = oblisk.audio } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        let bar = client
            .scene
            .surface("bar@TEST")
            .expect("a bare rostered signal on `content` must not stop the config applying");
        assert_eq!(
            layout::node::parse_content(&bar.children[0].properties).unwrap().0,
            "",
            "`content = audio` with audio still nil must take parse_content's default"
        );
        assert_eq!(
            rescue_state(&client.loader),
            (false, String::new()),
            "a startup that applies must not be in rescue"
        );
    }

    #[test]
    fn a_push_arriving_before_the_first_successful_apply_is_not_consumed_and_lost() {
        // `applied_output` is checked *before* the flag is taken: consuming the flag with nothing
        // to re-resolve against would silently discard the push.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (mut client, _outbound_rx) = test_client(&missing);
        run_startup(&mut client);
        assert!(client.state.applied_output.is_none(), "startup must have failed (no file)");

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "audio".to_string(),
                revision: 1,
                payload: serde_json::json!({ "app_name": "Zen" }),
            })
            .unwrap();
        client.re_resolve_if_dirty();

        assert!(
            client.dirty.take(),
            "the push must still be pending for whatever applies next, not consumed by the early return"
        );
    }

    #[test]
    fn repeated_re_resolves_that_retire_nodes_do_not_grow_the_lease_bag() {
        // `Scene::apply` runs up to once per poll turn, and `retire_child_first` pushes every
        // removed subtree onto `Scene::retiring`, which nothing in production drains
        // (ADR-0023). A children signal that alternates its length would leak a
        // `RetainedNode` at push cadence, in a process meant to run for a whole session.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            return panel { id = "bar", layer = "Top", child = row { children = computed({oblisk.audio}, function(n)
                if n == 3 then
                    return { rect { width = 1, height = 1 }, rect { width = 1, height = 1 }, rect { width = 1, height = 1 } }
                end
                return { rect { width = 1, height = 1 } }
            end) } }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        run_startup(&mut client);
        assert!(client.scene.surface("bar@TEST").is_some(), "startup must have applied");

        for revision in 1..=20 {
            let count = if revision % 2 == 0 { 1 } else { 3 };
            client
                .apply_state_snapshot(StateSnapshot {
                    capability: "audio".to_string(),
                    revision,
                    payload: serde_json::json!(count),
                })
                .unwrap();
            client.re_resolve_if_dirty();
        }

        assert_eq!(
            client.scene.surface("bar@TEST").unwrap().children[0].children.len(),
            1,
            "the last push shrank the row back to one child"
        );
        assert!(
            client.scene.retiring_ids().is_empty(),
            "a successful apply must drain the lease bag, since nothing holds a lease today; got {} entries after 20 re-resolves",
            client.scene.retiring_ids().len()
        );
    }

    #[test]
    fn a_clean_startup_leaves_the_scene_flag_clear() {
        // `set_rescue_state` writes through `rescue_handle`, which shares the one `DirtyFlag`, so
        // a successful startup used to mark the scene dirty by clearing rescue that was already
        // clear, costing a whole redundant `Scene::apply` on the first poll turn.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = "hi" } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        assert!(client.scene.surface("bar@TEST").is_some(), "startup must have applied");
        assert!(
            !client.dirty.take(),
            "an apply that succeeded resolved every signal at its current value, so nothing is stale"
        );
    }

    /// `screens_payload`'s shape, hand-written here so these tests do not depend on
    /// `crate::wayland` (which needs a live compositor to produce one).
    fn screens_json(names: &[&str]) -> serde_json::Value {
        serde_json::Value::Array(
            names
                .iter()
                .map(|name| serde_json::json!({ "name": name, "width": 1920, "height": 1080, "scale": 1, "refresh": 60.0 }))
                .collect(),
        )
    }

    #[test]
    fn a_config_looping_over_screens_declares_one_panel_per_connected_output() {
        // ADR-0041 decision 1: no `variants` primitive, because Lua already has `for`. If
        // the seed did not land before the evaluation, the loop would run zero times.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            local panels = {}
            for _, screen in ipairs(oblisk.screens:get()) do
                panels[#panels + 1] = panel { id = "bar@" .. screen.name, layer = "Top", monitor = screen.name }
            end
            return panels
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(client.set_screens(screens_json(&["eDP-1", "DP-1"])));
        let specs = client.run_startup_evaluation().expect("the config must evaluate");

        assert_eq!(specs.iter().map(SurfaceSpec::declared_id).collect::<Vec<_>>(), ["bar@eDP-1", "bar@DP-1"]);
        assert!(matches!(&specs[1], SurfaceSpec::Panel(panel) if panel.topology.monitor == "DP-1"));
    }

    #[test]
    fn a_config_reading_screens_before_any_output_is_known_sees_an_empty_list_not_a_nil() {
        // A `nil` here would make `ipairs` error and drop a config that never did anything wrong
        // straight into rescue.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = "screens: " .. #oblisk.screens:get() } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(run_startup(&mut client), "an unseeded `screens` must not fail the evaluation");
        let tree = client.scene.surface("bar@TEST").unwrap();
        assert_eq!(
            tree.children[0].properties.get("content").unwrap().as_string().unwrap().to_string_lossy(),
            "screens: 0"
        );
    }

    #[test]
    fn set_screens_repeating_the_same_list_reports_no_change_and_leaves_the_scene_clean() {
        // `update_output` fires for changes `screens` does not carry, so an unchanged re-push must
        // not buy a `Scene::apply` or a Supervisor round trip.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client.set_screens(screens_json(&["eDP-1"]));
        run_startup(&mut client);

        assert!(!client.set_screens(screens_json(&["eDP-1"])), "the same list is not an output change");
        assert!(!client.dirty.take(), "and must not mark the scene dirty");
    }

    #[test]
    fn a_real_output_change_marks_the_scene_dirty_so_a_config_reading_screens_re_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = computed({oblisk.screens}, function(list) return "n=" .. #list end) } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        client.set_screens(screens_json(&["eDP-1"]));
        run_startup(&mut client);
        let content = |client: &RendererClient| {
            client.scene.surface("bar@TEST").unwrap().children[0]
                .properties
                .get("content")
                .unwrap()
                .as_string()
                .unwrap()
                .to_string_lossy()
        };
        assert_eq!(content(&client), "n=1");

        assert!(client.set_screens(screens_json(&["eDP-1", "DP-1"])), "a monitor appearing is an output change");
        assert!(client.re_resolve_if_dirty(), "and the scene must re-resolve against it without re-reading shell.lua");
        assert_eq!(content(&client), "n=2");
    }

    #[test]
    fn seeding_screens_before_the_startup_apply_still_leaves_the_scene_flag_clear() {
        // The seed marks the flag like any live-signal write, but the apply that immediately
        // follows resolved against that very value, so entering the poll loop dirty would buy one
        // redundant `Scene::apply` before anything is drawn.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = "hi" } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(client.set_screens(screens_json(&["eDP-1"])));
        assert!(run_startup(&mut client));

        assert!(!client.dirty.take(), "the startup apply already resolved against the seeded screen list");
    }

    #[test]
    fn applied_surface_specs_returns_the_applied_declarations_without_reading_shell_lua_again() {
        // What a monitor hotplug re-expands against (ADR-0038 decision 3). The file is
        // deleted mid-test to prove it is never touched.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", monitor = "All" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        run_startup(&mut client);
        std::fs::remove_file(&path).unwrap();

        let specs = client.applied_surface_specs();
        assert_eq!(specs.len(), 1);
        assert!(
            matches!(&specs[0], SurfaceSpec::Panel(panel) if panel.topology.id == "bar" && panel.topology.monitor == "All")
        );
    }

    #[test]
    fn applied_surface_specs_is_empty_when_no_evaluation_has_ever_applied() {
        // A startup evaluation that failed declares nothing, so a hotplug adds no instance.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/no/such/shell.lua"));
        assert!(client.applied_surface_specs().is_empty());
    }

    #[test]
    fn request_reload_queues_the_frame_the_supervisor_starts_a_cycle_from() {
        // ADR-0041 decision 4: `is_current_reload` would drop the report of any sequence the
        // Supervisor did not itself send.
        let (client, mut outbound_rx) = test_client(std::path::Path::new("/no/such/shell.lua"));
        client.request_reload();
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::RequestReload);
    }

    #[test]
    fn a_topology_changed_reevaluate_leaves_the_scene_flag_clear() {
        // A `TopologyChanged` verdict must leave this generation's scene alone entirely (a
        // generation swap owns it instead), but the no-op `set_rescue_state(false, "")` on the
        // success path used to leave the flag set, so the next poll turn re-applied
        // `applied_output` to a scene that must not be mutated.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology {
            id: "other".to_string(),
            layer: LayerKind::Top,
            anchor: Default::default(),
            monitor: "All".to_string(),
            namespace: "oblisk-other".to_string(),
        })]);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 })
        );
        assert!(
            !client.dirty.take(),
            "a topology-changed generation must not have its scene marked dirty by the verdict itself"
        );
    }

    #[test]
    fn handle_frame_answers_a_reevaluate_frame_with_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        // A different applied topology so the fresh evaluation reads as changed -- proves the
        // dispatch/queue path, not `handle_reevaluate`'s own classification logic.
        client.state.applied_topology = Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology {
            id: "other".to_string(),
            layer: LayerKind::Top,
            anchor: Default::default(),
            monitor: "All".to_string(),
            namespace: "oblisk-other".to_string(),
        })]);

        assert_eq!(
            client.handle_frame(SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 })),
            FrameOutcome::Handled
        );

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 })
        );
    }

    #[test]
    fn handle_frame_hands_an_activate_draw_nonce_back_to_the_wayland_loop() {
        // Drawing needs `wayland::App`'s EGL and surface state, so the nonce goes back to the
        // caller for `App::activate_draw`.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(
            client.handle_frame(SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })),
            FrameOutcome::ActivateDraw(42)
        );
    }

    #[test]
    fn handle_frame_hands_a_set_session_lock_back_to_the_wayland_loop_in_both_directions() {
        // Both directions matter: `locked = true` reaches the Wayland thread to be refused there
        // when no `lock` surface is declared (ADR-0052 decision 3), and `locked = false` is
        // the only path permitted to unlock at all (ADR-0042).
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(
            client.handle_frame(SupervisorFrame::SetSessionLock(SetSessionLock { locked: true })),
            FrameOutcome::SetSessionLock(true)
        );
        assert_eq!(
            client.handle_frame(SupervisorFrame::SetSessionLock(SetSessionLock { locked: false })),
            FrameOutcome::SetSessionLock(false)
        );
    }

    #[test]
    fn handle_frame_logs_and_continues_on_deselect_input_and_promote_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);

        assert_eq!(
            client.handle_frame(SupervisorFrame::DeselectInput(DeselectInput { surface_id: "main_bar".to_string() })),
            FrameOutcome::Handled
        );
        assert_eq!(
            client.handle_frame(SupervisorFrame::PromoteGeneration(PromoteGeneration {
                surface_id: "main_bar".to_string()
            })),
            FrameOutcome::Handled
        );
        // A third, recognized frame to prove dispatch kept working after the two inert ones above
        // -- the report's exact verdict isn't the point, only that a real response arrives at all.
        assert_eq!(
            client.handle_frame(SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 9 })),
            FrameOutcome::Handled
        );

        assert_eq!(
            queued_frame(&mut outbound_rx),
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 9 })
        );
    }

    #[test]
    fn handle_frame_routes_process_output_and_exit_frames_to_the_registered_lua_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        // Register real out_cb/exit_cb through the real process.run global -- the id (0, the
        // first call on a fresh registry) is what the inbound frames below address.
        client
            .loader
            .evaluate(
                r#"
                process.run("cmd", {}, function(line, stream) probe_line = line; probe_stream = stream end, function(code) probe_code = code end)
                return panel { id = "bar", layer = "Top" }
                "#,
            )
            .unwrap();

        let output_frame = SupervisorFrame::ProcessOutput(ProcessOutputLine {
            id: 0,
            stream: shared::ProcessStream::Stdout,
            line: "hello".to_string(),
        });
        assert_eq!(client.handle_frame(output_frame), FrameOutcome::Handled);
        assert_eq!(
            client.handle_frame(SupervisorFrame::ProcessExited(ProcessExited { id: 0, code: Some(3) })),
            FrameOutcome::Handled
        );

        let lua = client.loader.lua().globals();
        assert_eq!(lua.get::<String>("probe_line").unwrap(), "hello");
        assert_eq!(lua.get::<String>("probe_stream").unwrap(), "stdout");
        assert_eq!(lua.get::<i64>("probe_code").unwrap(), 3);
    }

    /// Queues `frame` for the socket thread and returns what [`pump`] actually wrote to the wire
    /// for it. `pump`'s read half never produces anything here, so a timeout bounds the test.
    async fn pumped_to_the_wire(frame: RendererFrame) -> RendererFrame {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (inbound_tx, _inbound_rx) = std::sync::mpsc::channel();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        outbound_tx.send(frame).unwrap();

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx);
        let read_response = read_json_frame::<_, RendererFrame>(&mut wire);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                () = pumping => unreachable!("pump must not return on its own in this test"),
                frame = read_response => frame.unwrap(),
            }
        })
        .await
        .expect("the queued frame must reach the wire before the timeout")
    }

    #[tokio::test]
    async fn pump_writes_a_ready_signal_frame_to_the_wire() {
        let surfaces = vec!["main_bar".to_string(), "overlay_canvas".to_string()];
        let written = pumped_to_the_wire(RendererFrame::ReadySignal(ReadySignal { surfaces: surfaces.clone() })).await;
        assert_eq!(written, RendererFrame::ReadySignal(ReadySignal { surfaces }));
    }

    #[tokio::test]
    async fn pump_writes_a_presentation_evidence_frame_to_the_wire() {
        let evidence = PresentationEvidence { nonce: 7, surface_id: "wallpaper_layer@DP-1".to_string() };
        let written = pumped_to_the_wire(RendererFrame::PresentationEvidence(evidence.clone())).await;
        assert_eq!(written, RendererFrame::PresentationEvidence(evidence));
    }

    #[tokio::test]
    async fn pump_writes_a_queued_process_command_frame_to_the_wire() {
        let envelope = CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "ExecuteCommand".to_string(),
            params: shared::CommandParams {
                generation_id: 0,
                capability: "process".to_string(),
                action: "run".to_string(),
                arguments: vec![serde_json::json!("echo"), serde_json::json!(["hi"])],
                expected_revision: 0,
            },
            id: 1,
        };

        match pumped_to_the_wire(RendererFrame::Command(envelope)).await {
            RendererFrame::Command(envelope) => {
                assert_eq!(envelope.params.capability, "process");
                assert_eq!(envelope.params.action, "run");
                assert_eq!(envelope.id, 1);
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_writes_a_secure_submit_frames_secret_to_the_wire_intact() {
        // ADR-0005/ADR-0027: the wire frame carries the exact secret read out of the accumulated
        // `SecureBuffer`. `pump` zeroizes the frame's plaintext copy the instant the write completes.
        let written = pumped_to_the_wire(RendererFrame::SecureSubmit(SecureSubmit {
            generation_id: 4,
            capability: "polkit".to_string(),
            action: "authenticate".to_string(),
            secret: b"hunter2".to_vec(),
        }))
        .await;

        assert_eq!(
            written,
            RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            })
        );
    }

    #[tokio::test]
    async fn pump_forwards_a_decoded_supervisor_frame_to_the_wayland_thread() {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        // Write the frame and close the write half so `pump`'s *second* read hits EOF and
        // returns after forwarding exactly one frame.
        write_json_frame(&mut wire, &SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })).await.unwrap();
        wire.shutdown().await.unwrap();

        let (inbound_tx, inbound_rx) = std::sync::mpsc::channel();
        let (_outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx).await;

        assert_eq!(inbound_rx.try_recv(), Ok(SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })));
    }

    /// Advances `pumping` for up to `millis` and returns -- unless `pump` itself completes first,
    /// always a bug in these tests: it means one of its two loops broke, not that it merely
    /// yielded control back.
    async fn let_pump_advance(mut pumping: std::pin::Pin<&mut impl std::future::Future<Output = ()>>, millis: u64) {
        tokio::select! {
            () = &mut pumping => unreachable!("pump must not return on its own in this test"),
            () = tokio::time::sleep(std::time::Duration::from_millis(millis)) => {}
        }
    }

    /// `shared::framing::read_frame` does two sequential `read_exact` awaits, so partial progress
    /// lives in the read future itself. The old `pump` raced one `read_json_frame` call against
    /// one `outbound_rx.recv()` per `tokio::select!` iteration -- if the outbound branch won while
    /// a read was stuck mid-payload, `select!` dropped the read future, losing the bytes already
    /// consumed, and the next iteration read a length prefix out of the middle of a JSON payload.
    ///
    /// This drives exactly that race: an inbound frame's bytes arrive split across two writes,
    /// with an outbound frame becoming available while the inbound read is stalled in between.
    /// The fixed `pump` gives each direction its own long-lived loop, so the stalled read survives
    /// the outbound activity intact.
    #[tokio::test]
    async fn pump_survives_an_inbound_frame_split_around_an_outbound_frame() {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (inbound_tx, inbound_rx) = std::sync::mpsc::channel();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        let inbound_frame = SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 });
        let payload = serde_json::to_vec(&inbound_frame).unwrap();
        let mut wire_bytes = (payload.len() as u32).to_be_bytes().to_vec();
        wire_bytes.extend_from_slice(&payload);
        // Splits inside the payload, past the length prefix: stalls the *second* read_exact.
        let split_at = wire_bytes.len() / 2;
        assert!(split_at > 4, "the split point must land inside the payload, not the length prefix");

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx);
        tokio::pin!(pumping);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            wire.write_all(&wire_bytes[..split_at]).await.unwrap();
            // Let pump's reader consume the partial payload and block mid-`read_exact`.
            let_pump_advance(pumping.as_mut(), 20).await;

            // Queue an outbound frame while the inbound read is stalled mid-frame -- exactly the
            // race the old per-iteration `select!` lost.
            outbound_tx
                .send(RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }))
                .unwrap();
            let_pump_advance(pumping.as_mut(), 20).await;

            // The write direction must not be starved by the stuck read.
            let written = read_json_frame::<_, RendererFrame>(&mut wire).await.unwrap();
            assert_eq!(written, RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }));

            // Complete the inbound frame. If the earlier outbound activity had cancelled the read
            // (the bug), this length prefix would land mid-payload instead of at a frame boundary.
            wire.write_all(&wire_bytes[split_at..]).await.unwrap();
            let_pump_advance(pumping.as_mut(), 20).await;
        })
        .await
        .expect("pump must keep servicing both directions within the timeout");

        assert_eq!(
            inbound_rx.try_recv(),
            Ok(SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })),
            "the inbound frame split across two writes around an outbound frame must still decode correctly"
        );
    }
}
