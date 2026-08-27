//! Renderer-side Unix control-socket client (build-steps.md Phase 9).
//!
//! Connects to `$XDG_RUNTIME_DIR/oblisk-shell.sock` as a client; the Supervisor is the
//! listener (`supervisor/src/socket.rs`) -- Phase 11's text reuses this same direction
//! ("The Supervisor is the listener, the Renderer connects as client"), not inverted. Sends a
//! `shared::ConnectionHandshake` as the first frame, then holds the connection open.
//!
//! Runs on its own OS thread with a dedicated current-thread tokio runtime, same reasoning as
//! `text::shaping`'s dedicated worker thread (see that module's doc comment): the main thread
//! is occupied by `wayland::run()`'s blocking dispatch loop, and this is one dedicated I/O
//! task, not a general async-task need -- exactly what `renderer/Cargo.toml`'s existing
//! `rt`/`net`/`macros` tokio features (present since scaffolding, unused until now) were
//! staged for.
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
//! 1/3/6; Phase 15 item 2 adds the fourth): [`spawn_client`] gains four channel endpoints
//! bridging this thread to the Wayland/EGL thread (`crate::wayland`, see `main.rs`'s doc comment
//! for all four channels' roles). `ready_rx`/`presented_rx`/`secure_submit_rx` are plain
//! `std::sync::mpsc::Receiver`s -- not directly pollable from an async task -- so each is
//! bridged into a `tokio::sync::mpsc` channel via its own dedicated `std::thread` looping
//! `.recv()`-and-forward, this project's established bridging idiom (see `text::shaping`'s
//! worker thread). `activate_tx` is a `std::sync::mpsc::Sender`, whose `.send()` is synchronous
//! and non-blocking -- callable directly from async code, no bridging needed.
//! [`RendererClient::dispatch_loop`] then `tokio::select!`s over the wire read half and all three
//! bridged receivers: `SupervisorFrame::ActivateDraw` forwards its nonce to `activate_tx`;
//! `DeselectInput`/`PromoteGeneration` are real, received, and currently logged only (no real
//! input-region/focus machinery exists yet to hand them to -- docs/adr/0025 item 4); the ready,
//! presented, and secure_submit channels are written back out as `RendererFrame::ReadySignal`/
//! `PresentationEvidence`/`SecureSubmit`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::framing::{self, write_json_frame};
use shared::{
    ActivateDraw, ApplyPendingReload, CommandEnvelope, ConnectionHandshake, DeselectInput, PresentationEvidence, ProcessExited, ProcessOutputLine,
    PromoteGeneration, ReadySignal, ReevaluateReport, ReevaluateRequest, RendererFrame, SecureSubmit, StateSnapshot, SupervisorFrame, Zeroize,
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
use crate::wayland::SecureSubmitPayload;

/// Real per-output pixel dimensions aren't threaded from Wayland into this thread yet (see
/// docs/adr/0023 item 6) -- `wayland::mod`'s output/surface objects live on the main thread,
/// this thread only has the socket connection and the Lua loader.
const PLACEHOLDER_OUTPUT_SIZE: layout::LogicalSize = layout::LogicalSize { width: 1920.0, height: 40.0 };

fn generation_id_from_env() -> u32 {
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
/// startup-ordering guarantee between the two processes.
/// `ready_rx`/`presented_rx`/`activate_tx`/`secure_submit_rx` are the Wayland-thread bridging
/// channels -- see the module doc comment.
pub fn spawn_client(
    ready_rx: std::sync::mpsc::Receiver<Vec<String>>,
    presented_rx: std::sync::mpsc::Receiver<PresentationEvidence>,
    activate_tx: std::sync::mpsc::Sender<u64>,
    secure_submit_rx: std::sync::mpsc::Receiver<SecureSubmitPayload>,
) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("control-socket client: failed to start runtime: {err}");
                return;
            }
        };
        runtime.block_on(run(ready_rx, presented_rx, activate_tx, secure_submit_rx));
    });
}

