//! Renderer-side Unix control-socket client (build-steps.md Phase 9) and the `SupervisorFrame`
//! handling hanging off it.
//!
//! Connects to `$XDG_RUNTIME_DIR/oblisk-shell.sock` as a client; the Supervisor is the
//! listener (`supervisor/src/socket.rs`) -- Phase 11's text reuses this same direction
//! ("The Supervisor is the listener, the Renderer connects as client"), not inverted. Sends a
//! `shared::ConnectionHandshake` as the first frame, then holds the connection open.
//!
//! Two threads, two channels (docs/adr/0039, build-steps.md Phase 18). The socket thread runs a
//! dedicated current-thread tokio runtime -- same reasoning as `text::shaping`'s dedicated worker
//! thread (see that module's doc comment), and exactly what `renderer/Cargo.toml`'s `rt`/`net`/
//! `macros` tokio features were staged for -- and does nothing but framed I/O: [`pump`] forwards
//! every inbound `SupervisorFrame` to the Wayland dispatch thread over a `std::sync::mpsc`
//! channel, and writes every outbound `RendererFrame` it receives over a `tokio::sync::mpsc`
//! channel out to the wire.
//!
//! Everything that touches Lua state -- [`RendererClient`], its [`Loader`], the retained
//! [`Scene`], the live-signal map, the rescue state, the `process` registry -- lives on the
//! *Wayland* thread instead. `mlua::Lua` is `!Send`, and the paint pass that will eventually
//! consume the retained scene has to own the GL context, so the scene and the VM belong on the
//! thread that dispatches Wayland events (docs/adr/0039). `crate::wayland::run` therefore calls
//! [`RendererClient::start`] on its own thread and drives [`RendererClient::handle_frame`] from
//! its poll loop.
//!
//! Real `shell.lua` reload flow (build-steps.md Phase 13; `CONTEXT.md`, Watcher/Rollback/
//! In-place reload/Generation swap; docs/adr/0024): a `shared::StateSnapshot` push only
//! hydrates that snapshot's own `capability`-named live signal now (docs/adr/0029; created
//! lazily on first sight of a new capability name, see `apply_state_snapshot`) -- it no longer
//! triggers any Lua evaluation, unlike Phase 11's proof-of-wiring hack. Evaluation is driven by
//! the Supervisor's own
//! `shared::SupervisorFrame::Reevaluate`, sent after its `inotify` watch on
//! `~/.config/oblisk/` detects a debounced edit to `shell.lua`:
//!
//! 1. On `Reevaluate`, [`RendererClient::handle_reevaluate`] reads and evaluates the real
//!    `shell.lua` file ([`crate::lua::Loader::evaluate_file`]) *without* applying it to the
//!    retained [`Scene`] yet, diffs the evaluation's topology
//!    (`crate::layout::node::SurfaceTopology`) against whatever's currently applied, and reports
//!    back a `shared::ReevaluateReport::Unchanged`, `TopologyChanged`, or `Failed` verdict -- the
//!    Supervisor (CONTEXT.md's Watcher) owns what happens next, not this module.
//! 2. `Unchanged` evaluations are kept as `pending`, applied to the `Scene` only once the
//!    Supervisor sends back `ApplyPendingReload` for that same sequence -- never eagerly,
//!    since a `TopologyChanged` verdict must leave this generation's own scene untouched (that
//!    case is a generation swap, a different generation's job, Phase 14).
//! 3. `applied_topology` is `None` until an evaluation is actually applied (nothing yet, or the
//!    prior applied evaluation was superseded by rescue -- see below). `handle_reevaluate` treats
//!    `None` as "safe to apply", not as an empty topology to diff against: after a startup
//!    failure there's nothing to protect, so the next successful evaluation -- whether it's the
//!    file the user just fixed, or the same one retried -- must be able to recover, not be
//!    permanently misclassified as `TopologyChanged` (which nothing here ever applies).
//! 4. An evaluation failure sets the ad-hoc `rescue` global's `is_rescue`/`error_log` fields
//!    (mirrors the ad-hoc `audio` global, ADR-0022's precedent -- not the full `oblisk.*`
//!    signal tree) and leaves the prior applied scene untouched (`CONTEXT.md`'s Rollback).
//!
//! Deliberately deferred: real generation-ID assignment tied to process spawning (a later phase,
//! once Phase 7/8's spawn primitives are wired to this transport) -- for now the generation ID
//! comes from the `OBLISK_GENERATION_ID` env var, defaulting to `0`; reconnection if the
//! connection drops (mirrors `supervisor/src/socket.rs`'s own "no accept-loop restart policy"
//! ceiling, same reasoning, symmetric on this side).
//!
//! Real PBA handshake wiring (build-steps.md Phase 14, § 15.2-15.3, closing docs/adr/0019 items
//! 1/3/6; Phase 15 item 2 adds `SecureSubmit`): `ReadySignal`, `PresentationEvidence` and
//! `SecureSubmit` are all built by `crate::wayland::App` itself, at the point that actually knows
//! them, and reach the wire as ordinary outbound frames. `ActivateDraw` is the one inbound frame
//! [`RendererClient::handle_frame`] can't service on its own -- drawing needs the EGL/surface
//! state -- so it hands the nonce straight back to the Wayland poll loop.
//! `DeselectInput`/`PromoteGeneration` are real, received, and currently logged only (no real
//! input-region/focus machinery exists yet to hand them to -- docs/adr/0025 item 4).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::framing::{self, write_json_frame};
use shared::{
    ApplyPendingReload, ConnectionHandshake, DeselectInput, IdleEvent, ProcessExited, ProcessOutputLine, PromoteGeneration, ReevaluateReport,
    ReevaluateRequest, RendererFrame, StateSnapshot, SupervisorFrame, Zeroize,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::layout::node::SurfaceTopology;
use crate::layout::{self, Scene};
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::LiveSignalHandle;
use crate::lua::{self, Loader};
use crate::text::shaping::ShapingHandle;

/// Every surface still resolves against one hardcoded size instead of its own configured one
/// (docs/adr/0023 item 6, docs/adr/0039 decision 4).
///
/// ponytail: the sizes are reachable now, but not yet attributable. ADR-0039 decision 4 assumed
/// "on the same thread" was the whole blocker; it is not. `Scene` keys surfaces by the `id` a
/// config writes (`dev-config/oblisk/shell.lua` says `"bar"`), while `crate::wayland` hardcodes
/// `TrackedSurface::surface_id` from `SurfaceRole::label()` (`"main_bar"`, `"overlay_canvas"`,
/// `"wallpaper_layer@{output}"`). Those two id spaces have no overlap at all, so there is no
/// surface whose configured size a lookup could find, and any fallback for the misses would be a
/// mapping policy invented here and deleted by ADR-0038. Phase 20 item 4 moves the default
/// surfaces into Lua and deletes `SurfaceRole`, which is what makes the ids one space; the
/// per-surface size threads in there, against real correspondence rather than a guess.
const PLACEHOLDER_OUTPUT_SIZE: layout::LogicalSize = layout::LogicalSize { width: 1920.0, height: 40.0 };

/// This Renderer's own generation id (`OBLISK_GENERATION_ID`, defaulting to `0`). Read once in
/// `main`, then handed to both threads -- the socket thread stamps it into the handshake, the
/// Wayland thread stamps it into every outbound `CommandEnvelope` and `SecureSubmit`.
pub fn generation_id_from_env() -> u32 {
    std::env::var("OBLISK_GENERATION_ID").ok().and_then(|value| value.parse().ok()).unwrap_or(0)
}

/// Connects to `path` and sends the handshake identifying `generation_id`, returning the
/// live stream on success.
async fn connect_and_handshake(path: &Path, generation_id: u32) -> Result<UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = UnixStream::connect(path).await?;
    write_json_frame(&mut stream, &ConnectionHandshake { generation_id }).await?;
    Ok(stream)
}

