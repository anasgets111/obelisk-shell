//! Renderer-side Unix control-socket client and `SupervisorFrame` handling. Connects to
//! `$XDG_RUNTIME_DIR/obelisk-shell.sock`; the Supervisor listens (`supervisor/src/socket.rs`) and
//! sends `shared::ConnectionHandshake` first. Two threads/channels (ADR-0039): [`pump`] does framed
//! I/O, while the Wayland thread owns Lua and the GL-context paint pass because `mlua::Lua` is
//! `!Send`. `StateSnapshot` hydrates a capability signal and dirties the scene (ADR-0044 decision
//! 2), then runs its `on_change` handlers (ADR-0115); only `Reevaluate` runs Lua, classifying
//! against `applied_topology` as `Unchanged`, `TopologyChanged`, or `Failed`. `None` means "safe to
//! apply", not "empty topology", or startup failure would blank the shell. No reconnect after
//! disconnect (ADR-0059 decision 1): the Supervisor owns capabilities, `process.run` children, and
//! PAM.

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

/// `SupervisorFrame`s held for the Wayland thread before the socket reader is parked.
///
/// The queue was unbounded, so Supervisor's own bounded outbound simply moved the growth here: a
/// Renderer slow to drain (a long paint, a blocking decode) accumulated frames in its own heap
/// instead. Backpressure rather than a drop policy, for the same reason it is backpressure on the
/// Supervisor side: these carry lock and reload traffic, and a dropped one is a protocol failure
/// rather than a lost log line. Sized to absorb a full snapshot replay at reconnection without
/// parking, which is the largest legitimate burst.
pub const INBOUND_CAPACITY: usize = 1024;

/// This Renderer's generation id (`OBELISK_GENERATION_ID`, default `0`), stamped into the handshake
/// and every outbound `CommandEnvelope`/`SecureSubmit`.
pub fn generation_id_from_env() -> u32 {
    std::env::var(shared::GENERATION_ID_ENV).ok().and_then(|value| value.parse().ok()).unwrap_or(0)
}

/// Connects to `path`, sends the `generation_id` handshake, and returns the live stream.
async fn connect_and_handshake(
    path: &Path,
    generation_id: u32,
) -> Result<UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = UnixStream::connect(path).await?;
    write_json_frame(&mut stream, &ConnectionHandshake { generation_id }).await?;
    Ok(stream)
}

/// Spawns the connect-and-hold-open thread. Failure logs and drops `inbound_tx`, which the Wayland
/// thread reads as `Disconnected` (ADR-0059 decision 1). `supervisor/src/main.rs` binds first, so
/// there is no startup race.
pub fn spawn_client(
    generation_id: u32,
    inbound_tx: tokio::sync::mpsc::Sender<SupervisorFrame>,
    outbound_rx: mpsc::UnboundedReceiver<RendererFrame>,
    waker: crate::wake::Waker,
) {
    std::thread::spawn(move || {
        // Hold for the thread's life: connection close, runtime failure, or panic wakes Wayland to
        // find `inbound_rx` disconnected, rather than blocking on an unsatisfiable poll (ADR-0124).
        let _wake_on_exit = crate::wake::WakeOnDrop(waker.clone());
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("control-socket client: failed to start runtime: {err}");
                return;
            }
        };
        runtime.block_on(run(generation_id, inbound_tx, outbound_rx, waker));
    });
}

/// Reload state. `applied_topology` is this generation's topology and is `None` only before any
/// evaluation. `pending` holds evaluated-but-unapplied output/topology between `Unchanged`
/// `Reevaluate` and `ApplyPendingReload`. `applied_output` is ADR-0044 decision 2's re-resolve
/// target (Supervisor services § 14.2), retained so pushes skip `shell.lua`. mlua 0.12's `ValueRef`
/// holds `WeakLua`: a retained `mlua::Value` does not keep Lua alive and
/// `ValueRef::to_pointer` panics after death, so Rust field-drop order must keep `Lua` last (see
/// [`RendererClient`]).
struct ReloadState {
    applied_topology: Option<Vec<SurfaceFingerprint>>,
    applied_output: Option<lua::LoadOutput>,
    pending: Option<(u64, lua::LoadOutput, Vec<SurfaceFingerprint>)>,
}

/// What an inbound frame still owes Wayland after [`RendererClient::handle_frame`]. One enum avoids
/// an `Option<u64>` plus out-parameter: `ActivateDraw` needs `crate::wayland::App`'s EGL/surface
/// state; `SetSessionLock` needs SCTK `SessionLockState` and lock surfaces (ADR-0042). Two options
/// could make a caller service both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Fully serviced by [`RendererClient::handle_frame`].
    Handled,
    /// Supervisor services § 14.2's `ActivateDraw`: draw surfaces and request per-surface
    /// presentation feedback tagged with this nonce (`crate::wayland::App::activate_draw`).
    ActivateDraw(u64),
    /// ADR-0042/ADR-0052's `SetSessionLock`: match the session lock to this flag
    /// (`crate::wayland::App::set_session_lock`).
    SetSessionLock(bool),
}

/// One generation's Lua side: VM, retained scene, live signals, and reload state. Deliberately
/// `!Send`; `crate::wayland::App` owns it directly (ADR-0039), so closures, reconcile, and EGL
/// need no channel hop. **Field order is load-bearing: `loader` must stay last**. Most fields hold
/// `mlua::Value`s whose `WeakLua` does not keep the VM alive; moving `loader` first drops Lua
/// before them and panics (see [`ReloadState`]).
pub struct RendererClient {
    shell_lua_path: PathBuf,
    scene: Scene,
    /// `(surface, output)` pairs resolved by `expand_instances`, shared by
    /// [`Self::apply_instances`], [`Self::handle_apply_pending`], and
    /// [`Self::re_resolve_if_dirty`].
    instances: Vec<SurfaceInstance>,
    /// Whether this process holds or requested a lock. Arms [`lock_stays_authenticatable`]:
    /// `SurfaceFingerprint::Lock` carries only `id`, so editing `child` reads `Unchanged` and
    /// reloads in place; deleting the password while locked would leave only a VT switch. Written
    /// by `crate::wayland::App::set_session_lock` and teardown. **A `bool`, not instance ids**:
    /// hotplug replaces instances, so the veto reads [`Self::instances`].
    holds_session_lock: bool,
    /// Whether the last pass was the one follow-up a moved `geometry` rect earns (ADR-0147
    /// amendment). A moved rect in that pass gets no second follow-up, so a binding fed by its own
    /// measurement settles or stops, never spins.
    geometry_follow_up: bool,
    /// Clone of `crate::wayland::App`'s `ShapingHandle`: one worker and `FontSystem` per process
    /// (ADR-0023).
    shaping: ShapingHandle,
    /// Capability handles keyed by `StateSnapshot.capability` (ADR-0029), seeded from
    /// `shared::Capability::ALL` (ADR-0037). `RefCell` permits access through `&self`.
    capabilities: RefCell<HashMap<String, CapabilityHandle>>,
    /// Cloned into every [`Capability`], including lazy ones.
    /// Also this client's outbound half: [`CommandSender::frames`] hands out the Renderer ->
    /// Supervisor sender that socket-thread [`pump`] drains to the wire.
    ///
    /// ponytail: unbounded, and deliberately so for now. `send` is called from synchronous Wayland
    /// dispatch callbacks, which cannot await, so backpressure is not available here; and
    /// `try_send` dropping a `SecureSubmit` would lose a password mid-unlock rather than delay it.
    /// The ceiling is therefore a config that produces frames faster than the socket drains.
    /// Upgrade path: give the callbacks a non-blocking handoff to an async sender that can park,
    /// so the bound lands on the queue instead of on the callback. Bounding this channel as it
    /// stands would trade a memory bug for a correctness one.
    commands: CommandSender,
    rescue_handle: LiveSignalHandle,
    /// Renderer-sourced `obelisk.screens` handle (ADR-0041 decision 2), not in `capabilities`.
    screens_handle: LiveSignalHandle,
    /// `screens_handle`'s JSON mirror for [`Self::set_screens`] change detection.
    screens_payload: serde_json::Value,
    /// `rescue_handle`'s mirror for [`Self::set_rescue_state`] no-op detection.
    rescue_state: (bool, String),
    process_registry: ProcessRegistry,
    /// Renderer-sourced `obelisk.idle` threshold callbacks (ADR-0032).
    idle_registry: crate::lua::idle::IdleRegistry,
    /// Scene-dirty flag (ADR-0044 decision 2), cloned into every handed-out `LiveSignalHandle`.
    dirty: DirtyFlag,
    state: ReloadState,
    /// `obelisk` table for lazy members. Above `loader` for drop order.
    obelisk: mlua::Table,
    /// Last, load-bearing; see the struct docs.
    loader: Loader,
}

impl RendererClient {
    /// Builds one generation's VM, rescue signal, `process` registry, and seeded capability signals
    /// on the calling thread. `crate::wayland::run` calls it, never the socket thread: `mlua::Lua`
    /// is `!Send` and must be built where it runs (ADR-0039). Failure is fatal; no VM means no
    /// `shell.lua` evaluation or screen output.
    pub fn start(
        shaping: ShapingHandle,
        outbound_tx: mpsc::UnboundedSender<RendererFrame>,
        generation_id: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shell_lua_path =
            shared::shell_lua_path().map_err(|err| format!("failed to resolve shell.lua's path: {err}"))?;
        // One flag per generation (ADR-0044 decision 2), shared by rescue.
        let dirty = DirtyFlag::new();
        // `require` searches here and nowhere else (ADR-0047 decision 1); derive it once.
        let config_dir = shell_lua_path.parent().ok_or("shell.lua's path has no parent directory")?.to_path_buf();
        let loader =
            Loader::new(dirty.clone(), &config_dir).map_err(|err| format!("failed to start the Lua loader: {err}"))?;
        let process_registry = ProcessRegistry::new(generation_id, outbound_tx.clone());
        loader
            .register_process(process_registry.clone())
            .map_err(|err| format!("failed to register the process global: {err}"))?;
        // `ProcessRegistry` uses the same id, so `process.kill` cannot cross generations. § 3.2
        // commands all use this write path.
        let commands = CommandSender::new(generation_id, outbound_tx);
        let client = Self::new(loader, shell_lua_path, shaping, commands, process_registry, dirty)
            .map_err(|err| format!("failed to build the `obelisk` namespace: {err}"))?;
        Ok(client)
    }