/// Bridges a blocking `std::sync::mpsc::Receiver` into a `tokio::sync::mpsc` channel via a
/// dedicated `std::thread` looping `.recv()`-and-forward -- this project's established bridging
/// idiom (see the module doc comment). Do not try to poll a `std::sync::mpsc::Receiver` from
/// inside an async task directly; it has no async-aware waker.
fn bridge_to_tokio<T: Send + 'static>(rx: std::sync::mpsc::Receiver<T>) -> mpsc::UnboundedReceiver<T> {
    let (tx, bridged_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(value) = rx.recv() {
            if tx.send(value).is_err() {
                break;
            }
        }
    });
    bridged_rx
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

/// Everything one connection's reload/dispatch loop needs, grouped so it travels as one
/// receiver instead of the same 6-7 pieces threaded through every function's parameter list
/// separately (Standards review, docs/adr/0024).
struct RendererClient {
    loader: Loader,
    shell_lua_path: PathBuf,
    scene: Scene,
    shaping: ShapingHandle,
    /// One live Lua signal per capability seen so far, keyed by `StateSnapshot.capability`
    /// (docs/adr/0029) -- `"audio"`, `"network"`, `"bluetooth"`, and `"tray"` are seeded at
    /// construction (see `new`'s doc comment); any other capability's global is registered
    /// lazily, on the first `StateSnapshot` that names it, by `apply_state_snapshot`. `RefCell`,
    /// not `&mut self`: `apply_state_snapshot` is called through a `&self` receiver (see its own
    /// doc comment for why), and this is the one piece of `RendererClient` state that read path
    /// needs to mutate.
    capability_signals: RefCell<HashMap<String, LiveSignalHandle>>,
    rescue_handle: LiveSignalHandle,
    process_registry: ProcessRegistry,
    state: ReloadState,
    /// Tags every `RendererFrame::SecureSubmit` this connection writes (build-steps.md Phase 15
    /// item 2) -- the Wayland thread doesn't know it (see the module doc comment), so it's kept
    /// here alongside the rest of this connection's own identity instead of threaded through
    /// `dispatch_loop`'s parameter list as a ninth argument.
    generation_id: u32,
}

/// `dispatch_loop`'s bridged-channel endpoints, bundled into one parameter instead of five
/// separate ones (mirrors [`RendererClient`]'s own reasoning for bundling its 6-7 pieces).
struct DispatchChannels<'a> {
    ready_rx: &'a mut mpsc::UnboundedReceiver<Vec<String>>,
    presented_rx: &'a mut mpsc::UnboundedReceiver<PresentationEvidence>,
    activate_tx: &'a std::sync::mpsc::Sender<u64>,
    process_outbound_rx: &'a mut mpsc::UnboundedReceiver<CommandEnvelope>,
    secure_submit_rx: &'a mut mpsc::UnboundedReceiver<SecureSubmitPayload>,
}

impl RendererClient {
    // 10 parameters: one more than the pre-tray shape now that `tray`, like `audio`/`network`/
    // `bluetooth`, is pre-seeded (docs/adr/0031 -- the NetworkManager PR initially missed
    // pre-seeding "network" and it was caught in review as a real spec gap; not repeating that
    // miss here) -- not worth inventing a bundling struct for a one-off constructor already
    // called from exactly two places (`run` and this module's own `test_client`).
    #[allow(clippy::too_many_arguments)]
    fn new(
        loader: Loader,
        shell_lua_path: PathBuf,
        shaping: ShapingHandle,
        audio_handle: LiveSignalHandle,
        network_handle: LiveSignalHandle,
        bluetooth_handle: LiveSignalHandle,
        tray_handle: LiveSignalHandle,
        rescue_handle: LiveSignalHandle,
        process_registry: ProcessRegistry,
        generation_id: u32,
    ) -> Self {
        // "audio" is pre-seeded (not left to apply_state_snapshot's lazy path) so a `shell.lua`
        // that reads `audio:get()` before the first real push still gets a live signal (reading
        // `nil` inside it) instead of an undefined-global Lua error -- the same guarantee the
        // pre-Phase-16 hardcoded registration gave. "network", "bluetooth", and "tray" are
        // pre-seeded too (docs/adr/0029, docs/adr/0030, docs/adr/0031): they're the other known,
        // always-present capabilities as of this diff, and without this a `shell.lua` referencing
        // any of these globals before its Supervisor-side controller's first event ever fires
        // would hit the same undefined-global Lua error, not merely read `nil`. Any capability
        // beyond these four only starts existing once its first StateSnapshot actually arrives,
        // via `capability_signal`'s lazy path below.
        let capability_signals = RefCell::new(HashMap::from([
            ("audio".to_string(), audio_handle),
            ("network".to_string(), network_handle),
            ("bluetooth".to_string(), bluetooth_handle),
            ("tray".to_string(), tray_handle),
        ]));
        Self {
            loader,
            shell_lua_path,
            scene: Scene::new(),
            shaping,
            capability_signals,
            rescue_handle,
            process_registry,
            state: ReloadState { applied_topology: None, pending: None },
            generation_id,
        }
    }