/// Spawns the dedicated connect-and-hold-open thread. Connection failures (no Supervisor
/// listening yet, wrong path) are logged, not fatal -- build-steps.md doesn't yet define a
/// startup-ordering guarantee between the two processes; the Wayland thread keeps running with
/// nothing on the other end of its two channels.
pub fn spawn_client(generation_id: u32, inbound_tx: std::sync::mpsc::Sender<SupervisorFrame>, outbound_rx: mpsc::UnboundedReceiver<RendererFrame>) {
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

/// One connection's reload bookkeeping. `applied_topology` is `None` until an evaluation is
/// actually applied to the `Scene` -- see the module doc comment point 3 for why that's not the
/// same thing as an empty topology. `pending` holds the evaluated-but-not-yet-applied output
/// (and its already-computed topology, so `handle_apply_pending` doesn't need to recompute it)
/// between a `Reevaluate` that reported `Unchanged` and its matching `ApplyPendingReload`.
struct ReloadState {
    applied_topology: Option<Vec<SurfaceTopology>>,
    pending: Option<(u64, lua::LoadOutput, Vec<SurfaceTopology>)>,
}

/// One generation's whole Lua side: the VM, the retained scene, the live signals, and the
/// reload bookkeeping, grouped so they travel as one receiver instead of the same 6-7 pieces
/// threaded through every function's parameter list separately (Standards review, docs/adr/0024).
///
/// `!Send`, and deliberately so: `crate::wayland::App` owns one of these directly (docs/adr/0039),
/// so a Lua closure, a scene reconcile, and the EGL context are all reachable from one another
/// without a channel hop.
pub struct RendererClient {
    loader: Loader,
    shell_lua_path: PathBuf,
    scene: Scene,
    /// A clone of the one `ShapingHandle` `crate::wayland::App` also holds -- one worker thread
    /// and one `FontSystem` for the whole process (docs/adr/0023 item 8, closed by docs/adr/0039
    /// decision 3), instead of the second `FontSystem::new()`'s ~1s startup this used to pay.
    shaping: ShapingHandle,
    /// One live Lua signal per capability seen so far, keyed by `StateSnapshot.capability`
    /// (docs/adr/0029) -- every `shared::CAPABILITIES` roster name is seeded at construction
    /// (ADR-0037; see `new`'s doc comment); an unrostered capability's global is registered
    /// lazily, on the first `StateSnapshot` that names it, by `apply_state_snapshot`. `RefCell`,
    /// not `&mut self`: `apply_state_snapshot` is called through a `&self` receiver (see its own
    /// doc comment for why), and this is the one piece of `RendererClient` state that read path
    /// needs to mutate.
    capability_signals: RefCell<HashMap<String, LiveSignalHandle>>,
    rescue_handle: LiveSignalHandle,
    process_registry: ProcessRegistry,
    state: ReloadState,
    /// Where a `ReevaluateReport` goes: the socket thread's [`pump`] drains this and writes each
    /// frame to the wire. `UnboundedSender::send` is synchronous and non-blocking, so this is
    /// callable straight from the Wayland dispatch thread.
    outbound_tx: mpsc::UnboundedSender<RendererFrame>,
}

impl RendererClient {
    /// Builds one generation's entire Lua side on the calling thread: the VM, the rescue signal,
    /// the `process` global's registry, and the capability roster's seeded signals. Called from
    /// `crate::wayland::run`, never from the socket thread -- `mlua::Lua` is `!Send`, so it has to
    /// be constructed on the thread that will run it (docs/adr/0039).
    ///
    /// Every failure here is fatal to the process rather than logged-and-survived: a Renderer with
    /// no VM can never evaluate `shell.lua`, never answer a `Reevaluate`, and never put anything on
    /// screen, so keeping the Wayland connection up past one would only hide the failure.
    pub fn start(
        shaping: ShapingHandle,
        outbound_tx: mpsc::UnboundedSender<RendererFrame>,
        generation_id: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shell_lua_path = shared::shell_lua_path().map_err(|err| format!("failed to resolve shell.lua's path: {err}"))?;
        let loader = Loader::new().map_err(|err| format!("failed to start the Lua loader: {err}"))?;
        let rescue_handle = register_rescue_signal(&loader).map_err(|err| format!("failed to register the rescue signal: {err}"))?;
        let process_registry = ProcessRegistry::new(generation_id, outbound_tx.clone());
        loader.register_process(process_registry.clone()).map_err(|err| format!("failed to register the process global: {err}"))?;
        let client = Self::new(loader, shell_lua_path, shaping, outbound_tx, rescue_handle, process_registry)
            .map_err(|err| format!("failed to seed the capability roster's signals: {err}"))?;
        Ok(client)
    }

    /// Every `shared::CAPABILITIES` roster name is pre-seeded here (not left to
    /// `apply_state_snapshot`'s lazy path) so a `shell.lua` that reads any rostered capability's
    /// global before its first real push still gets a live signal (reading `nil` inside it)
    /// instead of an undefined-global Lua error and rescue -- ADR-0037's uniform
    /// nil-until-hydrated contract. The previous hand-listed four-name seed (audio/network/
    /// bluetooth/tray) froze at ADR-0031 while six more capabilities landed, exactly the drift a
    /// hand-list guarantees; the roster is the single source both processes share.
    /// `capability_signal`'s lazy path stays as the fallback for unrostered names.
    fn new(
        loader: Loader,
        shell_lua_path: PathBuf,
        shaping: ShapingHandle,
        outbound_tx: mpsc::UnboundedSender<RendererFrame>,
        rescue_handle: LiveSignalHandle,
        process_registry: ProcessRegistry,
    ) -> mlua::Result<Self> {
        let mut seeded = HashMap::new();
        for capability in shared::CAPABILITIES {
            let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
            loader.set_global(capability, signal)?;
            seeded.insert((*capability).to_string(), handle);
        }
        Ok(Self {
            loader,
            shell_lua_path,
            scene: Scene::new(),
            shaping,
            capability_signals: RefCell::new(seeded),
            rescue_handle,
            process_registry,
            state: ReloadState { applied_topology: None, pending: None },
            outbound_tx,
        })
    }

    fn set_rescue_state(&self, is_rescue: bool, error_log: &str) {
        match rescue_table(&self.loader, is_rescue, error_log) {
            Ok(table) => self.rescue_handle.set(mlua::Value::Table(table)),
            Err(err) => eprintln!("control-socket client: failed to build rescue state: {err}"),
        }
    }

    /// `StateSnapshot` pushes only hydrate `snapshot.capability`'s own live signal now -- no Lua
    /// evaluation runs from this path any more (see the module doc comment). `&self`, not `&mut
    /// self`: the lazy-registration path below needs interior mutability anyway (mirrors why
    /// `LiveSignalHandle` itself uses `Rc<RefCell<_>>` internally), and `capability_signals`'
    /// own `RefCell` is what lets this stay a read-path method instead of upgrading every caller
    /// to `&mut self`.
    fn apply_state_snapshot(&self, snapshot: StateSnapshot) -> mlua::Result<()> {
        let value = self.loader.to_lua_value(&snapshot.payload)?;
        let handle = self.capability_signal(&snapshot.capability)?;
        handle.set(value);
        Ok(())
    }

    /// Looks up `capability`'s live signal, registering a fresh one (initial value `nil`) as a
    /// new Lua global named `capability` the first time this capability is ever seen
    /// (docs/adr/0029). Every later `StateSnapshot` for the same capability reuses the same
    /// handle instead of re-registering the global on every push.
    fn capability_signal(&self, capability: &str) -> mlua::Result<LiveSignalHandle> {
        if let Some(handle) = self.capability_signals.borrow().get(capability) {
            return Ok(handle.clone());
        }
        let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        self.loader.set_global(capability, signal)?;
        self.capability_signals.borrow_mut().insert(capability.to_string(), handle.clone());
        Ok(handle)
    }

    /// Evaluates `shell.lua` once at startup and applies it directly -- no round trip through the
    /// Supervisor needed, since there's no prior applied scene to protect yet (build-steps.md
    /// Phase 13). Leaves `state.applied_topology` at `None` on any failure: a startup failure
    /// leaves the shell blank (docs/adr/0024 item 4) -- `CONTEXT.md`'s Rollback guarantee is
    /// about a *re*-evaluation keeping its prior scene, and there is no prior scene on first
    /// boot. Because `None` also means "safe to apply" (not "topology []"), a later successful
    /// `Reevaluate` can still recover from this state instead of being stuck forever.
    ///
    /// Runs before any layer surface is bound (`oblisk-supervisor-services-dbus.md` § 15.2's
    /// order: evaluate, bind, null-buffer, signal ready), which on one thread is just the order
    /// of the statements in `crate::wayland::run`.
    pub fn run_startup_evaluation(&mut self) {
        match evaluate_and_topology(&self.loader, &self.shell_lua_path) {
            Ok((output, topology)) => match self.scene.apply(&output.surfaces, PLACEHOLDER_OUTPUT_SIZE, &self.shaping) {
                Ok(()) => {
                    log_applied_surfaces(&self.scene, &output);
                    self.set_rescue_state(false, "");
                    self.state.applied_topology = Some(topology);
                }
                Err(err) => {
                    eprintln!("control-socket client: startup shell.lua evaluated but failed to apply to the scene: {err}");
                    self.set_rescue_state(true, &err.to_string());
                }
            },
            Err(err) => {
                eprintln!("control-socket client: startup shell.lua evaluation failed: {err}");
                self.set_rescue_state(true, &err.to_string());
            }
        }
    }

    /// Handles one inbound `SupervisorFrame`, decoded off the wire by [`pump`] and handed over by
    /// `crate::wayland::run`'s poll loop.
    ///
    /// Returns `Some(nonce)` for an `ActivateDraw` and `None` for everything else: drawing needs
    /// the EGL/surface state that lives on `crate::wayland::App`, so that one nonce goes back to
    /// the caller to drive `App::activate_draw` (§ 15.3). Every other frame is fully serviced
    /// here.
    #[must_use]
    pub fn handle_frame(&mut self, frame: SupervisorFrame) -> Option<u64> {
        match frame {
            SupervisorFrame::StateSnapshot(snapshot) => {
                if let Err(err) = self.apply_state_snapshot(snapshot) {
                    eprintln!("control-socket client: failed to convert a pushed StateSnapshot to a Lua value: {err}");
                }
            }
            SupervisorFrame::Reevaluate(request) => self.handle_reevaluate(request),
            SupervisorFrame::ApplyPendingReload(apply) => self.handle_apply_pending(apply),
            SupervisorFrame::ActivateDraw(activate) => return Some(activate.nonce),
            SupervisorFrame::DeselectInput(DeselectInput { surface_id }) => {
                // Real, received, currently-inert: no per-surface input-region/focus
                // machinery exists yet to hand this to -- docs/adr/0025 item 4.
                eprintln!("control-socket client: DeselectInput({surface_id}) received (no real input-region wiring yet)");
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
            SupervisorFrame::IdleEvent(IdleEvent { generation_id, threshold_sec, state }) => {
                // Real, received, currently-inert: no Lua-side `register_threshold` callback
                // registry exists yet to dispatch this to -- ADR-0032's Supervisor-side
                // controller and wire types are that slice's scope; the Renderer-side
                // `on_idle`/`on_resume` callback lookup is a later phase.
                eprintln!(
                    "control-socket client: IdleEvent(generation={generation_id}, threshold_sec={threshold_sec}, state={state:?}) \
                     received (no Lua callback registry wired yet)"
                );
            }
        }
        None
    }

    /// Runs one `Reevaluate` request: evaluates `shell.lua`, classifies the result against
    /// `state.applied_topology`, updates `state.pending` and the rescue signal, and queues the
    /// verdict on the outbound channel. `applied_topology == None` (nothing ever applied, e.g.
    /// after a startup or prior reload failure) is treated as "not changed" -- there's nothing to
    /// protect, so the fresh evaluation is safe to stage as `pending` -- see the module doc
    /// comment point 3.
    fn handle_reevaluate(&mut self, request: ReevaluateRequest) {
        let report = match evaluate_and_topology(&self.loader, &self.shell_lua_path) {
            Ok((output, topology)) => {
                self.set_rescue_state(false, "");
                let topology_changed = self.state.applied_topology.as_ref().is_some_and(|applied| applied != &topology);
                if topology_changed {
                    // A topology-changed generation must not have its own scene mutated -- that's
                    // the swap path, a different generation's job (CONTEXT.md, Generation swap).
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
    /// refers to -- a mismatch means a newer `Reevaluate` has already superseded it (the
    /// debounced watcher fired again before this round trip completed); logged and ignored, not
    /// fatal.
    fn handle_apply_pending(&mut self, apply: ApplyPendingReload) {
        if !matches!(&self.state.pending, Some((sequence, _, _)) if *sequence == apply.sequence) {
            eprintln!("control-socket client: ApplyPendingReload({}) doesn't match the currently pending reload; ignoring", apply.sequence);
            return;
        }
        let (_, output, topology) = self.state.pending.take().expect("just confirmed Some above");
        match self.scene.apply(&output.surfaces, PLACEHOLDER_OUTPUT_SIZE, &self.shaping) {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &output);
                self.state.applied_topology = Some(topology);
            }
            Err(err) => eprintln!("control-socket client: ApplyPendingReload's stored evaluation failed to apply: {err}"),
        }
    }
}

async fn run(generation_id: u32, inbound_tx: std::sync::mpsc::Sender<SupervisorFrame>, mut outbound_rx: mpsc::UnboundedReceiver<RendererFrame>) {
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
/// to the Wayland dispatch thread, and write every `RendererFrame` that thread queues out to the
/// wire (build-steps.md Phase 18; docs/adr/0039).
///
/// A frame that fails to decode is a transport-level failure here (this connection has exactly
/// one sender, the Supervisor, and a fixed set of message shapes -- a bad frame means the two
/// sides have desynced, not a stray bad actor), unlike an `ApplyPendingReload` sequence mismatch,
/// which is an expected, recoverable race, not a decode failure.
///
/// The read and write directions each get their own long-lived loop, selected over as two whole
/// futures rather than one frame each (a CONFIRMED correctness fix). `shared::framing::read_frame`
/// does two sequential `read_exact` awaits (length prefix, then payload); partial progress lives
/// in that future, not in `read_half` itself. The old shape -- `tokio::select!` racing one
/// `read_json_frame` call against one `outbound_rx.recv()` -- dropped the read future whenever the
/// outbound branch won first, discarding any bytes already consumed for the frame in flight. The
/// next iteration would then read a length prefix out of the middle of a JSON payload, see a
/// bogus length, and desync the connection. Neither loop below ever completes during normal
/// operation, so `select!` only ever tears down whichever loop has already finished (i.e. at
/// shutdown, when the connection is finished anyway) -- it never cancels a `read_exact` mid-frame.
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
            // The plaintext copy a `SecureSubmit` carried across the channel is scrubbed the
            // instant its write completes, not left to `Drop` (ADR-0005/ADR-0027). The source
            // `shared::SecureBuffer` was already zeroized where the frame was built, in
            // `crate::wayland`'s `secure_submit_frame`.
            //
            // ponytail: this reaches every copy this call site controls, not every copy that
            // exists. `write_json_frame` -> `serde_json::to_vec` (and its mirror,
            // `read_json_frame` -> `serde_json::from_slice`, on the Supervisor's read side)
            // allocate their own JSON-encoded buffers internally and drop them unscrubbed;
            // reaching those would mean a custom, non-JSON wire path for this one frame
            // variant, which is more than this slice's scope. `SecureBuffer`'s own module doc
            // already frames the "sanctioned read" as building the outgoing envelope, not
            // chasing every downstream allocation past it.
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

/// Names an outbound frame for a write-failure log line, so the one shared write path still
/// reports what it failed to send (each variant used to have its own `tokio::select!` arm and its
/// own message). A fixed label per variant, not `{frame:?}`: `RendererFrame`'s derived `Debug`
/// would print a `SecureSubmit`'s `secret` bytes straight into the log (ADR-0005).
fn frame_label(frame: &RendererFrame) -> &'static str {
    match frame {
        RendererFrame::ReadySignal(_) => "ReadySignal",
        RendererFrame::PresentationEvidence(_) => "PresentationEvidence",
        RendererFrame::ReevaluateReport(_) => "ReevaluateReport",
        RendererFrame::Command(_) => "Command",
        RendererFrame::SecureSubmit(_) => "SecureSubmit",
    }
}

/// Builds the `{ is_rescue, error_log }` table and registers it as the ad-hoc `rescue` global
/// (mirrors the ad-hoc `audio` global, ADR-0022's precedent -- see docs/adr/0024 item 3, not
/// the full `oblisk.*` signal tree). Returns the handle so later evaluations can update it.
fn register_rescue_signal(loader: &Loader) -> mlua::Result<LiveSignalHandle> {
    let table = rescue_table(loader, false, "")?;
    let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Table(table));
    loader.set_global("rescue", signal)?;
    Ok(handle)
}

fn rescue_table(loader: &Loader, is_rescue: bool, error_log: &str) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    table.set("is_rescue", is_rescue)?;
    table.set("error_log", error_log)?;
    Ok(table)
}

/// `output.surfaces`' topology fingerprint, order-sensitive (`CONTEXT.md`, Topology change). A
/// surface whose topology fields don't type-check fails with [`lua::LoaderError::InvalidTopology`]
/// -- a distinct message from an actual top-level-return shape error, since conflating the two
/// (as an earlier version of this function did) produced a misleading `rescue.error_log`.
fn surfaces_topology(output: &lua::LoadOutput) -> Result<Vec<SurfaceTopology>, lua::LoaderError> {
    let mut topology = Vec::with_capacity(output.surfaces.len());
    for surface in &output.surfaces {
        let fingerprint = layout::node::surface_topology(&surface.properties).map_err(|err| lua::LoaderError::InvalidTopology(err.to_string()))?;
        topology.push(fingerprint);
    }
    Ok(topology)
}

// ponytail: this top-level evaluation is uncapped, unlike a `computed`/`map` closure's 5ms hook
// (docs/adr/0021, `renderer/src/lua/signal.rs`'s `set_hook`). docs/adr/0039 accepts this: a slow
// evaluation now blocks the Wayland dispatch thread it runs on (via `handle_reevaluate`, called
// from `wayland::run`'s poll loop, and via `run_startup_evaluation` before that loop even starts),
// with no configure handling and no way to set `app.exit` until it returns -- `while true do end`
// in `shell.lua` wedges the whole process. Upgrade path: extend ADR-0021's hook to cover
// `Loader::evaluate_file` itself, not just the closures it registers.
fn evaluate_and_topology(loader: &Loader, shell_lua_path: &Path) -> Result<(lua::LoadOutput, Vec<SurfaceTopology>), lua::LoaderError> {
    let output = loader.evaluate_file(shell_lua_path)?;
    let topology = surfaces_topology(&output)?;
    Ok((output, topology))
}

/// Logs each surface's resolved geometry after a successful `scene.apply` -- diagnostic
/// visibility only, matching Phase 12's original `apply_to_scene` logging.
fn log_applied_surfaces(scene: &Scene, output: &lua::LoadOutput) {
    for surface in &output.surfaces {
        let resolved = layout::node::parse_surface_id(&surface.properties).ok().and_then(|id| scene.surface(&id));
        match resolved {
            Some(r) => eprintln!(
                "layout resolved: surface {:?} kind={} rect={:?} visible={} children={} properties={}",
                surface.kind,
                r.kind,
                r.rect,
                r.visible,
                r.children.len(),
                r.properties.len()
            ),
            None => eprintln!("layout resolved but surface {:?} has no resolvable `id`", surface.kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Reads `rescue:get()`'s current `is_rescue`/`error_log` fields back out by evaluating a
    /// tiny probe script -- `LiveSignalHandle` only exposes `set`, so this is the only way to
    /// observe what a prior `set_rescue_state` call actually stored.
    fn rescue_state(loader: &Loader) -> (bool, String) {
        let output = loader
            .evaluate(r#"return surface { id = "_rescue_probe", layer = "Top", is_rescue = rescue:get().is_rescue, error_log = rescue:get().error_log }"#)
            .unwrap();
        let props = &output.surfaces[0].properties;
        let is_rescue = props.get("is_rescue").unwrap().as_boolean().unwrap();
        let error_log = props.get("error_log").unwrap().as_string().unwrap().to_string_lossy();
        (is_rescue, error_log)
    }

    /// A client wired to a real outbound channel, whose receiver is handed back so a test can
    /// read whatever the client queued for the socket thread. The same channel carries the
    /// `process` registry's `CommandEnvelope`s (`RendererClient::start` does the same in
    /// production); none of this file's tests exercise `process.run` itself, see
    /// `lua/process.rs`'s own tests for that.
    fn test_client(shell_lua_path: &std::path::Path) -> (RendererClient, mpsc::UnboundedReceiver<RendererFrame>) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let loader = Loader::new().unwrap();
        let rescue_handle = register_rescue_signal(&loader).unwrap();
        let process_registry = ProcessRegistry::new(0, outbound_tx.clone());
        loader.register_process(process_registry.clone()).unwrap();
        let client =
            RendererClient::new(loader, shell_lua_path.to_path_buf(), ShapingHandle::spawn(), outbound_tx, rescue_handle, process_registry).unwrap();
        (client, outbound_rx)
    }

    /// The one frame `client` queued on its outbound channel, or a panic naming what was missing
    /// -- every reload verdict is a single frame, so a test never has to skip past unrelated ones.
    fn queued_frame(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> RendererFrame {
        outbound_rx.try_recv().expect("a frame must have been queued for the socket thread")
    }

    #[test]
    fn apply_state_snapshot_updates_the_live_signal_without_evaluating_shell_lua() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let snapshot = StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({ "app_name": "Zen" }) };
        client.apply_state_snapshot(snapshot).unwrap();

        let output = client.loader.evaluate(r#"return surface { id = "bar", layer = "Top", app_name = audio:get().app_name }"#).unwrap();
        let app_name = output.surfaces[0].properties.get("app_name").unwrap().as_string().unwrap().to_string_lossy();
        assert_eq!(app_name, "Zen");
    }

    #[test]
    fn apply_state_snapshot_lazily_registers_an_unrostered_capabilitys_live_signal() {
        // docs/adr/0029: a capability outside shared::CAPABILITIES (ADR-0037) has no
        // pre-registered global -- the first StateSnapshot naming it must create the Lua global
        // on the spot, not error.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        assert!(!shared::CAPABILITIES.contains(&"workspace"), "this test needs a genuinely unrostered name");

        let snapshot = StateSnapshot { capability: "workspace".to_string(), revision: 1, payload: serde_json::json!({ "active": 2 }) };
        client.apply_state_snapshot(snapshot).unwrap();

        let output = client.loader.evaluate(r#"return surface { id = "bar", layer = "Top", active = workspace:get().active }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("active").unwrap().as_integer(), Some(2));
    }

    #[test]
    fn every_rostered_capabilitys_global_exists_and_reads_nil_before_its_first_snapshot() {
        // ADR-0037's uniform contract: a shell.lua reading any rostered capability at boot --
        // before the Supervisor's first push, or forever for a dormant one like sysinfo -- gets
        // a live signal reading nil, never an undefined-global error into rescue. This is the
        // regression the frozen four-name hand-list allowed six times in a row.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::CAPABILITIES {
            let probe = format!(r#"return surface {{ id = "bar", layer = "Top", is_nil = {capability}:get() == nil }}"#);
            let output = client.loader.evaluate(&probe).unwrap_or_else(|err| panic!("rostered capability {capability:?} has no live global: {err}"));
            assert_eq!(output.surfaces[0].properties.get("is_nil").unwrap().as_boolean(), Some(true), "{capability} should read nil before its first snapshot");
        }
    }

    #[test]
    fn apply_state_snapshot_reuses_the_same_signal_across_repeated_pushes_for_one_capability() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        client
            .apply_state_snapshot(StateSnapshot { capability: "network".to_string(), revision: 1, payload: serde_json::json!({ "scanning": true }) })
            .unwrap();
        client
            .apply_state_snapshot(StateSnapshot { capability: "network".to_string(), revision: 2, payload: serde_json::json!({ "scanning": false }) })
            .unwrap();

        let output = client.loader.evaluate(r#"return surface { id = "bar", layer = "Top", scanning = network:get().scanning }"#).unwrap();
        assert_eq!(
            output.surfaces[0].properties.get("scanning").unwrap().as_boolean(),
            Some(false),
            "the second push must update the same registered global, not fail or create a second one"
        );
    }

    #[test]
    fn run_startup_evaluation_applies_a_valid_file_and_clears_rescue() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        client.run_startup_evaluation();

        assert!(client.scene.surface("bar").is_some());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn run_startup_evaluation_on_a_missing_file_sets_rescue_and_leaves_scene_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let (mut client, _outbound_rx) = test_client(&missing);

        client.run_startup_evaluation();

        assert!(client.state.applied_topology.is_none());
        assert!(client.scene.surface("bar").is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(!error_log.is_empty());
    }

    #[test]
    fn a_successful_reevaluate_after_a_startup_failure_recovers_instead_of_reporting_topology_changed_forever() {
        // Regression test for a CONFIRMED correctness finding: treating "nothing applied yet" as
        // an empty topology (rather than "no prior state to protect") made every subsequent
        // evaluation -- even a fix to a syntactically valid file -- permanently misclassify as
        // `TopologyChanged`, which nothing here ever applies, leaving the shell blank forever.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let (mut client, mut outbound_rx) = test_client(&missing);
        client.run_startup_evaluation();
        assert!(client.state.applied_topology.is_none(), "startup must have failed (no file yet)");

        write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
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
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology = Some(surfaces_topology(&client.loader.evaluate_file(&path).unwrap()).unwrap());

        client.handle_reevaluate(ReevaluateRequest { sequence: 5 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 5 }));
        assert!(matches!(&client.state.pending, Some((sequence, _, _)) if *sequence == 5));
    }

    #[test]
    fn handle_reevaluate_reports_topology_changed_and_does_not_store_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        // Seed a *different* applied topology (a different id) so the fresh evaluation reads as changed.
        client.state.applied_topology =
            Some(vec![SurfaceTopology { id: "other".to_string(), layer: "Top".to_string(), anchor: Default::default(), monitor: "All".to_string() }]);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
        assert!(client.state.pending.is_none(), "a topology-changed generation must not stage a pending apply");
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
        // Regression test for a minor correctness finding: a topology-field type error (e.g.
        // `anchor.top` not a boolean) used to be folded into `InvalidTopLevelReturn`'s fixed
        // "must be a `surface` node or an array of them" message, which is wrong for this case.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top", anchor = { top = "yes" } }"#);
        let (mut client, mut outbound_rx) = test_client(&path);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        match queued_frame(&mut outbound_rx) {
            RendererFrame::ReevaluateReport(ReevaluateReport::Failed { error, .. }) => {
                assert!(error.contains("topology"), "expected a topology-specific message, got: {error}");
                assert!(!error.contains("top-level return"), "must not reuse the unrelated top-level-return message, got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn handle_apply_pending_reconciles_the_pending_evaluation_into_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let (output, topology) = evaluate_and_topology(&client.loader, &path).unwrap();
        client.state.pending = Some((3, output, topology));

        client.handle_apply_pending(ApplyPendingReload { sequence: 3 });

        assert!(client.scene.surface("bar").is_some());
        assert!(client.state.pending.is_none());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn handle_apply_pending_ignores_a_mismatched_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let (output, topology) = evaluate_and_topology(&client.loader, &path).unwrap();
        client.state.pending = Some((3, output, topology));

        client.handle_apply_pending(ApplyPendingReload { sequence: 99 });

        assert!(client.scene.surface("bar").is_none(), "a stale ApplyPendingReload must not apply");
        assert!(client.state.pending.is_some(), "the still-current pending evaluation must survive a mismatched Apply");
    }

    #[test]
    fn handle_frame_answers_a_reevaluate_frame_with_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        // A different applied topology so the fresh evaluation reads as changed -- proves the
        // dispatch/queue path, not `handle_reevaluate`'s own classification logic (already
        // covered by the tests above).
        client.state.applied_topology =
            Some(vec![SurfaceTopology { id: "other".to_string(), layer: "Top".to_string(), anchor: Default::default(), monitor: "All".to_string() }]);

        assert_eq!(client.handle_frame(SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 })), None);

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
    }

    #[test]
    fn handle_frame_hands_an_activate_draw_nonce_back_to_the_wayland_loop() {
        // The one frame `handle_frame` can't service itself: drawing needs `wayland::App`'s EGL
        // and surface state, so the nonce goes back to the caller for `App::activate_draw`.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(client.handle_frame(SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })), Some(42));
    }

    #[test]
    fn handle_frame_logs_and_continues_on_deselect_input_and_promote_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);

        assert_eq!(client.handle_frame(SupervisorFrame::DeselectInput(DeselectInput { surface_id: "main_bar".to_string() })), None);
        assert_eq!(client.handle_frame(SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id: "main_bar".to_string() })), None);
        // A third, recognized frame to prove dispatch kept working (not stuck/panicked) after the
        // two inert ones above. No prior `applied_topology` is seeded, so this fresh evaluation
        // reports `Unchanged` (see the module doc comment point 3) -- the report's exact verdict
        // isn't this test's point, only that a real response arrives at all after the two inert
        // frames.
        assert_eq!(client.handle_frame(SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 9 })), None);

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 9 }));
    }

    #[test]
    fn handle_frame_routes_process_output_and_exit_frames_to_the_registered_lua_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        // Register real out_cb/exit_cb through the real process.run global, exactly as
        // `renderer/src/lua/process.rs`'s own tests do -- the id (0, the first call on a fresh
        // registry) is what the inbound frames below address.
        client
            .loader
            .evaluate(
                r#"
                process.run("cmd", {}, function(line, stream) probe_line = line; probe_stream = stream end, function(code) probe_code = code end)
                return surface { id = "bar", layer = "Top" }
                "#,
            )
            .unwrap();

        let output_frame = SupervisorFrame::ProcessOutput(ProcessOutputLine { id: 0, stream: shared::ProcessStream::Stdout, line: "hello".to_string() });
        assert_eq!(client.handle_frame(output_frame), None);
        assert_eq!(client.handle_frame(SupervisorFrame::ProcessExited(ProcessExited { id: 0, code: Some(3) })), None);

        let output = client
            .loader
            .evaluate(r#"return surface { id = "bar", layer = "Top", line = probe_line, stream = probe_stream, code = probe_code }"#)
            .unwrap();
        let props = &output.surfaces[0].properties;
        assert_eq!(props.get("line").unwrap().as_string().unwrap().to_string_lossy(), "hello");
        assert_eq!(props.get("stream").unwrap().as_string().unwrap().to_string_lossy(), "stdout");
        assert_eq!(props.get("code").unwrap().as_integer().unwrap(), 3);
    }

    /// Queues `frame` for the socket thread and returns what [`pump`] actually wrote to the wire
    /// for it. `pump`'s read half never produces anything here, so it would otherwise run
    /// forever; a timeout bounds the test instead of relying on a second frame to end the loop.
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
        // build-steps.md Phase 15 item 2 / ADR-0005/ADR-0027: the wire frame carries the exact
        // secret `crate::wayland`'s `secure_submit_frame` read out of the accumulated
        // `SecureBuffer`, tagged with this process's own generation_id. `pump` zeroizes the
        // frame's plaintext copy the instant this write completes.
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
    /// which is always a bug in these tests (a live connection with both directions still open):
    /// `pump` completing early means one of its two loops broke (a decode error, a closed
    /// channel), not that it merely yielded control back.
    async fn let_pump_advance(mut pumping: std::pin::Pin<&mut impl std::future::Future<Output = ()>>, millis: u64) {
        tokio::select! {
            () = &mut pumping => unreachable!("pump must not return on its own in this test"),
            () = tokio::time::sleep(std::time::Duration::from_millis(millis)) => {}
        }
    }

    /// Regression test for a CONFIRMED correctness finding: `shared::framing::read_frame` does
    /// two sequential `read_exact` awaits (length prefix, then payload), so partial progress lives
    /// in the read future itself, not in `read_half`. The old `pump` raced one `read_json_frame`
    /// call against one `outbound_rx.recv()` per `tokio::select!` iteration -- if the outbound
    /// branch won while a read was stuck mid-payload, `select!` dropped the read future, losing
    /// the bytes already consumed. The next iteration then read a length prefix out of the middle
    /// of a JSON payload and desynced the connection ("connection ended").
    ///
    /// This drives exactly that race: an inbound frame's bytes arrive split across two writes,
    /// with an outbound frame becoming available (and written) while the inbound read is stalled
    /// in between. The fixed `pump` gives the read and write directions their own long-lived
    /// loops, so the stalled read survives the outbound activity intact.
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
        // Splits inside the payload, past the length prefix -- the read this is meant to catch
        // stalls partway through the *second* read_exact, not the first.
        let split_at = wire_bytes.len() / 2;
        assert!(split_at > 4, "the split point must land inside the payload, not the length prefix");

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx);
        tokio::pin!(pumping);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            wire.write_all(&wire_bytes[..split_at]).await.unwrap();
            // Let pump's reader consume the partial payload and block mid-`read_exact` for the rest.
            let_pump_advance(pumping.as_mut(), 20).await;

            // Queue an outbound frame while the inbound read is stalled mid-frame -- exactly the
            // race the old per-iteration `select!` lost.
            outbound_tx.send(RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] })).unwrap();
            let_pump_advance(pumping.as_mut(), 20).await;

            // The outbound frame must have reached the wire even though the inbound read never
            // finished -- the write direction must not be starved by the stuck read either.
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