    /// [`lua::namespace::build`] owns the `obelisk` namespace; construction is separate from frame
    /// handling. [`Self::capability_handle`] lazily handles unrostered names from snapshots.
    fn new(
        loader: Loader,
        shell_lua_path: PathBuf,
        shaping: ShapingHandle,
        commands: CommandSender,
        process_registry: ProcessRegistry,
        dirty: DirtyFlag,
    ) -> mlua::Result<Self> {
        // Take it from `commands`, avoiding a drifting clone.
        let namespace = lua::namespace::build(&loader, &dirty, &commands, &shell_lua_path)?;
        Ok(Self {
            shell_lua_path,
            scene: Scene::new(),
            instances: Vec::new(),
            holds_session_lock: false,
            geometry_follow_up: false,
            shaping,
            capabilities: RefCell::new(namespace.capabilities),
            commands,
            rescue_handle: namespace.rescue,
            screens_handle: namespace.screens,
            screens_payload: namespace.screens_payload,
            // Matches `lua::namespace::build`'s initial rescue signal.
            rescue_state: (false, String::new()),
            process_registry,
            idle_registry: namespace.idle,
            dirty,
            state: ReloadState { applied_topology: None, applied_output: None, pending: None },
            obelisk: namespace.table,
            loader,
        })
    }

    /// Writes `rescue`'s `{ is_rescue, error_log }` only when changed. A write marks the shared
    /// `DirtyFlag` (ADR-0044 decision 2); a no-op rewrite could dirty a generation that must not
    /// mutate, unlike the genuine transition rejected by ADR-0044 decision 3. `pub` for
    /// `crate::wayland::App`'s `SessionLockHandler` (ADR-0052 decision 4), used for refused locks
    /// and both `finished` cases, the only user-facing path there.
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

    /// Hydrates only `snapshot.capability`; no Lua evaluation (module docs). `&self` is needed for
    /// the lazy `capabilities` registration.
    fn apply_state_snapshot(&self, snapshot: StateSnapshot) -> mlua::Result<()> {
        let value = self.loader.to_lua_value(&snapshot.payload)?;
        // Revision stamped into later `obelisk.<name>:invoke(...)`; advisory because dispatch does
        // not enforce it (Supervisor services § 13).
        let handle = self.capability_handle(&snapshot.capability)?;
        let previous = handle.hydrate(value, snapshot.revision);
        // Run `on_change` handlers (ADR-0115) after hydration and before layout. This is the only
        // Lua a push runs, not a `shell.lua` evaluation.
        handle.notify_change(self.loader.lua(), previous);
        Ok(())
    }

    /// Before each `shell.lua` evaluation, clear old `on_change` handlers. Evaluation registers
    /// them afresh; retaining them doubles side effects after a config save (ADR-0115).
    fn clear_change_handlers(&self) {
        for handle in self.capabilities.borrow().values() {
            handle.clear_handlers();
        }
    }

    /// Returns a handle, lazily adding `obelisk.<capability>` as `nil`, revision `0` (ADR-0029).
    /// Debug builds reject off-roster pushes first. **Refuses held names**: `Table::set` is silent,
    /// and an off-roster `rescue` push would replace the config-failure signal (ADR-0052 decision
    /// 1's bug).
    fn capability_handle(&self, capability: &str) -> mlua::Result<CapabilityHandle> {
        if let Some(handle) = self.capabilities.borrow().get(capability) {
            return Ok(handle.clone());
        }
        if self.obelisk.contains_key(capability)? {
            return Err(mlua::Error::runtime(format!(
                "a StateSnapshot named the unrostered capability {capability:?}, and `obelisk.{capability}` is already something else; refusing to replace it"
            )));
        }
        let (member, handle) = Capability::new(capability, self.dirty.clone(), self.commands.clone());
        self.obelisk.set(capability, member)?;
        self.capabilities.borrow_mut().insert(capability.to_string(), handle.clone());
        Ok(handle)
    }

    /// Evaluates `shell.lua` once at startup without a Supervisor round trip (ADR-0024, "safe to
    /// apply"). `applied_topology` stays `None` only on *evaluation* failure; a failed *apply*
    /// leaves it because surfaces are already bound and later topology changes need a new
    /// generation (ADR-0038). Runs before layer binding (Supervisor services § 14.2), split so the
    /// caller expands returned specs via [`Self::apply_instances`].
    pub fn run_startup_evaluation(&mut self) -> Option<Vec<SurfaceSpec>> {
        self.clear_change_handlers();
        match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                self.state.applied_topology = Some(specs.iter().map(SurfaceSpec::fingerprint).collect());
                // ADR-0044 decision 2 target: later pushes skip `shell.lua`.
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

    /// Replaces `(surface, output)` pairs, from `crate::wayland::run` before first apply and
    /// `OutputHandler` on each hotplug (ADR-0038 decision 3).
    pub fn set_instances(&mut self, instances: Vec<SurfaceInstance>) {
        self.instances = instances;
    }

    /// Arms the per-`Scene::apply` lock-authentication veto (ADR-0052 decision 3) when lock is
    /// requested, not when `locked` arrives; a reload in between could strip the field about to be
    /// shown.
    pub fn set_session_locked(&mut self, locked: bool) {
        self.holds_session_lock = locked;
    }

    /// The set last stored by [`Self::set_instances`], for `crate::wayland::App` to diff without a
    /// second copy that could drift.
    pub fn instances(&self) -> &[SurfaceInstance] {
        &self.instances
    }

    /// Declared surfaces from the applied evaluation, reparsed from `applied_output`, never
    /// `shell.lua`. Hotplug expands against this (ADR-0038 decision 3); reevaluation would race
    /// the Supervisor's `Reevaluate` (ADR-0041 decision 4). Empty before any apply.
    pub fn applied_surface_specs(&self) -> Vec<SurfaceSpec> {
        let Some(output) = self.state.applied_output.as_ref() else {
            return Vec::new();
        };
        match surface_specs(output) {
            Ok(specs) => specs,
            Err(err) => {
                // `surface_specs` already succeeded on `applied_output`; log, not panic, to keep a
                // painting shell alive.
                eprintln!("control-socket client: the applied evaluation's surface specs no longer parse: {err}");
                Vec::new()
            }
        }
    }

    /// Asks the Supervisor to start a reload cycle (ADR-0041 decision 4); it alone decides
    /// topology changes (ADR-0041 decision 3). No sequence: `supervisor/src/main.rs` drops reports
    /// without its `next_sequence`, so this only begins a cycle. Used when a `screens` loop changes
    /// the surface set.
    pub fn request_reload(&self) {
        if let Err(err) = self.commands.frames().send(RendererFrame::RequestReload) {
            eprintln!("control-socket client: failed to request a reload after an output change: {err}");
        }
    }

    /// Pushes `screens` (ADR-0041 decision 2) and reports change. As with `set_rescue_state`, the
    /// caller gates [`Self::request_reload`], so a duplicate must not request a reload.
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

    /// Records which of an instance's axes are measured from its own tree rather than allocated to
    /// it, for [`Self::set_instance_size`] to honour, and puts `ceiling` on those axes.
    /// `crate::wayland::layer` pushes both from the *resolved* panel spec, on creation and on every
    /// later pass: the socket parser cannot tell a genuinely omitted extent from a signal-bound
    /// one, since `parse_size_mode` defers both to `SizeMode::Content`, and pinning `available` for
    /// a signal-bound extent would strand the surface at its output's size.
    ///
    /// The ceiling is written, not merely permitted, because on a measured axis `available` *is*
    /// the ceiling and nothing else may set it -- `set_instance_size` declines that axis outright.
    /// An axis that becomes measured therefore has to be given one here, or it would keep whatever
    /// the last configure left: a panel reloaded from `width = 200` to a measured width would solve
    /// its content against 200 for the rest of the generation, and no later configure could widen
    /// it. Idempotent, and it dirties only when the ceiling actually moved.
    ///
    /// Unknown ids are ignored, like `set_instance_size`'s.
    pub fn set_measured_axes(&mut self, instance_id: &str, axes: (bool, bool), ceiling: layout::LogicalSize) {
        let Some(instance) = self.instances.iter_mut().find(|i| i.instance_id == instance_id) else {
            return;
        };
        instance.measured_axes = axes;
        let bounded = layout::LogicalSize {
            width: if axes.0 { ceiling.width } else { instance.available.width },
            height: if axes.1 { ceiling.height } else { instance.available.height },
        };
        if instance.available == bounded {
            return;
        }
        instance.available = bounded;
        self.dirty.mark();
    }

    /// Replaces one instance's compositor-configured `available` size and dirties the scene through
    /// ADR-0044 decision 2's [`DirtyFlag`] (ADR-0023). Ignore unknown ids instead of dirtying a
    /// nonexistent surface.
    ///
    /// A measured axis is left alone (`SurfaceInstance::measured_axes`). The compositor's answer
    /// there is the size this surface asked for after measuring its own content, so writing it back
    /// would turn the measurement into its own ceiling and the content could never outgrow the size
    /// it happened to open at.
    pub fn set_instance_size(&mut self, instance_id: &str, size: layout::LogicalSize) {
        let Some(instance) = self.instances.iter_mut().find(|i| i.instance_id == instance_id) else {
            return;
        };
        let size = layout::LogicalSize {
            width: if instance.measured_axes.0 { instance.available.width } else { size.width },
            height: if instance.measured_axes.1 { instance.available.height } else { size.height },
        };
        if instance.available == size {
            // Same no-op rule as `set_rescue_state`.
            return;
        }
        instance.available = size;
        self.dirty.mark();
    }

    /// Applies the last evaluation to current instances, setting rescue on failure. Returns success
    /// so `crate::wayland::run` can distinguish an exiting Candidate. Instances come only from
    /// [`Self::set_instances`].
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
                self.set_rescue_state(false, "");
                // Consume `set_screens`'s pre-evaluation seed (ADR-0041 decision 2) only after
                // success; a failed apply leaves it for the next one.
                self.dirty.take();
                self.settle_geometry();
                true
            }
            Err(err) => {
                eprintln!("control-socket client: startup shell.lua evaluated but failed to apply to the scene: {err}");
                self.set_rescue_state(true, &err.to_string());
                false
            }
        }
    }

    /// Retained scene; `paint_surface` looks up the resolved tree by the `"{id}@{output}"` id in
    /// its `TrackedSurface`.
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// Relays what a surface's paint actually drew into the retained scene, where a `retain`ing
    /// `image` stops covering the gap and a `transition` starts (ADR-0183). Narrow on purpose: the
    /// scene is not handed out mutably for a caller to walk itself.
    pub fn note_drawn_images(
        &mut self,
        instance_id: &str,
        drawn: &[crate::layout::paint::DrawnImage],
        now: std::time::Instant,
    ) {
        self.scene.note_drawn_images(instance_id, drawn, now);
    }

    /// Drops a departed instance's retained tree. Called by topology handling when an output goes
    /// away, which is the one path that removes a surface without a reload replacing the process.
    pub fn forget_surface(&mut self, instance_id: &str) {
        self.scene.forget(instance_id);
    }

    /// This generation's `Lua` builds the `button` `on_click` argument (ADR-0050 decision 3):
    /// `crate::wayland::App` holds the `mlua::Function`, not a VM. Do not hold the borrow across
    /// the call; see [`crate::wayland::App::fire_on_click`].
    pub fn lua(&self) -> &mlua::Lua {
        self.loader.lua()
    }

    /// Handles one [`pump`]-decoded frame. Returns [`FrameOutcome`]: `Handled`, or work handed to
    /// `crate::wayland::App` for EGL/surface draw (Supervisor § 14.2) or SCTK
    /// `SessionLockState`/lock surfaces (ADR-0042).
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
                // No per-surface input-region/focus machinery yet (ADR-0025).
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
            // `ext_session_lock_v1` is a Wayland object; `crate::wayland::App` services it
            // (ADR-0042, ADR-0052 decision 1).
            SupervisorFrame::SetSessionLock(SetSessionLock { locked }) => return FrameOutcome::SetSessionLock(locked),
            // The Supervisor addresses this process via its own socket (ADR-0032).
            SupervisorFrame::IdleEvent(IdleEvent { generation_id: _, threshold_sec, state }) => {
                self.idle_registry.dispatch_event(threshold_sec, state);
            }
            // ADR-0112: `obelisk set`/`obelisk toggle`. Refuse by name to stderr, the only place a
            // keybind mistake can be reported; the write dirties the scene.
            SupervisorFrame::SetState(set) => {
                if let Err(why) = lua::signal::write_state(self.lua(), &set) {
                    eprintln!(
                        "control-socket client: `obelisk` asked to write state {:?} and was refused: {why}",
                        set.name
                    );
                }
            }
        }
        FrameOutcome::Handled
    }

    /// Evaluates one `Reevaluate`, classifies against `state.applied_topology`, updates pending or
    /// rescue, and queues the verdict. `None` means "not changed". Diff only
    /// [`SurfaceFingerprint`](layout::node::SurfaceFingerprint) (ADR-0038 decision 2, ADR-0049
    /// decision 3): live objects accept `margin`, `keyboard_interactivity`, `exclusive`, size,
    /// and `window` `title`, so those reload in place instead of swapping generations.
    fn handle_reevaluate(&mut self, request: ReevaluateRequest) {
        self.clear_change_handlers();
        let report = match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                let topology: Vec<SurfaceFingerprint> = specs.iter().map(SurfaceSpec::fingerprint).collect();
                self.set_rescue_state(false, "");
                let topology_changed = self.state.applied_topology.as_ref().is_some_and(|applied| applied != &topology);
                if topology_changed {
                    // Another generation owns the swap; keep this scene unchanged.
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

        if let Err(err) = self.commands.frames().send(RendererFrame::ReevaluateReport(report)) {
            eprintln!("control-socket client: failed to send a ReevaluateReport: {err}");
        }
    }

    /// Applies `state.pending` only when its sequence matches `apply.sequence`; otherwise a newer
    /// `Reevaluate` superseded it, so log and ignore.
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
                // `ApplyPendingReload` follows only `Unchanged`, so this matches the earlier write.
                self.state.applied_topology = Some(topology);
                // ADR-0044 decision 2 re-resolve target.
                self.state.applied_output = Some(output);
                // The poll loop repaints on `re_resolve_if_dirty`.
                self.dirty.mark();
                self.settle_geometry();
            }
            Err(err) => {
                eprintln!("control-socket client: ApplyPendingReload's stored evaluation failed to apply: {err}")
            }
        }
    }

    /// Re-runs `Scene::apply` against `state.applied_output` after a live signal marks the scene
    /// dirty (ADR-0044 decision 2). Never touches `shell.lua`: the retained tree holds its signals,
    /// readable through this client's field ordering and decision 1's resolve-at-layout-time rule.
    /// Called once per poll turn after inbound frames; `DirtyFlag::take` coalesces pushes. Returns
    /// whether it re-resolved; `false` means clean or failed.
    pub fn re_resolve_if_dirty(&mut self) -> bool {
        // Check before taking the flag: a failed first apply must not swallow pushes and stay blank
        // until an inotify edit forces reevaluation.
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
            // Rollback keeps the prior scene. Do not set rescue: that is for `shell.lua`
            // evaluation, not a rejected capability push. ponytail: logging forever, nothing
            // user-visible. Upgrade: rescue-adjacent channel for rejected pushed values.
            eprintln!("control-socket client: dirty-scene re-resolve failed, keeping the prior scene: {err}");
            return false;
        }
        start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
        self.settle_geometry();
        dump_layout_if_asked(&self.scene);
        true
    }

    /// One follow-up pass when a pass moved a `geometry(name)` rect, so a property bound to the
    /// measurement lays out from it before anything else happens; never two in a row.
    fn settle_geometry(&mut self) {
        let moved = crate::lua::signal::take_geometry_moved(self.loader.lua());
        self.geometry_follow_up = moved && !self.geometry_follow_up;
        if self.geometry_follow_up {
            self.dirty.mark();
        }
    }

    /// One animation frame (ADR-0145): advances every tween to `now` and relays out the instances
    /// that carry one, without reading `shell.lua` or any signal. Returns whether any tree changed.
    /// Called from the poll loop when a compositor frame callback lands, the same turn position
    /// as [`Self::re_resolve_if_dirty`] and for the same downstream (surface state, hover,
    /// repaint).
    /// The poll loop's only timeout: when the earliest pending `delay(signal, ms)` is due
    /// (ADR-0146) or the earliest open `pulse(signal, ms)` window closes (ADR-0153). `None` while
    /// nothing is pending, which is the idle case ADR-0124 keeps timeout-free.
    pub fn next_wake_deadline(&self) -> Option<std::time::Instant> {
        crate::lua::signal::next_wake_deadline(self.loader.lua())
    }

    /// Dirties the scene when a `delay` came due or a `pulse` window closed, so this turn's
    /// re-resolve adopts the new value.
    pub fn wake_due_signals(&mut self) {
        if crate::lua::signal::take_due_wake(self.loader.lua(), std::time::Instant::now()) {
            self.dirty.mark();
        }
    }

    /// Returns the instance ids it advanced, so the caller repaints those surfaces and no others.
    pub fn tick_animations(&mut self, now: std::time::Instant) -> Vec<String> {
        self.scene.tick(&self.instances, &self.shaping, self.loader.lua(), now)
    }
}