    fn set_rescue_state(&self, is_rescue: bool, error_log: &str) {
        match rescue_table(&self.loader, is_rescue, error_log) {
            Ok(table) => self.rescue_handle.set(mlua::Value::Table(table)),
            Err(err) => eprintln!("control-socket client: failed to build rescue state: {err}"),
        }
    }

    /// `StateSnapshot` pushes only hydrate `snapshot.capability`'s own live signal now -- no Lua
    /// evaluation runs from this path any more (see the module doc comment). `&self`, not `&mut
    /// self`: this is called from `dispatch_loop`'s read arm, which only holds a shared
    /// reference into `RendererClient` at that point (mirrors why `LiveSignalHandle` itself uses
    /// `Rc<RefCell<_>>` internally) -- `capability_signals`' own `RefCell` is what makes the
    /// lazy-registration path below possible without upgrading every caller to `&mut self`.
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
    fn run_startup_evaluation(&mut self) {
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

    /// Runs one `Reevaluate` request: evaluates `shell.lua`, classifies the result against
    /// `state.applied_topology`, updates `state.pending` and the rescue signal, and writes the
    /// verdict back over `write_half`. `applied_topology == None` (nothing ever applied, e.g.
    /// after a startup or prior reload failure) is treated as "not changed" -- there's nothing to
    /// protect, so the fresh evaluation is safe to stage as `pending` -- see the module doc
    /// comment point 3.
    async fn handle_reevaluate<W: AsyncWrite + Unpin>(&mut self, request: ReevaluateRequest, write_half: &mut W) {
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

        if let Err(err) = write_json_frame(write_half, &RendererFrame::ReevaluateReport(report)).await {
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

    /// Reads `shared::SupervisorFrame`s off `read_half` until the connection ends, dispatching
    /// each to `apply_state_snapshot`/`handle_reevaluate`/`handle_apply_pending`/`activate_tx`
    /// (`ActivateDraw`) -- and, alongside the read half, writes out whatever arrives on
    /// `channels`' bridged receivers as `ReadySignal`/`PresentationEvidence`/`Command`/
    /// `SecureSubmit` frames (build-steps.md Phase 14; Phase 15 item 2). A frame that fails to
    /// decode is a transport-level failure here (this connection has exactly one sender, the
    /// Supervisor, and a fixed set of message shapes -- a bad frame means the two sides have
    /// desynced, not a stray bad actor), unlike an `ApplyPendingReload` sequence mismatch, which
    /// is an expected, recoverable race, not a decode failure.
    async fn dispatch_loop<R, W>(&mut self, read_half: &mut R, write_half: &mut W, channels: DispatchChannels<'_>)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let DispatchChannels { ready_rx, presented_rx, activate_tx, process_outbound_rx, secure_submit_rx } = channels;
        loop {
            tokio::select! {
                frame = framing::read_json_frame::<_, SupervisorFrame>(read_half) => match frame {
                    Ok(SupervisorFrame::StateSnapshot(snapshot)) => {
                        if let Err(err) = self.apply_state_snapshot(snapshot) {
                            eprintln!("control-socket client: failed to convert a pushed StateSnapshot to a Lua value: {err}");
                        }
                    }
                    Ok(SupervisorFrame::Reevaluate(request)) => {
                        self.handle_reevaluate(request, write_half).await;
                    }
                    Ok(SupervisorFrame::ApplyPendingReload(apply)) => {
                        self.handle_apply_pending(apply);
                    }
                    Ok(SupervisorFrame::ActivateDraw(ActivateDraw { nonce })) => {
                        if let Err(err) = activate_tx.send(nonce) {
                            eprintln!("control-socket client: failed to forward ActivateDraw(nonce={nonce}) to the Wayland thread: {err}");
                        }
                    }
                    Ok(SupervisorFrame::DeselectInput(DeselectInput { surface_id })) => {
                        // Real, received, currently-inert: no per-surface input-region/focus
                        // machinery exists yet to hand this to -- docs/adr/0025 item 4.
                        eprintln!("control-socket client: DeselectInput({surface_id}) received (no real input-region wiring yet)");
                    }
                    Ok(SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id })) => {
                        eprintln!("control-socket client: PromoteGeneration({surface_id}) received (no real focus wiring yet)");
                    }
                    Ok(SupervisorFrame::ProcessOutput(ProcessOutputLine { id, stream, line })) => {
                        self.process_registry.dispatch_output(id, stream, line);
                    }
                    Ok(SupervisorFrame::ProcessExited(ProcessExited { id, code })) => {
                        self.process_registry.dispatch_exit(id, code);
                    }
                    Err(err) => {
                        eprintln!("control-socket client: connection ended: {err}");
                        break;
                    }
                },
                Some(surfaces) = ready_rx.recv() => {
                    if let Err(err) = write_json_frame(write_half, &RendererFrame::ReadySignal(ReadySignal { surfaces })).await {
                        eprintln!("control-socket client: failed to send a ReadySignal: {err}");
                    }
                }
                Some(evidence) = presented_rx.recv() => {
                    if let Err(err) = write_json_frame(write_half, &RendererFrame::PresentationEvidence(evidence)).await {
                        eprintln!("control-socket client: failed to send PresentationEvidence: {err}");
                    }
                }
                Some(envelope) = process_outbound_rx.recv() => {
                    if let Err(err) = write_json_frame(write_half, &RendererFrame::Command(envelope)).await {
                        eprintln!("control-socket client: failed to send a process Command: {err}");
                    }
                }
                Some(mut payload) = secure_submit_rx.recv() => {
                    // The one sanctioned read (ADR-0005/ADR-0027): performed here, as close as
                    // possible to the actual wire write, then both the source buffer and this
                    // call's own plaintext copy are `.zeroize()`'d immediately after that write
                    // completes -- not left to `Drop` alone.
                    //
                    // ponytail: this reaches every copy this call site controls, not every copy
                    // that exists. `write_json_frame` -> `serde_json::to_vec` (and its mirror,
                    // `read_json_frame` -> `serde_json::from_slice`, on the Supervisor's read
                    // side) allocate their own JSON-encoded buffers internally and drop them
                    // unscrubbed; reaching those would mean a custom, non-JSON wire path for this
                    // one frame variant, which is more than this slice's scope. `SecureBuffer`'s
                    // own module doc already frames the "sanctioned read" as building the
                    // outgoing envelope, not chasing every downstream allocation past it.
                    let mut frame = RendererFrame::SecureSubmit(SecureSubmit {
                        generation_id: self.generation_id,
                        capability: payload.capability,
                        action: payload.action,
                        secret: payload.buffer.expose_secret().to_vec(),
                    });
                    if let Err(err) = write_json_frame(write_half, &frame).await {
                        eprintln!("control-socket client: failed to send a SecureSubmit: {err}");
                    }
                    if let RendererFrame::SecureSubmit(inner) = &mut frame {
                        inner.secret.zeroize();
                    }
                    payload.buffer.zeroize();
                }
            }
        }
    }
}

async fn run(
    ready_rx: std::sync::mpsc::Receiver<Vec<String>>,
    presented_rx: std::sync::mpsc::Receiver<PresentationEvidence>,
    activate_tx: std::sync::mpsc::Sender<u64>,
    secure_submit_rx: std::sync::mpsc::Receiver<SecureSubmitPayload>,
) {
    let mut ready_rx = bridge_to_tokio(ready_rx);
    let mut presented_rx = bridge_to_tokio(presented_rx);
    let mut secure_submit_rx = bridge_to_tokio(secure_submit_rx);

    let path = match shared::control_socket_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("control-socket client: {err}");
            return;
        }
    };
    let shell_lua_path = match shared::shell_lua_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("control-socket client: failed to resolve shell.lua's path: {err}");
            return;
        }
    };

    let generation_id = generation_id_from_env();
    let stream = match connect_and_handshake(&path, generation_id).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("control-socket client: failed to connect to {}: {err}", path.display());
            return;
        }
    };

    let loader = match Loader::new() {
        Ok(loader) => loader,
        Err(err) => {
            eprintln!("control-socket client: failed to start the Lua loader: {err}");
            return;
        }
    };
    let (signal, audio_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
    if let Err(err) = loader.set_global("audio", signal) {
        eprintln!("control-socket client: failed to register the audio signal: {err}");
        return;
    }
    let (network_signal, network_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
    if let Err(err) = loader.set_global("network", network_signal) {
        eprintln!("control-socket client: failed to register the network signal: {err}");
        return;
    }
    let (bluetooth_signal, bluetooth_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
    if let Err(err) = loader.set_global("bluetooth", bluetooth_signal) {
        eprintln!("control-socket client: failed to register the bluetooth signal: {err}");
        return;
    }
    let (tray_signal, tray_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
    if let Err(err) = loader.set_global("tray", tray_signal) {
        eprintln!("control-socket client: failed to register the tray signal: {err}");
        return;
    }
    let rescue_handle = match register_rescue_signal(&loader) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("control-socket client: failed to register the rescue signal: {err}");
            return;
        }
    };
    let (process_outbound_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
    let process_registry = ProcessRegistry::new(generation_id, process_outbound_tx);
    if let Err(err) = loader.register_process(process_registry.clone()) {
        eprintln!("control-socket client: failed to register the process global: {err}");
        return;
    }

    let shaping = ShapingHandle::spawn();
    let mut client = RendererClient::new(
        loader,
        shell_lua_path,
        shaping,
        audio_handle,
        network_handle,
        bluetooth_handle,
        tray_handle,
        rescue_handle,
        process_registry,
        generation_id,
    );
    client.run_startup_evaluation();

    let (mut read_half, mut write_half) = stream.into_split();
    let channels = DispatchChannels {
        ready_rx: &mut ready_rx,
        presented_rx: &mut presented_rx,
        activate_tx: &activate_tx,
        process_outbound_rx: &mut process_outbound_rx,
        secure_submit_rx: &mut secure_submit_rx,
    };
    client.dispatch_loop(&mut read_half, &mut write_half, channels).await;
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

    fn test_client(shell_lua_path: &std::path::Path) -> RendererClient {
        let loader = Loader::new().unwrap();
        let (audio_signal, audio_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("audio", audio_signal).unwrap();
        let (network_signal, network_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("network", network_signal).unwrap();
        let (bluetooth_signal, bluetooth_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("bluetooth", bluetooth_signal).unwrap();
        let (tray_signal, tray_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("tray", tray_signal).unwrap();
        let rescue_handle = register_rescue_signal(&loader).unwrap();
        // None of this file's tests exercise process.run itself (see lua/process.rs's own tests
        // for that) -- a throwaway channel is enough to satisfy RendererClient's shape.
        let (process_tx, _process_rx) = mpsc::unbounded_channel();
        let process_registry = ProcessRegistry::new(0, process_tx);
        loader.register_process(process_registry.clone()).unwrap();
        RendererClient::new(
            loader,
            shell_lua_path.to_path_buf(),
            ShapingHandle::spawn(),
            audio_handle,
            network_handle,
            bluetooth_handle,
            tray_handle,
            rescue_handle,
            process_registry,
            0,
        )
    }

    #[test]
    fn apply_state_snapshot_updates_the_live_signal_without_evaluating_shell_lua() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let client = test_client(&missing);

        let snapshot = StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({ "app_name": "Zen" }) };
        client.apply_state_snapshot(snapshot).unwrap();

        let output = client.loader.evaluate(r#"return surface { id = "bar", layer = "Top", app_name = audio:get().app_name }"#).unwrap();
        let app_name = output.surfaces[0].properties.get("app_name").unwrap().as_string().unwrap().to_string_lossy();
        assert_eq!(app_name, "Zen");
    }

    #[test]
    fn apply_state_snapshot_lazily_registers_a_new_capabilitys_live_signal() {
        // docs/adr/0029: a capability other than "audio"/"network"/"bluetooth"/"tray"
        // (docs/adr/0030, docs/adr/0031) has no pre-registered global -- the first StateSnapshot
        // naming it must create the Lua global on the spot, not error.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let client = test_client(&missing);

        let snapshot = StateSnapshot { capability: "mpris".to_string(), revision: 1, payload: serde_json::json!({ "playing": true }) };
        client.apply_state_snapshot(snapshot).unwrap();

        let output = client.loader.evaluate(r#"return surface { id = "bar", layer = "Top", playing = mpris:get().playing }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("playing").unwrap().as_boolean(), Some(true));
    }

    #[test]
    fn apply_state_snapshot_reuses_the_same_signal_across_repeated_pushes_for_one_capability() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let client = test_client(&missing);

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
        let mut client = test_client(&path);

        client.run_startup_evaluation();

        assert!(client.scene.surface("bar").is_some());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn run_startup_evaluation_on_a_missing_file_sets_rescue_and_leaves_scene_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let mut client = test_client(&missing);

        client.run_startup_evaluation();

        assert!(client.state.applied_topology.is_none());
        assert!(client.scene.surface("bar").is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(!error_log.is_empty());
    }

    #[tokio::test]
    async fn a_successful_reevaluate_after_a_startup_failure_recovers_instead_of_reporting_topology_changed_forever() {
        // Regression test for a CONFIRMED correctness finding: treating "nothing applied yet" as
        // an empty topology (rather than "no prior state to protect") made every subsequent
        // evaluation -- even a fix to a syntactically valid file -- permanently misclassify as
        // `TopologyChanged`, which nothing here ever applies, leaving the shell blank forever.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let mut client = test_client(&missing);
        client.run_startup_evaluation();
        assert!(client.state.applied_topology.is_none(), "startup must have failed (no file yet)");

        write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(
            frame,
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 1 }),
            "the first successful evaluation after a startup failure must be treated as safe to apply, not a topology change"
        );
        assert!(client.state.pending.is_some());
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_unchanged_and_stores_pending_when_topology_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        client.state.applied_topology = Some(surfaces_topology(&client.loader.evaluate_file(&path).unwrap()).unwrap());

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 5 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 5 }));
        assert!(matches!(&client.state.pending, Some((sequence, _, _)) if *sequence == 5));
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_topology_changed_and_does_not_store_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        // Seed a *different* applied topology (a different id) so the fresh evaluation reads as changed.
        client.state.applied_topology =
            Some(vec![SurfaceTopology { id: "other".to_string(), layer: "Top".to_string(), anchor: Default::default(), monitor: "All".to_string() }]);

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
        assert!(client.state.pending.is_none(), "a topology-changed generation must not stage a pending apply");
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_failed_and_sets_rescue_on_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), "this is not lua");
        let mut client = test_client(&path);

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 2 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        match frame {
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

    #[tokio::test]
    async fn handle_reevaluate_reports_a_topology_field_error_distinctly_from_a_top_level_return_error() {
        // Regression test for a minor correctness finding: a topology-field type error (e.g.
        // `anchor.top` not a boolean) used to be folded into `InvalidTopLevelReturn`'s fixed
        // "must be a `surface` node or an array of them" message, which is wrong for this case.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top", anchor = { top = "yes" } }"#);
        let mut client = test_client(&path);

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        match frame {
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
        let mut client = test_client(&path);
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
        let mut client = test_client(&path);
        let (output, topology) = evaluate_and_topology(&client.loader, &path).unwrap();
        client.state.pending = Some((3, output, topology));

        client.handle_apply_pending(ApplyPendingReload { sequence: 99 });

        assert!(client.scene.surface("bar").is_none(), "a stale ApplyPendingReload must not apply");
        assert!(client.state.pending.is_some(), "the still-current pending evaluation must survive a mismatched Apply");
    }

    #[tokio::test]
    async fn dispatch_loop_answers_a_reevaluate_frame_with_a_report_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        // A different applied topology so the fresh evaluation reads as changed -- proves the
        // wire-level decode/dispatch/encode path, not `handle_reevaluate`'s own classification
        // logic (already covered by the tests above).
        client.state.applied_topology =
            Some(vec![SurfaceTopology { id: "other".to_string(), layer: "Top".to_string(), anchor: Default::default(), monitor: "All".to_string() }]);

        // Not `tokio::spawn`: `Loader`/`Scene` hold `mlua`/`Rc`-backed state that isn't `Send`
        // (the real client only ever runs on its own dedicated current-thread runtime -- see
        // the module doc comment). Everything below runs sequentially in this one task instead:
        // write the request and close the write half so `dispatch_loop`'s *second* read hits
        // EOF and returns after processing exactly one frame; its response is already sitting
        // in the duplex buffer to be read back afterward.
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        write_json_frame(&mut wire, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 })).await.unwrap();
        wire.shutdown().await.unwrap();

        // Channel ends this test doesn't exercise -- kept alive (not dropped) so `dispatch_loop`'s
        // `Some(..) = rx.recv()` branches simply stay pending instead of resolving to `None`.
        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();

        client
            .dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            })
            .await;

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
    }

    #[tokio::test]
    async fn dispatch_loop_forwards_an_activate_draw_nonce_to_the_wayland_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        write_json_frame(&mut wire, &SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })).await.unwrap();
        wire.shutdown().await.unwrap();

        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();

        client
            .dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            })
            .await;

        assert_eq!(activate_rx.try_recv(), Ok(42));
    }

    #[tokio::test]
    async fn dispatch_loop_logs_and_continues_on_deselect_input_and_promote_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        write_json_frame(&mut wire, &SupervisorFrame::DeselectInput(DeselectInput { surface_id: "main_bar".to_string() })).await.unwrap();
        write_json_frame(&mut wire, &SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id: "main_bar".to_string() })).await.unwrap();
        // A third, recognized frame to prove the loop kept running (not stuck/panicked) after
        // the two inert ones above, then close so dispatch_loop returns. No prior
        // `applied_topology` is seeded, so this fresh evaluation reports `Unchanged` (see the
        // module doc comment point 3) -- the report's exact verdict isn't this test's point,
        // only that a real response arrives at all after the two inert frames.
        write_json_frame(&mut wire, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 9 })).await.unwrap();
        wire.shutdown().await.unwrap();

        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();

        client
            .dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            })
            .await;

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 9 }));
    }

    #[tokio::test]
    async fn dispatch_loop_writes_a_ready_signal_frame_when_the_bridged_ready_channel_fires() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();
        ready_tx.send(vec!["main_bar".to_string(), "overlay_canvas".to_string()]).unwrap();

        // Run dispatch_loop concurrently with reading its response -- the read half never
        // produces anything here, so dispatch_loop would otherwise run forever; a timeout bounds
        // the test instead of relying on a second frame to end the loop.
        let dispatch =
            client.dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            });
        let read_response = read_json_frame::<_, RendererFrame>(&mut wire);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                _ = dispatch => unreachable!("dispatch_loop must not return on its own in this test"),
                frame = read_response => frame.unwrap(),
            }
        })
        .await
        .expect("a ReadySignal frame must arrive before the timeout");

        assert_eq!(frame, RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string(), "overlay_canvas".to_string()] }));
    }

    #[tokio::test]
    async fn dispatch_loop_writes_a_presentation_evidence_frame_when_the_bridged_presented_channel_fires() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();
        presented_tx.send(PresentationEvidence { nonce: 7, surface_id: "wallpaper_layer@DP-1".to_string() }).unwrap();

        let dispatch =
            client.dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            });
        let read_response = read_json_frame::<_, RendererFrame>(&mut wire);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                _ = dispatch => unreachable!("dispatch_loop must not return on its own in this test"),
                frame = read_response => frame.unwrap(),
            }
        })
        .await
        .expect("a PresentationEvidence frame must arrive before the timeout");

        assert_eq!(frame, RendererFrame::PresentationEvidence(PresentationEvidence { nonce: 7, surface_id: "wallpaper_layer@DP-1".to_string() }));
    }

    #[tokio::test]
    async fn dispatch_loop_writes_a_secure_submit_frame_reading_the_buffer_before_zeroizing_it() {
        // build-steps.md Phase 15 item 2 / ADR-0005/ADR-0027: the wire frame carries the exact
        // secret the Wayland thread accumulated, tagged with the connection's own generation_id
        // (not something the Wayland thread knows -- see the module doc comment).
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        client.generation_id = 4;

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");
        secure_submit_tx.send(SecureSubmitPayload { capability: "polkit".to_string(), action: "authenticate".to_string(), buffer }).unwrap();

        let dispatch = client.dispatch_loop(
            &mut server_read,
            &mut server_write,
            DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            },
        );
        let read_response = read_json_frame::<_, RendererFrame>(&mut wire);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                _ = dispatch => unreachable!("dispatch_loop must not return on its own in this test"),
                frame = read_response => frame.unwrap(),
            }
        })
        .await
        .expect("a SecureSubmit frame must arrive before the timeout");

        assert_eq!(
            frame,
            RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            })
        );
    }

    #[tokio::test]
    async fn dispatch_loop_writes_a_queued_process_command_frame_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();
        process_tx
            .send(CommandEnvelope {
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
            })
            .unwrap();

        let dispatch =
            client.dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            });
        let read_response = read_json_frame::<_, RendererFrame>(&mut wire);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                _ = dispatch => unreachable!("dispatch_loop must not return on its own in this test"),
                frame = read_response => frame.unwrap(),
            }
        })
        .await
        .expect("a process Command frame must arrive before the timeout");

        match frame {
            RendererFrame::Command(envelope) => {
                assert_eq!(envelope.params.capability, "process");
                assert_eq!(envelope.params.action, "run");
                assert_eq!(envelope.id, 1);
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_loop_routes_process_output_and_exit_frames_to_the_registered_lua_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
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

        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);
        write_json_frame(&mut wire, &SupervisorFrame::ProcessOutput(ProcessOutputLine { id: 0, stream: shared::ProcessStream::Stdout, line: "hello".to_string() }))
            .await
            .unwrap();
        write_json_frame(&mut wire, &SupervisorFrame::ProcessExited(ProcessExited { id: 0, code: Some(3) })).await.unwrap();
        wire.shutdown().await.unwrap();

        let (_ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (_presented_tx, mut presented_rx) = mpsc::unbounded_channel();
        let (activate_tx, _activate_rx) = std::sync::mpsc::channel();
        let (_process_tx, mut process_outbound_rx) = mpsc::unbounded_channel();
        let (_secure_submit_tx, mut secure_submit_rx) = mpsc::unbounded_channel();

        client
            .dispatch_loop(&mut server_read, &mut server_write, DispatchChannels {
                ready_rx: &mut ready_rx,
                presented_rx: &mut presented_rx,
                activate_tx: &activate_tx,
                process_outbound_rx: &mut process_outbound_rx,
                secure_submit_rx: &mut secure_submit_rx,
            })
            .await;

        let output = client
            .loader
            .evaluate(r#"return surface { id = "bar", layer = "Top", line = probe_line, stream = probe_stream, code = probe_code }"#)
            .unwrap();
        let props = &output.surfaces[0].properties;
        assert_eq!(props.get("line").unwrap().as_string().unwrap().to_string_lossy(), "hello");
        assert_eq!(props.get("stream").unwrap().as_string().unwrap().to_string_lossy(), "stdout");
        assert_eq!(props.get("code").unwrap().as_integer().unwrap(), 3);
    }
}