/// `OBELISK_DUMP_LAYOUT=<instance id>` (e.g. `panel_host@eDP-1`) prints each visible node's kind,
/// rect, and text after every pass. Off unless asked. It answers which node has the wrong geometry
/// in a live session, including layouts the test harness did not build (a card at the bell's
/// output scale with the Supervisor's current feed).
fn dump_layout_if_asked(scene: &Scene) {
    let Ok(wanted) = std::env::var("OBELISK_DUMP_LAYOUT") else { return };
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
    walk(surface, 0, &mut out);
    eprint!("{out}");
}

async fn run(
    generation_id: u32,
    inbound_tx: tokio::sync::mpsc::Sender<SupervisorFrame>,
    mut outbound_rx: mpsc::UnboundedReceiver<RendererFrame>,
    waker: crate::wake::Waker,
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
    pump(&mut read_half, &mut write_half, &inbound_tx, &mut outbound_rx, Some(&waker)).await;
}

/// After handshake, forward decoded `SupervisorFrame`s to Wayland and queued `RendererFrame`s to
/// the wire (ADR-0039). Decode failure is transport failure: one sender and fixed shapes mean
/// desync, unlike a recoverable `ApplyPendingReload` sequence mismatch. Read and write are separate
/// long-lived futures. `read_json_frame` has two sequential `read_exact`s; racing one frame read
/// against `outbound_rx.recv()` would drop partial bytes when outbound wins and desync the stream.
async fn pump<R, W>(
    read_half: &mut R,
    write_half: &mut W,
    inbound_tx: &tokio::sync::mpsc::Sender<SupervisorFrame>,
    outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>,
    waker: Option<&crate::wake::Waker>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reader = async {
        loop {
            match framing::read_json_frame::<_, SupervisorFrame>(read_half).await {
                Ok(frame) => {
                    // Awaited, so a Supervisor pushing faster than the Wayland thread drains parks
                    // this reader rather than growing the queue. `Sender::send` is cancel-safe --
                    // if the `select!` below drops this future, the frame was not delivered and
                    // nothing half-arrives -- which is why the backpressure is safe to take here.
                    if let Err(err) = inbound_tx.send(frame).await {
                        eprintln!("control-socket client: the Wayland thread is gone; stopping the socket loop: {err}");
                        break;
                    }
                    if let Some(waker) = waker {
                        waker.wake();
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
            // Scrub immediately after write, not at `Drop` (ADR-0005/ADR-0027). The serialized
            // copy is `write_json_frame`'s to scrub and it does; this is the frame's own bytes.
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

/// Names a frame for write-failure logs. Fixed labels avoid `{frame:?}`, whose derived `Debug`
/// would print `SecureSubmit.secret` (ADR-0005).
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
        // Never sent here (ADR-0112), but a wildcard could hide a new unnamed variant.
        RendererFrame::SetState(_) => "SetState",
    }
}

/// Starts capabilities named by applied `textfield` `secure_submit`s (ADR-0070 decision 5), so a
/// password prompt registers its agent even if nothing reads the member. The sender deduplicates.
///
/// Called from every successful apply, `re_resolve_if_dirty` included, so a `textfield` a pushed
/// value reveals registers its agent on the re-resolve that reveals it rather than waiting for the
/// next reevaluation.
///
/// ponytail: one tree walk per instance per re-resolve, at capability-push cadence.
/// `CommandSender::start_capability` dedupes, so a repeat costs one set lookup and no frame.
/// Upgrade: an accumulated roster, if the walk itself ever shows up.
fn start_secure_submit_capabilities(scene: &Scene, instances: &[SurfaceInstance], commands: &CommandSender) {
    for instance in instances {
        let Some(tree) = scene.surface(&instance.instance_id) else { continue };
        for target in crate::layout::secure_submit::secure_submit_targets(tree) {
            commands.start_capability(&target.capability);
        }
    }
}

/// Diagnostic geometry after `scene.apply`. Iterates *instances*, not declarations: one declaration
/// can produce several sizes.
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
        let path = dir.path().join("obelisk-shell.sock");
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

    /// Evaluates `setup` above a minimal `panel`, then reads a global. A global avoids smuggling a
    /// value onto the node, which rejects keys absent from `lua::nodes::NODE_PROPERTIES`.
    fn probe<T: mlua::FromLua>(loader: &Loader, setup: &str, name: &str) -> T {
        loader.evaluate(&format!("{setup}\nreturn panel {{ id = \"_probe\", layer = \"Top\" }}")).unwrap();
        loader.lua().globals().get(name).unwrap()
    }

    /// Reads `rescue:get()` by probe script; `LiveSignalHandle` exposes only `set`, so this is the
    /// only way to observe `set_rescue_state`'s stored value.
    fn rescue_state(loader: &Loader) -> (bool, String) {
        let setup = "is_rescue, error_log = obelisk.rescue:get().is_rescue, obelisk.rescue:get().error_log";
        (probe(loader, setup, "is_rescue"), probe(loader, setup, "error_log"))
    }

    /// Client with a real outbound channel; return its receiver for queued socket frames.
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

    /// The whole `blur` path from Lua to the rects the compositor is handed (ADR-0195), on the
    /// shape the design exists for: a full-screen surface whose click-catcher covers everything
    /// and whose only glass is one card. A `layer-rule` blurring the surface rect flattens the
    /// entire output; this must hand over the card alone.
    #[test]
    fn a_full_screen_surfaces_blur_region_is_the_card_that_asked_and_not_the_catcher() {
        let dir = tempfile::tempdir().unwrap();
        let shell_lua = write_shell_lua(
            dir.path(),
            r##"return {
                panel {
                    id = "host", layer = "Overlay",
                    anchor = { top = true, bottom = true, left = true, right = true },
                    exclusive = false, width = "Fill", height = "Fill",
                    child = rect { width = "Fill", height = "Fill", children = {
                        button { width = "Fill", height = "Fill", on_click = function() end },
                        column { margin = { top = 260, left = 200 }, children = {
                            rect {
                                width = 620, height = 260, radius = 0,
                                background = "#20222eb0", blur = true,
                            },
                        } },
                    } },
                },
            }"##,
        );
        let (mut client, _rx) = test_client(&shell_lua);
        assert!(run_startup(&mut client), "the config must resolve into a scene");
        let tree = client.scene().surface("host@TEST").expect("the panel resolved");

        assert_eq!(
            layout::blur_regions(tree, 1.0),
            [crate::text::snap::PhysicalRect { x0: 200, y0: 260, x1: 820, y1: 520 }],
            "the card that asked, at its surface-local position"
        );
        assert_eq!(
            layout::overlay_input_regions(tree, 1.0),
            [
                crate::text::snap::PhysicalRect { x0: 0, y0: 0, x1: 1920, y1: 1080 },
                crate::text::snap::PhysicalRect { x0: 200, y0: 260, x1: 820, y1: 520 }
            ],
            "while the catcher still takes every click, which is the difference between the two walks"
        );
    }

    /// One 1920x1080 `"TEST"` output keeps fixture ids readable (`"bar@TEST"`).
    fn test_outputs() -> Vec<OutputGeometry> {
        vec![OutputGeometry { name: "TEST".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }]
    }

    /// `crate::wayland::run` startup in Supervisor § 14.2 Candidate order: evaluate, expand,
    /// store instances, apply.
    fn run_startup(client: &mut RendererClient) -> bool {
        let Some(specs) = client.run_startup_evaluation() else {
            return false;
        };
        let instances = expand_instances(&specs, &test_outputs());
        client.set_instances(instances);
        client.apply_instances()
    }

    /// Instances for exactly `ids`, for tests that seed `state.pending` instead of [`run_startup`].
    fn instances_for(ids: &[&str]) -> Vec<SurfaceInstance> {
        ids.iter()
            .map(|id| SurfaceInstance {
                instance_id: format!("{id}@TEST"),
                declared_id: (*id).to_string(),
                output: "TEST".to_string(),
                available: layout::LogicalSize { width: 1920.0, height: 1080.0 },
                measured_axes: (false, false),
            })
            .collect()
    }

    /// Next queued non-start frame, or a panic naming what was missing. Reading `obelisk.lock`
    /// queues a start (ADR-0070 decision 1), so tests would otherwise step over it; the dedicated
    /// `a_capability_read_asks_the_supervisor_to_start_it` test accounts for starts.
    fn queued_frame(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> RendererFrame {
        loop {
            match outbound_rx.try_recv().expect("a frame must have been queued for the socket thread") {
                RendererFrame::StartCapability { .. } => continue,
                frame => return frame,
            }
        }
    }

    /// All queued capability starts that `queued_frame` skips.
    fn queued_starts(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Vec<String> {
        let mut started = Vec::new();
        while let Ok(frame) = outbound_rx.try_recv() {
            if let RendererFrame::StartCapability { capability } = frame {
                started.push(capability);
            }
        }
        started
    }

    /// ADR-0070 decision 1: reading starts it. Unmentioned capabilities never run.
    #[test]
    fn a_capability_read_asks_the_supervisor_to_start_it() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load("local _ = obelisk.audio").exec().unwrap();

        assert_eq!(queued_starts(&mut outbound_rx), vec!["audio".to_string()]);
    }

    /// An evaluation touching no capability costs nothing.
    #[test]
    fn a_config_that_reads_no_capability_starts_none() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load("local _ = obelisk.version.major").exec().unwrap();

        assert!(queued_starts(&mut outbound_rx).is_empty(), "`version` is off the roster and has nothing behind it");
    }

    /// `__index` fires once per name because it moves the member onto the table. A `computed` in a
    /// `list` `itemfn` reads `obelisk.audio` once per row per layout pass; starting per read would
    /// be a frame per row per frame.
    #[test]
    fn re_reading_a_capability_queues_no_second_start() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load("for _ = 1, 50 do local _ = obelisk.audio end").exec().unwrap();

        assert_eq!(queued_starts(&mut outbound_rx), vec!["audio".to_string()]);
    }

    /// A typo stays ordinary nil so the config line gets named. Any other metamethod result would
    /// make `obelisk.audioo:get()` fail inside the engine.
    #[test]
    fn a_name_no_capability_owns_reads_nil_and_starts_nothing() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        let is_nil: bool = client.loader.lua().load("return obelisk.audioo == nil").eval().unwrap();

        assert!(is_nil);
        assert!(queued_starts(&mut outbound_rx).is_empty());
    }

    /// ADR-0070 decision 5: polkit has no roster entry or `obelisk.polkit`, so only a
    /// `secure_submit`
    /// naming it can request the authentication agent.
    #[test]
    fn a_secure_submit_target_starts_the_capability_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "prompt", layer = "Top", child = textfield {
                   secure_submit = { capability = "polkit", action = "authenticate" } } }"#,
        );
        let (mut client, mut outbound_rx) = test_client(&path);

        client.run_startup_evaluation().unwrap();
        client.set_instances(instances_for(&["prompt"]));
        assert!(client.apply_instances());

        assert!(queued_starts(&mut outbound_rx).contains(&"polkit".to_string()));
    }

    /// The other half of ADR-0070 decision 5: a field a pushed value reveals must register its
    /// agent on the re-resolve that reveals it. `re_resolve_if_dirty` never reads `shell.lua`, so
    /// waiting for the next reevaluation left a revealed prompt with no agent behind it.
    #[test]
    fn a_secure_submit_revealed_by_a_pushed_value_starts_its_capability_on_that_re_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            return panel { id = "prompt", layer = "Top", child = row { children = computed({obelisk.network}, function(ssid)
                if ssid then
                    return { textfield { secure_submit = { capability = "polkit", action = "authenticate" } } }
                end
                return {}
            end) } }
            "#,
        );
        let (mut client, mut outbound_rx) = test_client(&path);
        run_startup(&mut client);
        assert!(
            !queued_starts(&mut outbound_rx).contains(&"polkit".to_string()),
            "nothing declares the field yet, so nothing has named polkit"
        );

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "network".to_string(),
                revision: 1,
                payload: serde_json::json!("home"),
            })
            .unwrap();
        assert!(client.re_resolve_if_dirty(), "the push must have re-resolved");

        assert!(
            client.scene.surface("prompt@TEST").unwrap().children[0].children.len() == 1,
            "the re-resolve must have revealed the field"
        );
        assert!(
            queued_starts(&mut outbound_rx).contains(&"polkit".to_string()),
            "the re-resolve that revealed the field must start the capability it names"
        );
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

        assert_eq!(probe::<String>(&client.loader, "app_name = obelisk.audio:get().app_name", "app_name"), "Zen");
    }

    #[test]
    fn apply_state_snapshot_lazily_registers_an_unrostered_capabilitys_live_signal() {
        // ADR-0029: the first off-roster snapshot creates the Lua global, not an error.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        assert!(shared::Capability::from_name("workspace").is_none(), "this test needs a genuinely unrostered name");

        let snapshot = StateSnapshot {
            capability: "workspace".to_string(),
            revision: 1,
            payload: serde_json::json!({ "active": 2 }),
        };
        client.apply_state_snapshot(snapshot).unwrap();

        assert_eq!(probe::<i64>(&client.loader, "active = obelisk.workspace:get().active", "active"), 2);
    }

    /// Bar capabilities at or beyond module width for
    /// [`the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing`]. Strings exceed
    /// each `util.truncate` limit so the module clamp is exercised, not mistaken for layout.
    fn widest_bar_snapshots() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("audio", serde_json::json!({ "volume": 1.0, "muted": false, "apps": [] })),
            ("brightness", serde_json::json!({ "percent": 100 })),
            ("battery", serde_json::json!({ "present": true, "percent": 100, "charging": true })),
            ("power", serde_json::json!({ "on_battery": true, "energy_rate": 22.5, "active_profile": "performance" })),
            (
                "updates",
                serde_json::json!({ "package_manager": "pacman", "count": 0, "installing": true, "install_current_step": 128, "install_total_steps": 512 }),
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
        // The untested half of ADR-0062: a config-authored `hover` property remains a handle in the
        // resolved tree (decision 3), and writing it moves a second bound node. `pointer_frame`
        // needs a compositor, so drive `layout::hover` on a real resolved tree instead.
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

        // `hover_writes` receives the same tree as `App::sync_hover` when the pointer enters.
        let tree = client.scene.surface("bar@TEST").unwrap();
        let writes = layout::hover::hover_writes(tree, Some(layout::hit::LogicalPoint { x: 50.0, y: 10.0 }));
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

        // Leave again, the edge callback designs lose when reevaluation replaces the node
        // (ADR-0062 decision 1).
        let tree = client.scene.surface("bar@TEST").unwrap();
        for write in layout::hover::hover_writes(tree, None) {
            write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(write.hovered));
        }
        assert!(client.re_resolve_if_dirty());
        assert!(!hover_row(&client).children[0].visible, "the pointer left, so it is hidden again");
    }

    #[test]
    fn the_shipped_dev_config_evaluates_and_declares_every_surface_it_ships() {
        // Use `dev-config/obelisk/shell.lua`, not a fixture: it is the worked example split across
        // thirty-odd `require`d files, so renames or moved modules escape `components/` tests.
        // Evaluate as `run_startup_evaluation`: seeded capabilities read nil before the first
        // snapshot, as at real boot.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/obelisk/shell.lua");
        let (client, _outbound_rx) = test_client(&shell_lua);

        let (_output, specs) = evaluate_and_specs(&client.loader, &shell_lua)
            .unwrap_or_else(|err| panic!("the shipped dev config must evaluate: {err}"));

        // Assert id and role, not only count, to identify a missing surface.
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
                ("wallpaper_tooltip", "popup"),
                ("network_tooltip", "popup"),
                ("bluetooth_tooltip", "popup"),
                ("screen_recorder_tooltip", "popup"),
                ("idle_tooltip", "popup"),
                ("modal_host", "panel"),
                ("lock_screen", "lock"),
                ("polkit_dialog", "panel"),
            ]
        );
    }

    fn contains(rect: crate::text::snap::LogicalRect, point: layout::hit::LogicalPoint) -> bool {
        point.x >= rect.x && point.x < rect.x + rect.width && point.y >= rect.y && point.y < rect.y + rect.height
    }

    /// Absolute centres of `hover` nodes, accumulating parent-relative origins as `layout::hit`
    /// does.
    fn hover_region_centres(
        node: &crate::layout::ResolvedNode,
        x: f32,
        y: f32,
        out: &mut Vec<layout::hit::LogicalPoint>,
    ) {
        let (x, y) = (x + node.rect.x, y + node.rect.y);
        // A collapsed pill cell (`components/expanding_pill.lua`) keeps its slot at zero width;
        // nothing can point at it, so it has no centre to light.
        if node.properties.contains_key("hover") && node.rect.width > 0.0 && node.rect.height > 0.0 {
            out.push(layout::hit::LogicalPoint { x: x + node.rect.width / 2.0, y: y + node.rect.height / 2.0 });
        }
        for child in &node.children {
            hover_region_centres(child, x, y, out);
        }
    }

    #[test]
    fn every_hover_region_the_shipped_bar_declares_lights_exactly_one_slot() {
        // Wiring check for per-module tooltips: regions name slots by string, so a typo lights
        // nothing and never opens its tooltip while both halves still parse and resolve.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/obelisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        for (capability, payload) in widest_bar_snapshots() {
            client
                .apply_state_snapshot(StateSnapshot { capability: capability.to_string(), revision: 1, payload })
                .unwrap();
        }
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let bar = client.scene.surface("bar@TEST").unwrap();
        let mut centres = Vec::new();
        hover_region_centres(bar, 0.0, 0.0, &mut centres);
        assert!(centres.len() >= 2, "the bar declares more than one hover region, got {}", centres.len());

        // Regions nest: a pill row hides circles off the pill, while a point on a circle lights
        // both. `hover_writes` checks every region, not only the innermost; each lit region must
        // contain the point and each unlit one must not.
        for centre in &centres {
            let writes = layout::hover::hover_writes(bar, Some(*centre));
            let lit: Vec<bool> = writes.iter().map(|write| write.hovered).collect();
            assert!(lit.iter().any(|hovered| *hovered), "a region's own centre must light it, at {centre:?}");
            for write in &writes {
                assert_eq!(
                    write.hovered,
                    write.rect.is_some_and(|rect| contains(rect, *centre)),
                    "a region lights exactly when it contains the point, at {centre:?} got {lit:?}"
                );
            }
        }

        // Distinct slots, not one signal shared by regions. A copied slot name opens the wrong live
        // tooltip while both regions parse, resolve, and light one *write*.
        let writes = layout::hover::hover_writes(bar, Some(centres[0]));
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
    fn a_popup_bound_to_a_hover_slot_opens_for_that_slot_alone_and_closes_when_the_pointer_leaves() {
        // The other half of ADR-0062: `hover(name)` -> pill `hover`, `hover_rect(name)` ->
        // popup `anchor_rect`, and the popup's `visible`. A fixture rather than the shipped
        // config, so a bar module moving cannot break an engine contract, and so the pass costs
        // microseconds instead of racing the 5ms evaluation budget under a parallel test run.
        //
        // Two slots, because the contract is that a point inside one region is outside the other:
        // one region alone could not tell "opens when hovered" from "always open".
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r##"
            local left, right = hover("left"), hover("right")
            return {
                panel {
                    id = "bar", layer = "Top", width = "Fill", height = 40,
                    child = row {
                        width = "Fill", height = "Fill", spacing = 0,
                        children = {
                            row { id = "left_pill", width = 60, height = 20, hover = left },
                            row { id = "right_pill", width = 60, height = 20, hover = right },
                        },
                    },
                },
                popup {
                    id = "tip", parent = "bar",
                    anchor_rect = hover_rect("left"),
                    visible = left,
                    width = 120, height = 40,
                    grab = false,
                    anchor = "BottomLeft", gravity = "BottomRight",
                    child = rect { width = 120, height = 40, background = "#000000ff" },
                },
            }
            "##,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "the fixture must resolve into a scene");

        let tip_is_up = |client: &RendererClient| client.scene.surface("tip").expect("the popup resolves").visible;
        assert!(!tip_is_up(&client), "a hover popup is not up before the pointer has been anywhere");

        // Two bugs that happen before the first hover. `grab` defaults true in § 6, but a grabbing
        // popup needs an input serial hover cannot produce, so the compositor refused `visible =
        // true` on every re-resolve. And `anchor_rect` is required non-zero, while its rect signal
        // begins nil, which reads as absent rather than as a rect.
        let (_output, specs) = evaluate_and_specs(&client.loader, &path).expect("the fixture evaluates");
        let tip = specs
            .iter()
            .find_map(|spec| match spec {
                SurfaceSpec::Popup(popup) if popup.id == "tip" => Some(popup),
                _ => None,
            })
            .expect("the fixture declares the popup");
        assert!(!tip.grab, "a hover-opened popup must not ask for a grab; there is no click to arm its serial");
        assert!(
            tip.anchor_rect.width > 0.0 && tip.anchor_rect.height > 0.0,
            "anchor_rect has to be a real non-zero rect before anything has been hovered, got {:?}",
            tip.anchor_rect
        );

        // Cloned: the loop re-resolves through `client`, and the centres are the bar as it stood
        // before any of that.
        let bar = client.scene.surface("bar@TEST").unwrap().clone();
        let mut centres = Vec::new();
        hover_region_centres(&bar, 0.0, 0.0, &mut centres);
        assert_eq!(centres.len(), 2, "the fixture declares one hover region per pill");

        let mut opened_by = 0;
        for centre in centres {
            let writes = layout::hover::hover_writes(&bar, Some(centre));
            // Applied as `App::sync_hover` does: a point inside one region is outside the others,
            // and turning those *off* is half the walk.
            for write in &writes {
                assert_eq!(
                    write.hovered,
                    write.rect.is_some_and(|rect| contains(rect, centre)),
                    "a region lights exactly when it contains the point"
                );
            }
            for write in writes {
                write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(write.hovered));
                let Some(rect) = write.rect else {
                    continue;
                };
                // Built here because `crate::wayland::input` is private. This is § 6's
                // `anchor_rect`; a wrong shape fails the re-resolve below.
                let table = client.lua().create_table().unwrap();
                table.set("x", rect.x).unwrap();
                table.set("y", rect.y).unwrap();
                table.set("width", rect.width).unwrap();
                table.set("height", rect.height).unwrap();
                write.signal.hover_rect_handle().unwrap().set_changed(mlua::Value::Table(table));
            }
            client.re_resolve_if_dirty();
            if tip_is_up(&client) {
                opened_by += 1;
            }
        }
        assert_eq!(opened_by, 1, "the slot the popup names opens it, and the other one does not");

        // Closing again. `anchor_rect` keeps its last rect rather than clearing, which is what
        // preserves § 6's non-zero rule on the way out.
        let bar = client.scene.surface("bar@TEST").unwrap();
        for write in layout::hover::hover_writes(bar, None) {
            write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(false));
        }
        assert!(client.re_resolve_if_dirty());
        assert!(!tip_is_up(&client), "the pointer left the bar, so the popup closed");
    }

    #[test]
    fn a_surface_whose_visible_reads_a_capability_goes_up_and_down_with_the_payload() {
        // Regression: a card with no `visible` binding sat in the corner saying "no notifications"
        // for ever. The engine contract under it is that a surface's `visible` may be a signal over
        // a capability, and that a snapshot re-resolve moves the surface -- a fixture, so a
        // rearranged dev-config cannot fail an engine test.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r##"
            return { panel {
                id = "notification_area", layer = "Top", anchor = { top = true, right = true },
                width = 200, height = 60,
                -- nil until the first push, as at real boot.
                visible = obelisk.notifications:map(function(n) return n ~= nil and #n.feed > 0 end),
                child = rect { width = 200, height = 60, background = "#111111ff" },
            } }
            "##,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "the fixture must resolve into a scene");

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

        // The Supervisor expires it from the feed (ADR-0033); auto-hide is only this empty list,
        // with no timer here.
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
        // Regression: the last history notification's bottom was clipped because content-sized
        // card height (ADR-0110) was one body-text line short. Position, not content, triggered it:
        // taffy 0.14 adds a container's margin to children's minimum cross size while measuring
        // (`layout::scene::taffy_style` shows the workaround), and the card margin centers it under
        // the indicator. Put the anchor at the bell, seed the session's output, and require every
        // card node to contain its child: card/body/list/cards/rows.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/obelisk/shell.lua");
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
                      "body": [{ "kind": "text", "text": "Obelisk · 1 · Backup" }] },
                    { "id": 8, "app_name": "Telegram Desktop", "summary": "Anas", "timestamp": 1_699_998_000,
                      "body": [{ "kind": "text", "text": "have a look at this: https://github.com/anasgets111/obelisk-shell/pull/12 and tell me what you think." }] }
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
        // Lock screen blankness risks lockout: blind typing hides typos, `pam_unix` delays a wrong
        // password two seconds, and `pam_faillock` locks after three. Use shipped `lock.lua`, not a
        // fixture, so this config's field is the one filling.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/obelisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let lock = client.scene.surface("lock_screen@TEST").expect("the lock screen resolves");
        // Walk into `Transformed`, which is where a scaled subtree keeps its commands. The card
        // enters with a `scale`, so every glyph on it sits one level down and a flat scan of the
        // top-level list reported a lock screen that draws no text at all.
        fn drawn_text(commands: &[layout::paint::DrawCmd], out: &mut Vec<String>) {
            for command in commands {
                match &command.draw {
                    layout::paint::Draw::Text { content, .. } => out.push(content.clone()),
                    layout::paint::Draw::Transformed { commands, .. } => drawn_text(commands, out),
                    _ => {}
                }
            }
        }
        let masked = |focus: Option<&layout::paint::FieldFocus>| -> Vec<String> {
            let mut out = Vec::new();
            drawn_text(&layout::paint::build(lock, 1.0, focus).commands, &mut out);
            out
        };

        let unfocused = masked(None);
        assert!(
            unfocused.iter().any(|drawn| drawn == "Password"),
            "an untouched field shows its placeholder: {unfocused:?}"
        );

        // The pair declared by `lock.lua` and answered by the Supervisor unlock path.
        let target =
            layout::node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() };
        let typed = masked(Some(&layout::paint::FieldFocus::Masked { target: &target, filled: 5 }));
        assert!(
            typed.iter().any(|drawn| drawn == "*****"),
            "five keystrokes must draw five of this config's `mask_character`: {typed:?}"
        );
        assert!(
            !typed.iter().any(|drawn| drawn == "Password"),
            "the placeholder gives way once something is typed: {typed:?}"
        );
    }

    #[test]
    fn the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing() {
        // Regression happened twice and appears only in screenshots: fixed-percentage zones plus a
        // non-shrinking `row` let an overfull zone paint past its right edge. Resolve at 1920x1080
        // (`test_outputs`). Zone sizing is a config choice; two `Fill` spacers around a
        // content-sized center would remove this failure, but
        // `dev-config/obelisk/modules/bar/init.lua`
        // explains why that rewrite waits. Until then, this test guards clipping.
        // Load the bar with near-worst-case snapshots, not boot nils: strings exceed truncation,
        // tray has items, and normally hidden privacy is visible with a camera user.
        let shell_lua = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/obelisk/shell.lua");
        let (mut client, _outbound_rx) = test_client(&shell_lua);
        for (capability, payload) in widest_bar_snapshots() {
            client
                .apply_state_snapshot(StateSnapshot { capability: capability.to_string(), revision: 1, payload })
                .unwrap_or_else(|err| panic!("{capability} snapshot must apply: {err}"));
        }
        assert!(run_startup(&mut client), "the shipped dev config must resolve into a scene");

        let bar = client.scene.surface("bar@TEST").expect("the bar instance must resolve");
        // bar -> padded row -> three zones.
        let zones = &bar.children[0].children;
        assert_eq!(zones.len(), 3, "the bar is three zones (`modules/bar/init.lua`)");

        // Report every zone before asserting, so one overflow cannot hide the next.
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

        // Panel host's other axis: its card is as tall as the panel (ADR-0110), and each body list
        // caps and scrolls. Only the card can run off the output; surface height is the room below
        // the bar. `children[0]` is the root with click-outside catcher then card
        // (`modules/shell/panel_host.lua`), so card paints and hit-tests above it. A closed host is
        // a frozen empty root (ADR-0124), so the card exists only when a kind is shown.
        client
            .lua()
            .load(r#"state("panel_open", false):set(true) state("panel_kind", ""):set("notifications")"#)
            .exec()
            .expect("the dev config declares both panel states");
        assert!(client.re_resolve_if_dirty(), "opening the panel host is a re-resolve");
        let host = client.scene.surface("panel_host@TEST").expect("the panel host must resolve");
        let card = &host.children[0].children[1];
        let card_bottom = card.rect.y + card.rect.height;
        assert!(
            card_bottom <= host.rect.height,
            "the panel card ends {card_bottom:.0}px down a {:.0}px surface; it runs off the output",
            host.rect.height
        );
        let widest = card.children.iter().map(|section| section.rect.width).fold(0.0_f32, f32::max);
        // Same derivation: hard-coded 24 assumed `spacing.md` was 12px, but responsive scale makes
        // it 11px, comparing a `Fill` section with a card two pixels too narrow.
        let content_width = card.rect.width - 2.0 * card.children.first().map_or(0.0, |first| first.rect.x);
        assert!(
            widest <= content_width,
            "a bar panel is {widest:.0}px in {content_width:.0}px of card; it will paint past the edge"
        );
    }

    #[test]
    fn every_rostered_capability_is_on_the_obelisk_table_and_reads_nil_before_its_first_snapshot() {
        // ADR-0037: every rostered capability is `obelisk.<name>`, a live signal reading nil at
        // boot, never an index-into-nil rescue error.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::Capability::ALL.iter().map(|c| c.as_str()) {
            let setup = format!("is_nil = obelisk.{capability}:get() == nil");
            assert!(
                probe::<bool>(&client.loader, &setup, "is_nil"),
                "obelisk.{capability} should read nil before its first snapshot"
            );
        }
    }

    #[test]
    fn no_rostered_capability_is_left_as_a_bare_global() {
        // `set_global` never removes a bare seed; it would work until colliding with a node
        // constructor as `lock` did (ADR-0052 decision 1).
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::Capability::ALL.iter().map(|c| c.as_str()) {
            // `lock` is § 6's legitimate node constructor.
            if capability == "lock" {
                continue;
            }
            let setup = format!("is_nil = {capability} == nil");
            assert!(
                probe::<bool>(&client.loader, &setup, "is_nil"),
                "{capability} is still a bare global; § 2 names it obelisk.{capability}"
            );
        }
    }

    #[test]
    fn an_unrostered_push_refuses_to_replace_a_name_the_obelisk_table_already_holds() {
        // `rescue` reports config failures; replacing it with an empty capability hides the shell's
        // breakage, as in ADR-0052 decision 1's `lock` bug.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let snapshot = StateSnapshot { capability: "rescue".to_string(), revision: 1, payload: serde_json::json!({}) };
        let err = client.apply_state_snapshot(snapshot).unwrap_err().to_string();
        assert!(err.contains("already something else"), "the refusal must say why: {err}");

        // Real `rescue` still reads its own table, not an empty capability.
        assert!(probe::<bool>(&client.loader, "intact = obelisk.rescue:get().is_rescue == false", "intact"));
    }

    #[test]
    fn rescue_and_screens_moved_onto_the_same_table_as_the_roster() {
        // § 2.10 and § 2.15 name both `obelisk.*`, like capabilities.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let setup = r#"
            rescued = obelisk.rescue:get().is_rescue
            screen_count = #obelisk.screens:get()
            bare_rescue_gone = rescue == nil
            bare_screens_gone = screens == nil
        "#;
        assert!(!probe::<bool>(&client.loader, setup, "rescued"));
        assert_eq!(probe::<i64>(&client.loader, setup, "screen_count"), 0);
        assert!(probe::<bool>(&client.loader, setup, "bare_rescue_gone"));
        assert!(probe::<bool>(&client.loader, setup, "bare_screens_gone"));
    }

    #[test]
    fn obelisk_version_is_three_integers_a_config_can_compare() {
        // Shape matters: configs compare `obelisk.version.major > 0` or `minor >= 2`, so all three
        // fields must be numeric.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let setup = "major, minor, patch = obelisk.version.major, obelisk.version.minor, obelisk.version.patch";
        // Read through Lua against Cargo's string, not a second builder call: assert what config
        // sees. This also reaches `lua::namespace::version_parts`'s `expect`, so it needs no
        // narrower test.
        let field = |name: &str| probe::<i64>(&client.loader, setup, name);
        assert_eq!(format!("{}.{}.{}", field("major"), field("minor"), field("patch")), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn obelisk_config_dir_is_the_directory_shell_lua_was_loaded_from() {
        // Derive from the loaded path, not a fresh resolution, so explicit `shell.lua` cannot
        // report an unread directory.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/opt/obelisk-config/shell.lua"));
        let dir = probe::<String>(&client.loader, "dir = obelisk.config_dir", "dir");
        assert_eq!(dir, "/opt/obelisk-config");
    }

    /// `RendererClient::new` seeds after `Loader::new` registers § 6 constructors. A bare `lock`
    /// seed would silently overwrite the constructor; `set` over an existing global is silent, so
    /// `lock { ... }` would report "attempt to call a userdata value" against the config rather
    /// than the seed. Assert after a full generation, because that registration order is the
    /// contract.
    #[test]
    fn a_full_generation_keeps_lock_as_the_node_constructor_and_puts_the_capability_on_obelisk() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r##"return {
                panel { id = "bar", layer = "Top" },
                lock {
                    id = "screen",
                    child = text { content = obelisk.lock:map(function(s) return (s and s.error) or "" end) },
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

        // The root `lock { ... }` still produced a § 6 surface.
        let setup = r#"
            lock_kind = lock { id = "screen" }.kind
            capability_type = type(obelisk.lock)
            attempts = obelisk.lock:get().attempts
        "#;
        assert_eq!(
            probe::<String>(&client.loader, setup, "lock_kind"),
            "lock",
            "the global `lock` must still be § 6.4's node constructor"
        );
        // The § 2 capability name is reachable and hydrated.
        assert_eq!(probe::<String>(&client.loader, setup, "capability_type"), "userdata");
        assert_eq!(
            probe::<i64>(&client.loader, setup, "attempts"),
            2,
            "the `lock` StateSnapshot must reach `obelisk.lock`, not a bare global nothing registered"
        );
    }

    #[test]
    fn a_declared_store_opens_the_file_the_config_named_and_nothing_else() {
        // ADR-0136: path, file name, and defaults belong to config; the envelope must carry exactly
        // `shell.lua`'s values, not a directory chosen here.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client
            .loader
            .lua()
            .load(r#"store = persistent_table { path = "/tmp/bar/", name = "settings.json", defaults = { theme = "mocha" } }"#)
            .exec()
            .unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("declaring a store must queue a RendererFrame::Command");
        };
        assert_eq!(envelope.params.capability, "storage");
        assert_eq!(envelope.params.action, "open");
        assert_eq!(
            envelope.params.arguments,
            vec![serde_json::json!("/tmp/bar/settings.json"), serde_json::json!({ "theme": "mocha" })],
            "the joined path is the key both sides use, so it is built once, here"
        );
    }

    #[test]
    fn a_stores_key_reads_the_value_the_supervisor_pushed_for_that_file() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        client
            .loader
            .lua()
            .load(r#"store = persistent_table { path = "/tmp/bar", name = "state.json" }"#)
            .exec()
            .unwrap();

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "storage".to_string(),
                revision: 1,
                payload: serde_json::json!({
                    "files": { "/tmp/bar/state.json": { "theme": "latte", "wallpaper": { "fit": "cover" } } }
                }),
            })
            .unwrap();

        let setup = r#"
            theme = store.theme:get()
            fit = store.wallpaper:get().fit
            unset = store.nothing_here:get()
        "#;
        assert_eq!(probe::<String>(&client.loader, setup, "theme"), "latte");
        assert_eq!(probe::<String>(&client.loader, setup, "fit"), "cover", "a table value is stored whole");
        assert_eq!(
            probe::<Option<String>>(&client.loader, setup, "unset"),
            None,
            "a key the file does not have reads nil, so a property falls back to its documented default"
        );
    }

    #[test]
    fn a_stores_write_names_the_same_file_the_declaration_did() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);
        client
            .loader
            .lua()
            .load(r#"store = persistent_table { path = "/tmp/bar", name = "state.json" }"#)
            .exec()
            .unwrap();
        let _open = queued_frame(&mut outbound_rx);

        client.loader.lua().load(r#"store:set("theme", "latte")"#).exec().unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("a store write must be queued as RendererFrame::Command");
        };
        assert_eq!(envelope.params.action, "set");
        assert_eq!(
            envelope.params.arguments,
            vec![serde_json::json!("/tmp/bar/state.json"), serde_json::json!("theme"), serde_json::json!("latte")]
        );
    }

    #[test]
    fn two_declarations_of_one_file_are_one_table() {
        // A module and `shell.lua` may redeclare a store, and in-place reload reruns all
        // declarations:
        // all three executions land on one table and signal per key, or one `:set()` leaves the
        // other stale.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let setup = r#"
            first = persistent_table { path = "/tmp/bar", name = "state.json" }
            second = persistent_table { path = "/tmp/bar/", name = "state.json" }
            same = rawequal(first, second)
        "#;
        assert!(probe::<bool>(&client.loader, setup, "same"), "the joined path is the identity");
    }

    #[test]
    fn a_declared_session_process_declares_the_name_and_the_stop_signal_the_config_chose() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client
            .loader
            .lua()
            .load(r#"rec = session_process { name = "screen-recorder", stop_signal = "INT" }"#)
            .exec()
            .unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("declaring a session process must queue a RendererFrame::Command");
        };
        assert_eq!(envelope.params.capability, "processes");
        assert_eq!(envelope.params.action, "declare");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!("screen-recorder"), serde_json::json!("INT")]);
    }

    #[test]
    fn a_session_processs_fields_read_what_the_supervisor_pushed_for_that_name() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        client.loader.lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "processes".to_string(),
                revision: 1,
                payload: serde_json::json!({
                    "sessions": {
                        "recorder": { "running": true, "pid": 4321, "started_at": 1700000000,
                                      "exit_code": null, "start_error": "" }
                    }
                }),
            })
            .unwrap();

        let setup = r#"
            running = rec.running:get()
            pid = rec.pid:get()
            started = rec.started_at:get()
            code = rec.exit_code:get()
        "#;
        assert!(probe::<bool>(&client.loader, setup, "running"));
        assert_eq!(probe::<i64>(&client.loader, setup, "pid"), 4321);
        assert_eq!(probe::<i64>(&client.loader, setup, "started"), 1_700_000_000);
        assert_eq!(
            probe::<Option<i64>>(&client.loader, setup, "code"),
            None,
            "a run still going has no exit status, and nil is what a config must be able to test"
        );
    }

    #[test]
    fn an_undeclared_names_fields_read_nil_rather_than_a_stopped_program() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        client.loader.lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "processes".to_string(),
                revision: 1,
                payload: serde_json::json!({ "sessions": {} }),
            })
            .unwrap();

        assert_eq!(
            probe::<Option<bool>>(&client.loader, "running = rec.running:get()", "running"),
            None,
            "before the Supervisor has answered, `running` is unknown rather than false"
        );
    }

    #[test]
    fn a_session_processs_methods_name_the_program_the_declaration_did() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);
        client.loader.lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();
        let _declare = queued_frame(&mut outbound_rx);

        client
            .loader
            .lua()
            .load(
                r#"
                rec:start("gpu-screen-recorder", { "-w", "DP-1" })
                rec:signal("USR2")
                rec:stop()
            "#,
            )
            .exec()
            .unwrap();

        let actions: Vec<(String, Vec<serde_json::Value>)> = (0..3)
            .map(|_| match queued_frame(&mut outbound_rx) {
                RendererFrame::Command(envelope) => (envelope.params.action, envelope.params.arguments),
                other => panic!("a session-process method must queue a Command, got {other:?}"),
            })
            .collect();
        assert_eq!(actions[0].0, "start");
        assert_eq!(
            actions[0].1,
            vec![
                serde_json::json!("recorder"),
                serde_json::json!("gpu-screen-recorder"),
                serde_json::json!(["-w", "DP-1"])
            ]
        );
        assert_eq!(actions[1], ("signal".to_string(), vec![serde_json::json!("recorder"), serde_json::json!("USR2")]));
        assert_eq!(actions[2], ("stop".to_string(), vec![serde_json::json!("recorder")]));
    }

    #[test]
    fn two_declarations_of_one_program_are_one_handle() {
        // The same reason `persistent_table` caches: a module and `shell.lua` may both declare it,
        // and in-place reload reruns both. Two handles would mean two sets of field signals, one of
        // them stale.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let setup = r#"
            first = session_process { name = "recorder" }
            second = session_process { name = "recorder", stop_signal = "INT" }
            same = rawequal(first, second)
        "#;
        assert!(probe::<bool>(&client.loader, setup, "same"), "the declared name is the identity");
    }

    /// A config `on_click` invoking lock queues a real § 7 envelope.
    #[test]
    fn a_config_calling_the_lock_action_queues_a_command_for_the_supervisor() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, mut outbound_rx) = test_client(&missing);

        client.loader.lua().load(r#"obelisk.lock:invoke("lock")"#).exec().unwrap();

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
            !probe::<bool>(&client.loader, "scanning = obelisk.network:get().scanning", "scanning"),
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

    /// A `window` and `popup` alongside panels.
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
        // All roles matter: `expand_instances` and `create_surfaces` branch on the variant; an
        // untagged role could bind only as a layer surface.
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
        // ADR-0049 decision 3: a window object may come and go within a generation, but its
        // *declaration* is fixed, so adding one changes topology.
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

    /// Lock screen whose `child` holds § 6's `secure_submit` field.
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
        // `SurfaceFingerprint::Lock` carries only `id`, so edits inside the lock are `Unchanged`
        // and bypass the generation-swap gate. Restyles must land (ADR-0052 decision 2), but
        // removing the way out must be refused; `Scene::apply` rollback preserves the live tree.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        // Restyle: same surfaces and field, different colour.
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

        // Must refuse: same lock, no `textfield`.
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
        // With no lock held, the lock screen may become empty like any surface.
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
        // The veto used to store `lock` instance ids from `LockCommand::Acquire`. Each hotplug's
        // `handle_output_change` replaces instances without revisiting that list; a lid closing on
        // a dock left the veto checking an unpaintable fossil while the live lock screen was open.
        // This apply mirrors `handle_output_change`: re-expand applied specs against current
        // outputs, store, resolve.
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
        // `expand_instances` makes one instance per lock spec/output. Two declarations make
        // `ensure_lock_surfaces` send two `get_lock_surface`s for one `wl_output`;
        // `ext-session-lock-v1` calls that `duplicate_output`, and the compositor kills the
        // connection after lock, leaving only a VT switch. Both entry points reject it: startup
        // returns no specs and `Reevaluate` reports `Failed`, never staging it.
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
        // `xdg-shell.xml` permits `set_title`/`set_app_id` after mapping, so a title change must
        // not respawn the process.
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
            r#"return window { id = "settings", title = "Obelisk settings", app_id = "obelisk.settings" }"#,
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
        // § 6's `anchor_rect` feeds `xdg_positioner::set_anchor_rect`; zero size leaves it
        // incomplete, so `get_popup` raises `invalid_positioner` and kills Wayland. A config typo
        // must instead be an evaluation `LayoutError` in `rescue.error_log`, naming the property.
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
        // Likewise, `set_max_size` raises `invalid_size` when max < min; validate at evaluation so
        // the config author sees it.
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
        // Treating "nothing applied yet" as empty topology instead of "no prior state" made every
        // later evaluation `TopologyChanged`, which nothing applies, leaving the shell blank.
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
        // Seed a *different* topology so fresh evaluation reads as changed.
        client.state.applied_topology = Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology {
            id: "other".to_string(),
            layer: LayerKind::Top,
            anchor: Default::default(),
            monitor: "All".to_string(),
            namespace: "obelisk-other".to_string(),
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
        // ADR-0038 decision 2: layer-shell accepts `margin`, exclusive zone,
        // `keyboard_interactivity`, and size on a live surface. Comparing whole specs would swap
        // generations for each, respawning just to move a bar 4px.
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

        // Change every in-place field; leave topology unchanged.
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
        // `get_layer_surface` fixes namespace at creation; no live request changes it, so namespace
        // edits need a new surface and generation.
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
    fn a_measured_axis_keeps_its_ceiling_when_the_compositor_configures_the_size_it_asked_for() {
        // The conflation this guards against: `available` is the box a tree is solved against, and
        // on a measured axis that box is a *ceiling*, not an allocation. Writing the granted size
        // back would make the measurement its own cap.
        //
        // It only bites where natural size depends on the bound, which is exactly what wrapping
        // does. Measured against 1000 this paragraph is one line wide; against 180 it is 180 wide
        // and two lines tall. Grant it the 342 it asked for, feed that back as the ceiling, and it
        // can never grow wider again -- it wraps taller inside the width it happened to open at.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "startup must have applied");

        let ceiling = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap().available;
        client.set_measured_axes("bar@TEST", (true, false), ceiling);
        client.dirty.take();

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 342.0, height: 32.0 });

        let after = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap().available;
        assert_eq!(after.width, ceiling.width, "the measured axis keeps the ceiling it was seeded with");
        assert_eq!(after.height, 32.0, "the allocated axis takes the configured size, as before");
    }

    #[test]
    fn an_axis_that_becomes_measured_is_given_a_ceiling_instead_of_keeping_its_last_allocation() {
        // A reload may drop `width = 200` and leave the axis measured. Nothing else can set
        // `available` there afterwards -- `set_instance_size` declines a measured axis outright --
        // so without this the content would be solved against 200 for the rest of the generation
        // and no configure could ever widen it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "startup must have applied");
        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 200.0, height: 39.0 });
        client.dirty.take();

        let ceiling = layout::LogicalSize { width: 1920.0, height: 1080.0 };
        client.set_measured_axes("bar@TEST", (true, false), ceiling);

        let instance = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap();
        assert_eq!(instance.measured_axes, (true, false));
        assert_eq!(instance.available.width, 1920.0, "the newly measured axis takes the ceiling it may grow into");
        assert_eq!(instance.available.height, 39.0, "the allocated axis keeps what the compositor configured");
        assert!(client.dirty.take(), "the tree must be solved again against the room it actually has");
    }

    #[test]
    fn recording_the_same_measurement_again_changes_nothing_and_names_no_instance_it_lacks() {
        // It runs on every resolved pass, so a pass that re-derived the same panel spec must not
        // cost a re-resolve of every surface in the scene.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "startup must have applied");
        assert!(!client.dirty.take(), "a clean startup leaves the flag clear");
        let ceiling = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap().available;

        client.set_measured_axes("bar@TEST", (false, true), ceiling);
        assert!(!client.dirty.take(), "the ceiling it already had is not a change to any tree");

        client.set_measured_axes("no-such-surface@TEST", (true, true), ceiling);
        assert!(!client.dirty.take(), "an unknown id is ignored, the way an unknown configure is");
    }

    #[test]
    fn set_instance_size_replaces_one_instances_available_size_and_marks_the_scene_dirty() {
        // `configure` reuses ADR-0044 decision 2's one dirty flag, not a second change mechanism.
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
        // `monitor = "All"` across laptop and 4K external means two configured sizes; one declared
        // surface tree cannot serve both.
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
        // A topology-field type error such as `anchor.top` not boolean used to become
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
        // The poll loop repaints only when `re_resolve_if_dirty` reports change.
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

    // ADR-0044 decision 2: a `StateSnapshot` dirties the scene; dirty re-resolve uses the last
    // applied evaluation without `shell.lua`. `workspace` is outside `shared::Capability::ALL`, so
    // push it before `run_startup_evaluation` for a bare (not `:get()`) reference to evaluate.

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
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = obelisk.workspace }"#);
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

        // Break the file: re-resolve must read the retained tree's live signal, never disk.
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
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = obelisk.workspace }"#);
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

        // Replace `applied_output` directly, bypassing the dirtying push path, with
        // `visible = true`.
        // A true no-op leaves the scene as the first resolve left it.
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
        // ADR-0044 decision 2: "drain first, then re-resolve once"; several pushes before one read
        // coalesce into one dirty read.
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
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = obelisk.workspace }"#);
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

        // `visible` requires boolean; a table makes re-resolve fail.
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
        // ADR-0044 decision 1: startup runs before an inbound frame drains, so rostered
        // capabilities read `nil`; bare bindings still apply using each parser's absent-property
        // default.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", visible = obelisk.audio, child = rect { width = obelisk.network, height = 10, children = obelisk.tray } }"#,
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
        // ADR-0044 example: `text { content = obelisk.mpris.title }` applies at boot with `title`
        // still nil, like `visible`/`children` above.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = obelisk.audio } }"#,
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
        // Check `applied_output` before taking the flag; otherwise a push with nothing to resolve
        // against is silently discarded.
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
    fn repeated_re_resolves_remove_nodes_and_preserve_the_remaining_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            return panel { id = "bar", layer = "Top", child = row { children = computed({obelisk.audio}, function(n)
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

        let first_id = client.scene.surface("bar@TEST").unwrap().children[0].children[0].id;
        for revision in 1..=20 {
            let count = if revision % 2 == 0 { 1 } else { 3 };
            client
                .apply_state_snapshot(StateSnapshot {
                    capability: "audio".to_string(),
                    revision,
                    payload: serde_json::json!(count),
                })
                .unwrap();
            assert!(client.re_resolve_if_dirty());
            let root = client.scene.surface("bar@TEST").unwrap();
            assert_eq!(root.children[0].children.len(), count);
            assert_eq!(root.children[0].children[0].id, first_id);
            assert_eq!(client.scene.census().1, count + 2);
        }

        assert_eq!(
            client.scene.surface("bar@TEST").unwrap().children[0].children.len(),
            1,
            "the last push shrank the row back to one child"
        );
    }

    #[test]
    fn a_clean_startup_leaves_the_scene_flag_clear() {
        // `set_rescue_state` writes the shared `DirtyFlag`; startup used to dirty it while clearing
        // already-clear rescue, costing a redundant first-poll `Scene::apply`.
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

    /// `screens_payload` shape, hand-written so tests need no live compositor via `crate::wayland`.
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
        // ADR-0041 decision 1: no `variants`; Lua already has `for`. Without the pre-evaluation
        // seed, this loop would run zero times.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            local panels = {}
            for _, screen in ipairs(obelisk.screens:get()) do
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
        // `nil` would make `ipairs` error and put an innocent config in rescue.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = "screens: " .. #obelisk.screens:get() } }"#,
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
        // `update_output` handles changes absent from `screens`; an unchanged push buys neither
        // `Scene::apply` nor a Supervisor round trip.
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
            r#"return panel { id = "bar", layer = "Top", child = text { content = computed({obelisk.screens}, function(list) return "n=" .. #list end) } }"#,
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
        // The seed marks the flag, but the immediate apply resolves that value; entering the poll
        // loop dirty would add one redundant `Scene::apply` before drawing.
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
        // Hotplug expansion source (ADR-0038 decision 3). Delete the file to prove it is untouched.
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
        // Failed startup declared nothing, so hotplug adds no instance.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/no/such/shell.lua"));
        assert!(client.applied_surface_specs().is_empty());
    }

    #[test]
    fn request_reload_queues_the_frame_the_supervisor_starts_a_cycle_from() {
        // ADR-0041 decision 4: `is_current_reload` would drop sequences the Supervisor did not
        // send.
        let (client, mut outbound_rx) = test_client(std::path::Path::new("/no/such/shell.lua"));
        client.request_reload();
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::RequestReload);
    }

    #[test]
    fn a_topology_changed_reevaluate_leaves_the_scene_flag_clear() {
        // `TopologyChanged` leaves this scene alone; a generation swap owns it. A no-op
        // `set_rescue_state(false, "")` used to leave dirty set, so the next poll re-applied
        // `applied_output` to a scene that must not change.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology {
            id: "other".to_string(),
            layer: LayerKind::Top,
            anchor: Default::default(),
            monitor: "All".to_string(),
            namespace: "obelisk-other".to_string(),
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
        // Different applied topology proves dispatch/queue, not `handle_reevaluate` classification.
        client.state.applied_topology = Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology {
            id: "other".to_string(),
            layer: LayerKind::Top,
            anchor: Default::default(),
            monitor: "All".to_string(),
            namespace: "obelisk-other".to_string(),
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
        // Drawing needs `wayland::App`'s EGL/surface state, so return the nonce to
        // `App::activate_draw`.
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
        // Both directions matter: `locked = true` reaches Wayland for refusal without a `lock`
        // surface (ADR-0052 decision 3); `locked = false` is the only unlock path (ADR-0042).
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
        // A third recognized frame proves dispatch still works; the exact verdict is irrelevant.
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
        // Register real callbacks through `process.run`; fresh registry id 0 is what inbound frames
        // address.
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

    /// Queues `frame`, returns what [`pump`] wrote. The read half produces nothing here, so timeout
    /// bounds the test.
    async fn pumped_to_the_wire(frame: RendererFrame) -> RendererFrame {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel(INBOUND_CAPACITY);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        outbound_tx.send(frame).unwrap();

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx, None);
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
        // ADR-0005/ADR-0027: wire carries the exact `SecureBuffer` secret; `pump` zeroizes its
        // plaintext frame copy immediately after write.
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

        // Close the write half so `pump`'s *second* read hits EOF after forwarding one frame.
        write_json_frame(&mut wire, &SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })).await.unwrap();
        wire.shutdown().await.unwrap();

        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(INBOUND_CAPACITY);
        let (_outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx, None).await;

        assert_eq!(inbound_rx.try_recv(), Ok(SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })));
    }

    /// Advances `pumping` for up to `millis`. If `pump` completes first, a loop broke, which is a
    /// test bug, not a normal yield.
    async fn let_pump_advance(mut pumping: std::pin::Pin<&mut impl std::future::Future<Output = ()>>, millis: u64) {
        tokio::select! {
            () = &mut pumping => unreachable!("pump must not return on its own in this test"),
            () = tokio::time::sleep(std::time::Duration::from_millis(millis)) => {}
        }
    }

    /// `shared::framing::read_frame` does two sequential `read_exact`s, so partial progress lives
    /// in its future. Old `pump` raced one `read_json_frame` against `outbound_rx.recv()` per
    /// `select!`; outbound could drop a stalled read, losing consumed bytes, and the next iteration
    /// read a length prefix from the middle of JSON. This reproduces the race with an inbound frame
    /// split across writes and outbound activity between them. Fixed `pump` gives each direction a
    /// long-lived loop, so the stalled read survives.
    #[tokio::test]
    async fn pump_survives_an_inbound_frame_split_around_an_outbound_frame() {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(INBOUND_CAPACITY);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        let inbound_frame = SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 });
        let payload = serde_json::to_vec(&inbound_frame).unwrap();
        let mut wire_bytes = (payload.len() as u32).to_be_bytes().to_vec();
        wire_bytes.extend_from_slice(&payload);
        // Split past the length prefix, stalling the *second* `read_exact`.
        let split_at = wire_bytes.len() / 2;
        assert!(split_at > 4, "the split point must land inside the payload, not the length prefix");

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx, None);
        tokio::pin!(pumping);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            wire.write_all(&wire_bytes[..split_at]).await.unwrap();
            // Consume the partial payload and block mid-`read_exact`.
            let_pump_advance(pumping.as_mut(), 20).await;

            // Queue outbound while inbound is stalled mid-frame, the old `select!` race.
            outbound_tx
                .send(RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }))
                .unwrap();
            let_pump_advance(pumping.as_mut(), 20).await;

            // A stuck read must not starve writes.
            let written = read_json_frame::<_, RendererFrame>(&mut wire).await.unwrap();
            assert_eq!(written, RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }));

            // Complete inbound. If outbound cancelled the read, this prefix would land mid-payload.
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
