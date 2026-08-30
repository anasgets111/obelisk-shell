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
//! In-place reload/Generation swap; docs/adr/0024): a `shared::StateSnapshot` push hydrates that
//! snapshot's own `capability`-named live signal (docs/adr/0029; created lazily on first sight of
//! a new capability name, see `apply_state_snapshot`) and marks the scene dirty
//! (docs/adr/0044 decision 2, `CONTEXT.md`'s Dirty scene entry) -- it never triggers a Lua
//! evaluation itself, unlike Phase 11's proof-of-wiring hack. `RendererClient::re_resolve_if_dirty`
//! is what a dirty flag leads to: a re-run of `Scene::apply` against the last applied
//! `lua::LoadOutput`, not a re-read of `shell.lua`. Evaluation itself is still driven only by the
//! Supervisor's own
//! `shared::SupervisorFrame::Reevaluate`, sent after its `inotify` watch on
//! `~/.config/oblisk/` detects a debounced edit to `shell.lua`:
//!
//! 1. On `Reevaluate`, [`RendererClient::handle_reevaluate`] reads and evaluates the real
//!    `shell.lua` file ([`crate::lua::Loader::evaluate_file`]) *without* applying it to the
//!    retained [`Scene`] yet, diffs the evaluation's topology
//!    (`crate::layout::node::SurfaceFingerprint`, one entry per declared surface of any role)
//!    against whatever's currently applied, and reports back a
//!    `shared::ReevaluateReport::Unchanged`, `TopologyChanged`, or `Failed` verdict -- the
//!    Supervisor (CONTEXT.md's Watcher) owns what happens next, not this module.
//! 2. `Unchanged` evaluations are kept as `pending`, applied to the `Scene` only once the
//!    Supervisor sends back `ApplyPendingReload` for that same sequence -- never eagerly,
//!    since a `TopologyChanged` verdict must leave this generation's own scene untouched (that
//!    case is a generation swap, a different generation's job, Phase 14).
//! 3. `applied_topology` is the topology this generation's *surfaces were built from*, and it is
//!    `None` only when no evaluation has produced one (first boot, or a startup evaluation that
//!    failed). Since docs/adr/0038 that is the evaluation's topology, recorded in
//!    [`RendererClient::run_startup_evaluation`], not the applied scene's: `crate::wayland::run`
//!    creates the surfaces straight from those specs, so they exist whether or not the tree inside
//!    them resolved. Before that the surfaces were a fixed Rust-owned set with no relation to the
//!    topology at all, and tying this field to a successful apply was the only proxy available. Now
//!    the thing the diff is really asking is "do the surfaces that exist still match what the config
//!    declares", and that is this field.
//!
//!    It covers **every** declared role since build-steps.md Phase 22, not only the panels
//!    (docs/adr/0049 decision 3, ADR-0001). A `window`'s and a `popup`'s Wayland object comes and
//!    goes inside one generation, but the *declaration* is still fixed for that generation's life,
//!    so adding or removing one is a topology change like any other -- and a panel-only fingerprint
//!    could not see it, so such an edit reported `Unchanged` and reloaded in place into a generation
//!    that had built no surface for it.
//!
//!    `handle_reevaluate` treats `None` as "safe to apply", not as an empty topology to diff
//!    against: after a startup failure there's nothing to protect, so the next successful
//!    evaluation -- whether it's the file the user just fixed, or the same one retried -- must be
//!    able to recover, not be permanently misclassified as `TopologyChanged` (which nothing here
//!    ever applies).
//! 4. An evaluation failure sets the ad-hoc `rescue` global's `is_rescue`/`error_log` fields
//!    (mirrors the ad-hoc `audio` global, ADR-0022's precedent -- not the full `oblisk.*`
//!    signal tree) and leaves the prior applied scene untouched (`CONTEXT.md`'s Rollback).
//! 5. A dirty-scene re-resolve failure is a distinct case from 4, not a variant of it:
//!    [`RendererClient::re_resolve_if_dirty`] leaves the prior scene untouched the same way (via
//!    [`Scene::apply`]'s own rollback), but never sets `rescue` -- `rescue` means `shell.lua`
//!    itself failed to evaluate, and a rejected pushed value is a property parser rejecting one
//!    capability's transient number or string, not that.
//!
//! Deliberately deferred: real generation-ID assignment tied to process spawning (a later phase,
//! once Phase 7/8's spawn primitives are wired to this transport) -- for now the generation ID
//! comes from the `OBLISK_GENERATION_ID` env var, defaulting to `0`. Reconnection if the
//! connection drops is not deferred, it is refused: docs/adr/0059 decision 1 makes a dropped
//! connection this process's exit, because the Supervisor holds every capability, every
//! `process.run` child and PAM, so there is no useful shell left on this side to reconnect *with*.
//!
//! Real PBA handshake wiring (build-steps.md Phase 14, § 15.2-15.3, closing docs/adr/0019 items
//! 1/3/6; Phase 15 item 2 adds `SecureSubmit`): `ReadySignal`, `PresentationEvidence` and
//! `SecureSubmit` are all built by `crate::wayland::App` itself, at the point that actually knows
//! them, and reach the wire as ordinary outbound frames. `ActivateDraw` and `SetSessionLock` are
//! the two inbound frames [`RendererClient::handle_frame`] can't service on its own -- drawing
//! needs the EGL/surface state, and a session lock is a Wayland object (docs/adr/0042) -- so each
//! hands its argument straight back to the Wayland poll loop as a [`FrameOutcome`].
//! `DeselectInput`/`PromoteGeneration` are real, received, and currently logged only (no real
//! input-region/focus machinery exists yet to hand them to -- docs/adr/0025 item 4).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::framing::{self, write_json_frame};
use shared::{
    ApplyPendingReload, ConnectionHandshake, DeselectInput, IdleEvent, ProcessExited, ProcessOutputLine, PromoteGeneration, ReevaluateReport,
    ReevaluateRequest, RendererFrame, SetSessionLock, StateSnapshot, SupervisorFrame, Zeroize,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{SurfaceFingerprint, SurfaceSpec};
use crate::layout::{self, Scene};
use crate::lua::capability::{Capability, CapabilityHandle, CommandSender};
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::{DirtyFlag, LiveSignalHandle};
use crate::lua::{self, Loader};
use crate::text::shaping::ShapingHandle;

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

/// Spawns the dedicated connect-and-hold-open thread. A connection failure (wrong path, a
/// Supervisor that is not there) logs and ends this thread, which drops `inbound_tx`, which the
/// Wayland thread reads as `Disconnected` and exits on (docs/adr/0059 decision 1). There is no
/// startup race to tolerate: `supervisor/src/main.rs` binds the control socket before it spawns
/// the first Renderer, so nothing legitimate reaches this path with the Supervisor alive.
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

/// One connection's reload bookkeeping. `applied_topology` is the topology this generation's
/// surfaces were built from, `None` only when no evaluation has produced one -- see the module
/// doc comment point 3 for why that's not the same thing as an empty topology, and for why
/// docs/adr/0038 moved it off "successfully applied" and onto "successfully evaluated". `pending`
/// holds the evaluated-but-not-yet-applied output (and its already-computed topology, so
/// `handle_apply_pending` doesn't need to recompute it) between a `Reevaluate` that reported
/// `Unchanged` and its matching `ApplyPendingReload`.
///
/// `applied_output` is the evaluation later re-resolves run against -- ADR-0044 decision 2's
/// re-resolve target. `handle_apply_pending` sets it only on a successful apply;
/// `run_startup_evaluation` sets it on a successful evaluation, because the caller needs its
/// surfaces bound before anything can be resolved at all (§ 15.2's evaluate-then-bind order). The
/// two rules agree in the only case where both could fire, since an in-place reload happens only
/// after the diff already found the topology unchanged. Kept alive past the evaluation that
/// produced it, rather than dropped once applied, so every later push can resolve against it
/// without re-running `shell.lua`.
///
/// What makes that safe is a *drop-order* obligation, not a lifetime the values enforce for
/// themselves, and ADR-0044 decision 4 states the mechanism backwards (its own amendment says so).
/// mlua 0.12's `ValueRef` holds a `WeakLua`, not a strong reference
/// (`mlua-0.12.0/src/types/value_ref.rs`), so these retained `mlua::Value`s do *not* keep the VM
/// alive: nothing stops the `Lua` being dropped first. The hazard is the opposite one -- reading a
/// value out of a dead state panics, because `ValueRef::to_pointer` locks the state and
/// `LoadOutput`'s derived `Debug` reaches it (a `{other:?}` in a `LayoutError` message is enough).
/// `ValueRef::Drop` is the only operation that survives a dead state, since it uses `try_lock` and
/// no-ops. So the `Lua` must outlive every retained value because the code reads them, and Rust
/// drops struct fields in declaration order: see [`RendererClient`]'s field ordering.
struct ReloadState {
    applied_topology: Option<Vec<SurfaceFingerprint>>,
    applied_output: Option<lua::LoadOutput>,
    pending: Option<(u64, lua::LoadOutput, Vec<SurfaceFingerprint>)>,
}

/// What one inbound [`SupervisorFrame`] still owes the Wayland thread after
/// [`RendererClient::handle_frame`] has done everything it can do on its own.
///
/// One enum rather than an `Option<u64>` plus a second out-parameter, and the second frame is what
/// forced it: `ActivateDraw` and `SetSessionLock` are the two frames whose work lives on
/// `crate::wayland::App` (EGL and surface state for the first, SCTK's `SessionLockState` and the
/// lock surfaces for the second, docs/adr/0042). A second `Option` beside the first would let a
/// caller service both, neither, or the wrong one, and nothing in the type would say that exactly
/// one of them can be owed per frame. This says it.
///
/// `Handled` is not "nothing happened": most frames -- a `StateSnapshot` hydrating a signal, a
/// `Reevaluate` producing a report -- do their whole job inside `handle_frame` and owe the caller
/// only the knowledge that they did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Fully serviced inside [`RendererClient::handle_frame`].
    Handled,
    /// § 15.3's `ActivateDraw`: draw the announced surface set and request presentation feedback
    /// for each, tagged with this nonce (`crate::wayland::App::activate_draw`).
    ActivateDraw(u64),
    /// ADR-0042/docs/adr/0052's `SetSessionLock`: make the session lock match this flag and report
    /// a `LockReport` for what happened (`crate::wayland::App::set_session_lock`).
    SetSessionLock(bool),
}

/// One generation's whole Lua side: the VM, the retained scene, the live signals, and the
/// reload bookkeeping, grouped so they travel as one receiver instead of the same 6-7 pieces
/// threaded through every function's parameter list separately (Standards review, docs/adr/0024).
///
/// `!Send`, and deliberately so: `crate::wayland::App` owns one of these directly (docs/adr/0039),
/// so a Lua closure, a scene reconcile, and the EGL context are all reachable from one another
/// without a channel hop.
///
/// **The field order below is load-bearing: `loader` must stay last.** Rust drops fields in
/// declaration order, and almost every other field here holds `mlua::Value`s belonging to that
/// `Loader`'s VM -- `scene`'s `RetainedNode::properties`, `state`'s retained `LoadOutput`s,
/// `capability_signals`/`rescue_handle`'s `Rc<RefCell<Value>>`, `process_registry`'s Lua
/// callbacks. Those values do not keep the VM alive (mlua 0.12's `ValueRef` holds a `WeakLua`; see
/// [`ReloadState`]'s doc comment), so declaring `loader` first meant the `Lua` was dropped *before*
/// them. It happens not to crash today only because `ValueRef::Drop` `try_lock`s and no-ops on a
/// dead state and nothing reads a value during teardown -- but any read does lock, and
/// `ValueRef::to_pointer` panics, which a `Debug` format of a `LoadOutput` reaches. Do not reorder
/// `loader` back up.
pub struct RendererClient {
    shell_lua_path: PathBuf,
    scene: Scene,
    /// The `(surface, output)` pairs this generation is currently resolving
    /// (`CONTEXT.md`, Surface instance; `layout::instance::expand_instances`). Owned here rather
    /// than passed per call because three separate paths need the same set --
    /// [`Self::apply_instances`] at startup, [`Self::handle_apply_pending`] on an in-place reload,
    /// and [`Self::re_resolve_if_dirty`] on every capability push -- and only `crate::wayland`
    /// knows the outputs they were expanded from.
    instances: Vec<SurfaceInstance>,
    /// Whether this process is holding, or has just asked for, a session lock -- written only by
    /// `crate::wayland::App::set_session_lock` and its teardown paths, through
    /// [`Self::set_session_locked`].
    ///
    /// It is the arming half of [`lock_stays_authenticatable`], the veto every `Scene::apply` below
    /// carries. See that function for the hole it closes; the short version is that
    /// `SurfaceFingerprint::Lock` carries only the `id`, so editing a lock's `child` reads as
    /// `Unchanged` and reloads *in place*, and an in-place reload that deletes the password field
    /// while the lock is up leaves the session with no way out but a VT switch.
    ///
    /// **A `bool` and not the instance ids, and that is the whole of the fourth review's defect 1.**
    /// This used to hold the `lock` instance ids that existed at the moment the lock was asked for.
    /// `crate::wayland::App::handle_output_change` destroys instances and creates new ones on every
    /// hotplug and never revisited that list, and `Scene` leaves a retired instance's tree behind,
    /// so after a lid close onto a dock the veto went on validating `screen@eDP-1` -- a fossil
    /// nothing can paint -- while the live lock screen was `screen@DP-1`. A snapshot taken at
    /// acquire time cannot survive a hotplug, so there is no snapshot: the veto reads
    /// [`Self::instances`], which `set_instances` keeps current for exactly this reason.
    holds_session_lock: bool,
    /// A clone of the one `ShapingHandle` `crate::wayland::App` also holds -- one worker thread
    /// and one `FontSystem` for the whole process (docs/adr/0023 item 8, closed by docs/adr/0039
    /// decision 3), instead of the second `FontSystem::new()`'s ~1s startup this used to pay.
    shaping: ShapingHandle,
    /// One handle per capability seen so far, keyed by `StateSnapshot.capability`
    /// (docs/adr/0029) -- every `shared::CAPABILITIES` roster name is seeded at construction
    /// (ADR-0037; see `new`'s doc comment); an unrostered capability is added lazily, on the
    /// first `StateSnapshot` that names it, by `apply_state_snapshot`. `RefCell`,
    /// not `&mut self`: `apply_state_snapshot` is called through a `&self` receiver (see its own
    /// doc comment for why), and this is the one piece of `RendererClient` state that read path
    /// needs to mutate.
    capabilities: RefCell<HashMap<String, CapabilityHandle>>,
    /// Cloned into every [`Capability`] this client builds, including the lazy path's. Kept
    /// rather than consumed by the constructor because that lazy path builds a `Capability` long
    /// after `new` has returned.
    commands: CommandSender,
    rescue_handle: LiveSignalHandle,
    /// `oblisk.screens`'s handle (docs/adr/0041 decision 2) -- Renderer-sourced, so it
    /// is deliberately not in `capabilities` above and deliberately not in
    /// `shared::CAPABILITIES` either. See [`register_screens_signal`].
    screens_handle: LiveSignalHandle,
    /// What `screens_handle` currently holds, mirrored as JSON so [`Self::set_screens`] can tell a
    /// real output change from a re-push of the same list -- exactly `rescue_state`'s job below,
    /// and load-bearing for the same reason plus one more: an unchanged re-push would also ask the
    /// Supervisor for a reload cycle it has no reason to run.
    screens_payload: serde_json::Value,
    /// What `rescue_handle` currently holds, mirrored here as a plain tuple so
    /// [`Self::set_rescue_state`] can tell a real change from a no-op rewrite -- see its doc
    /// comment. Seeded to match `register_rescue_signal`'s initial `{ is_rescue = false,
    /// error_log = "" }` table.
    rescue_state: (bool, String),
    process_registry: ProcessRegistry,
    /// The scene-dirty flag (ADR-0044 decision 2, `CONTEXT.md`'s Dirty scene entry). Cloned into
    /// every `LiveSignalHandle` this client hands out (the roster seed, the lazy
    /// `capability_signal` path, `rescue_handle`, and `screens_handle`), so a `set` on any of them
    /// marks this same flag. Two readers, both of which check and clear in one step:
    /// `re_resolve_if_dirty`, and `apply_instances` -- which is itself a full resolve against
    /// every current value, so a mark made before it has already been satisfied by it.
    dirty: DirtyFlag,
    state: ReloadState,
    /// Where a `ReevaluateReport` goes: the socket thread's [`pump`] drains this and writes each
    /// frame to the wire. `UnboundedSender::send` is synchronous and non-blocking, so this is
    /// callable straight from the Wayland dispatch thread.
    outbound_tx: mpsc::UnboundedSender<RendererFrame>,
    /// The `oblisk` table itself (build-steps.md Phase 25 item 3), held so
    /// [`Self::capability_handle`]'s lazy path can add a member to it after construction. Below
    /// every other retained value and above `loader` for the drop-order reason this struct's own
    /// doc comment gives: it is an `mlua` value like the signals above it.
    oblisk: mlua::Table,
    /// Last, and that is load-bearing -- see this struct's own doc comment. Every field above
    /// holds `mlua::Value`s from this VM, and reading one from a dead `Lua` panics.
    loader: Loader,
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
        // One flag for this whole generation (ADR-0044 decision 2), created before anything that
        // hands out a `LiveSignalHandle` so the rescue signal shares it too, and before the loader
        // so the `state(name, initial)` global it registers marks this same flag (decision 5).
        let dirty = DirtyFlag::new();
        // The directory holding the `shell.lua` about to be evaluated, which is where `require`
        // will look and nowhere else (ADR-0047 decision 1). Taken from the resolved path rather
        // than by asking `shared::config_dir()` a second time, so the file and its module search
        // path can never disagree.
        let config_dir = shell_lua_path.parent().ok_or("shell.lua's path has no parent directory")?.to_path_buf();
        let loader = Loader::new(dirty.clone(), &config_dir).map_err(|err| format!("failed to start the Lua loader: {err}"))?;
        let process_registry = ProcessRegistry::new(generation_id, outbound_tx.clone());
        loader.register_process(process_registry.clone()).map_err(|err| format!("failed to register the process global: {err}"))?;
        // The one write path § 3.2's commands all take (build-steps.md Phase 25 item 1), stamped
        // with the same generation id `ProcessRegistry` above stamps, from the same source: § 7.3's
        // guard rule drops a packet whose generation is stale, so a second source for it would be a
        // second way to be silently ignored.
        let commands = CommandSender::new(generation_id, outbound_tx);
        let client = Self::new(loader, shell_lua_path, shaping, commands, process_registry, dirty)
            .map_err(|err| format!("failed to build the `oblisk` namespace: {err}"))?;
        Ok(client)
    }

    /// Builds the whole `oblisk` namespace (build-steps.md Phase 25 item 3): every
    /// `shared::CAPABILITIES` roster name, the two Renderer-sourced signals `rescue` and
    /// `screens`, and `version`. Before this every capability but `lock` was a bare global, which
    /// made every § 2 example in the IDL wrong about the name it used.
    ///
    /// **Every roster name is pre-seeded here**, not left to [`Self::capability_handle`]'s lazy
    /// path, so a `shell.lua` that reads any rostered capability before its first real push gets
    /// a live signal (reading `nil` inside it) instead of an index-into-nil Lua error and rescue.
    /// That is ADR-0037's uniform nil-until-hydrated contract. The previous hand-listed four-name
    /// seed (audio/network/bluetooth/tray) froze at ADR-0031 while six more capabilities landed,
    /// exactly the drift a hand-list guarantees; the roster is the single source both processes
    /// share. The lazy path stays as the fallback for unrostered names.
    ///
    /// **One table, so a typo is a Lua error rather than silence.** A bare global that does not
    /// exist reads `nil` and a config gets "attempt to index a nil value" at the use site with no
    /// hint that the *name* was the problem. Inside a table the same typo is still `nil`, so this
    /// buys nothing on its own -- what it buys is the collision the bare form could not avoid.
    /// § 6.4's `lock` node constructor owns the global `lock`, and seeding a bare `lock` signal
    /// silently overwrote it and broke every `lock { ... }` declaration in the file that declares
    /// the lock screen (docs/adr/0052 decision 1 found that the hard way). The engine's DSL and
    /// § 2's state now live in separate namespaces and cannot collide again, whatever § 2 grows.
    fn new(
        loader: Loader,
        shell_lua_path: PathBuf,
        shaping: ShapingHandle,
        commands: CommandSender,
        process_registry: ProcessRegistry,
        dirty: DirtyFlag,
    ) -> mlua::Result<Self> {
        // Taken off `commands` rather than passed alongside it, so this constructor stays inside
        // clippy's argument limit and so there is visibly one channel rather than two clones of
        // one that could drift apart.
        let outbound_tx = commands.frames();
        let oblisk = loader.create_table()?;
        let mut seeded = HashMap::new();
        for capability in shared::CAPABILITIES {
            let (member, handle) = Capability::new(capability, dirty.clone(), commands.clone());
            oblisk.set(*capability, member)?;
            seeded.insert((*capability).to_string(), handle);
        }
        let rescue_handle = register_rescue_signal(&loader, &oblisk, dirty.clone())?;
        // Seeded to an empty list (not `nil`) so a config that loops over `oblisk.screens`
        // iterates zero times rather than erroring, and set through `new_live`'s initial value
        // rather than a `set` so seeding it does not mark the scene dirty before anything has
        // ever been applied.
        let screens_payload = serde_json::Value::Array(Vec::new());
        let screens_handle = register_screens_signal(&loader, &oblisk, dirty.clone(), &screens_payload)?;
        oblisk.set("version", version_table(&loader)?)?;
        // The directory the config was loaded from, so a config can name a file it ships beside
        // itself (build-steps.md Phase 29 item 4). ADR-0047 made the config a directory rather
        // than a file, which makes it a place to put a wallpaper, an icon or a sound, and until
        // now nothing in Lua could say where that place is. A string beside `version` rather than
        // a capability: it is static process information, not something that pushes.
        //
        // The parent of `shell.lua` rather than a second call to `shared::config_dir()`, so this
        // cannot disagree with the file actually loaded.
        oblisk.set(
            "config_dir",
            shell_lua_path.parent().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default(),
        )?;
        loader.set_global("oblisk", oblisk.clone())?;
        Ok(Self {
            shell_lua_path,
            scene: Scene::new(),
            instances: Vec::new(),
            holds_session_lock: false,
            shaping,
            capabilities: RefCell::new(seeded),
            commands,
            rescue_handle,
            screens_handle,
            screens_payload,
            // Matches the table `register_rescue_signal` already put in the signal.
            rescue_state: (false, String::new()),
            process_registry,
            dirty,
            state: ReloadState { applied_topology: None, applied_output: None, pending: None },
            outbound_tx,
            oblisk,
            loader,
        })
    }

    /// Writes the `rescue` global's `{ is_rescue, error_log }` table -- but only when the value
    /// actually differs from what is already in there.
    ///
    /// The early return is a correctness fix, not an optimization. `rescue_handle` is a
    /// `LiveSignalHandle` like any capability's, so writing through it marks the shared
    /// `DirtyFlag` (ADR-0044 decision 2), and two of this method's callers write on their
    /// *success* paths: `run_startup_evaluation` clears rescue right after a successful
    /// `Scene::apply`, and `handle_reevaluate` clears it before every verdict. Rewriting an
    /// unchanged value therefore marked the scene dirty when nothing had changed, so a clean
    /// startup made the first poll turn redo a whole `Scene::apply` for nothing, and -- worse -- a
    /// `TopologyChanged` verdict left the flag set, so the next turn mutated the scene of a
    /// generation that must not have its scene mutated at all (that case is a generation swap,
    /// Phase 14; see this module's doc comment point 2).
    ///
    /// This is not the memoization ADR-0044 decision 3 rejects. Decision 3 is about not caching a
    /// signal's *resolved* value across reads; this is about a write that stores nothing new not
    /// claiming the scene changed. A genuine rescue transition still marks dirty and still
    /// re-resolves, because `rescue` is a live signal a config may read like any other.
    ///
    /// `pub` for the third caller, which is outside this module: `crate::wayland::App`'s
    /// `SessionLockHandler`, which docs/adr/0052 decision 4 requires to set `rescue` on a refused
    /// lock and on both `finished` cases, because in each of them the ordinary scene is what is on
    /// the glass and `rescue` is the only channel that reaches the user. The ADR is explicit that
    /// the Renderer sets that signal itself rather than round-tripping it through the Supervisor,
    /// and this is the one write path to it.
    pub fn set_rescue_state(&mut self, is_rescue: bool, error_log: &str) {
        if self.rescue_state.0 == is_rescue && self.rescue_state.1 == error_log {
            return;
        }
        match rescue_table(&self.loader, is_rescue, error_log) {
            Ok(table) => {
                self.rescue_handle.set(mlua::Value::Table(table));
                self.rescue_state = (is_rescue, error_log.to_string());
            }
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
        // Value and revision together (build-steps.md Phase 25 item 2): the revision is what a
        // later `oblisk.<name>:invoke(...)` stamps onto its envelope for § 7.3's staleness guard,
        // and this push is the only thing that moves it.
        self.capability_handle(&snapshot.capability)?.hydrate(value, snapshot.revision);
        Ok(())
    }

    /// Looks up `capability`'s handle, adding a fresh `oblisk.<capability>` member (value `nil`,
    /// revision `0`) the first time this capability is ever seen (docs/adr/0029). Every later
    /// `StateSnapshot` for the same capability reuses the same handle instead of rebuilding the
    /// member on every push.
    ///
    /// Unreachable in a debug build, where `supervisor::snapshot::push_snapshot`'s
    /// `debug_assert` rejects an off-roster capability before it is ever sent. This is what
    /// happens in release instead of a panic: the capability appears under `oblisk` and works.
    ///
    /// **It refuses to overwrite a name the table already holds**, which is the one thing this
    /// path must not do. `oblisk` also carries `rescue`, `screens` and `version`, none of which
    /// are capabilities, and `Table::set` over an existing key says nothing. An off-roster push
    /// named `rescue` would replace the signal that reports config failures with an empty one,
    /// and the symptom would be a shell that stops reporting its own breakage. That is ADR-0052
    /// decision 1's bug exactly, one level down: a silent `set` over a name something else owns.
    /// Refusing logs through `handle_frame`'s existing error path instead.
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

    /// Evaluates `shell.lua` once at startup and applies it directly -- no round trip through the
    /// Supervisor needed, since there's no prior applied scene to protect yet (build-steps.md
    /// Phase 13). Leaves `state.applied_topology` at `None` when the *evaluation* fails: a startup
    /// failure leaves the shell blank (docs/adr/0024 item 4) -- `CONTEXT.md`'s Rollback guarantee
    /// is about a *re*-evaluation keeping its prior scene, and there is no prior scene on first
    /// boot. Because `None` also means "safe to apply" (not "topology []"), a later successful
    /// `Reevaluate` can still recover from this state instead of being stuck forever.
    ///
    /// A failed *apply* no longer clears it, which is the one behavior this split moved rather
    /// than preserved. That is the correct side of the line since docs/adr/0038: the caller has
    /// already bound the declared surfaces by then, so they exist and a later edit that changes the
    /// topology genuinely needs a new generation to build a different set. The old rule would have
    /// applied that edit in place, into surfaces the config no longer describes. See the module doc
    /// comment point 3.
    ///
    /// Runs before any layer surface is bound (`oblisk-supervisor-services-dbus.md` § 15.2's
    /// order: evaluate, bind, null-buffer, signal ready), which on one thread is just the order
    /// of the statements in `crate::wayland::run`.
    ///
    /// Split from the scene apply since build-steps.md Phase 20, and the split is forced by that
    /// same § 15.2 ordering rather than chosen: the caller needs the returned
    /// [`SurfaceSpec`](layout::node::SurfaceSpec)s to expand into surface instances
    /// (`layout::instance::expand_instances`) *before* there is anything to resolve a tree against,
    /// so evaluation has to hand its result back rather than consume it. [`Self::apply_instances`]
    /// is the other half.
    ///
    /// Returns `None` when the evaluation itself failed, with rescue set exactly as before.
    pub fn run_startup_evaluation(&mut self) -> Option<Vec<SurfaceSpec>> {
        match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                self.state.applied_topology = Some(specs.iter().map(SurfaceSpec::fingerprint).collect());
                // ADR-0044 decision 2: hold the evaluation that was actually applied, so a
                // later push can re-resolve against it without re-running shell.lua.
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

    /// Replaces the `(surface, output)` pairs this generation resolves against
    /// (`layout::instance::expand_instances`'s output). Called from `crate::wayland::run` between
    /// the startup evaluation and the first apply, and again from `crate::wayland::App`'s
    /// `OutputHandler` on every monitor hotplug (docs/adr/0038 decision 3).
    pub fn set_instances(&mut self, instances: Vec<SurfaceInstance>) {
        self.instances = instances;
    }

    /// Arms or disarms the lock-authentication veto every `Scene::apply` in this module carries
    /// (docs/adr/0052 decision 3). `crate::wayland::App::set_session_lock` arms it the moment it
    /// asks the compositor for the lock -- not when `locked` arrives, because a reload landing
    /// inside that window would strip the field out of the tree the compositor is about to show --
    /// and every path that gives the lock up disarms it.
    ///
    /// Carries no instance ids: which surfaces the veto has to defend is a question only the apply
    /// that is running can answer, and [`lock_stays_authenticatable`] asks it there. See
    /// [`Self::holds_session_lock`].
    pub fn set_session_locked(&mut self, locked: bool) {
        self.holds_session_lock = locked;
    }

    /// The set [`Self::set_instances`] last stored, so `crate::wayland::App` can diff a fresh
    /// expansion against it (`layout::instance::reconcile_instances`) without keeping a second
    /// copy that could drift from this one.
    pub fn instances(&self) -> &[SurfaceInstance] {
        &self.instances
    }

    /// The whole declared surface roster of the evaluation currently applied, re-parsed from the
    /// retained `applied_output` rather than re-read from `shell.lua`.
    ///
    /// This is what a monitor hotplug expands against (docs/adr/0038 decision 3): the declared set
    /// is unchanged by an output appearing, so the specs already in hand are exactly the right
    /// ones, and re-evaluating the file here would both cost an evaluation and race the
    /// `Reevaluate` the Supervisor is about to send anyway (docs/adr/0041 decision 4).
    ///
    /// Empty when nothing has ever applied (a startup evaluation that failed), which is the
    /// honest answer: there are no declared surfaces to expand, so a hotplug adds none.
    pub fn applied_surface_specs(&self) -> Vec<SurfaceSpec> {
        let Some(output) = self.state.applied_output.as_ref() else {
            return Vec::new();
        };
        match surface_specs(output) {
            Ok(specs) => specs,
            Err(err) => {
                // Unreachable in practice -- `applied_output` is only ever stored after
                // `surface_specs` already succeeded on it -- but a panic here would take down a
                // shell that is painting fine, over an output event.
                eprintln!("control-socket client: the applied evaluation's surface specs no longer parse: {err}");
                Vec::new()
            }
        }
    }

    /// Asks the Supervisor to start a reload cycle (docs/adr/0041 decision 4). Sent when the
    /// output list changed, because a config that loops over `screens` declares a different set of
    /// surfaces before and after, which is a topology change and so a generation swap
    /// (docs/adr/0041 decision 3) -- a decision only the Supervisor makes.
    ///
    /// Carries no sequence: `supervisor/src/main.rs` owns `next_sequence` and drops any report
    /// that does not name the one it most recently sent, so this asks it to *begin* a cycle rather
    /// than fabricating one. Everything after that -- the `Reevaluate` coming back, the topology
    /// diff, the verdict -- is the existing path unchanged.
    pub fn request_reload(&self) {
        if let Err(err) = self.outbound_tx.send(RendererFrame::RequestReload) {
            eprintln!("control-socket client: failed to request a reload after an output change: {err}");
        }
    }

    /// Pushes the `screens` signal's new value (docs/adr/0041 decision 2) and reports whether it
    /// actually changed.
    ///
    /// The early return is the same correctness rule `set_rescue_state` documents, with one extra
    /// consequence: `update_output` fires for changes this signal does not carry, and re-pushing
    /// an identical list would both mark the scene dirty for nothing and -- since the caller gates
    /// [`Self::request_reload`] on this return value -- ask the Supervisor for a reload cycle no
    /// output change justifies.
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

    /// One instance's `available` size, replaced by the size the compositor actually configured
    /// that surface to, and the scene marked dirty so the next poll turn re-resolves it
    /// (build-steps.md Phase 20 item 4, closing docs/adr/0023 item 6).
    ///
    /// Reuses the one [`DirtyFlag`] ADR-0044 decision 2 already established rather than adding a
    /// second "something changed" mechanism next to it: a configure and a capability push both
    /// mean the same thing to the scene, that the resolved geometry no longer matches its inputs.
    /// An unknown `instance_id` is ignored -- a `configure` for a surface no instance names should
    /// not exist, and silently doing nothing is better than marking the whole scene dirty over it.
    pub fn set_instance_size(&mut self, instance_id: &str, size: layout::LogicalSize) {
        let Some(instance) = self.instances.iter_mut().find(|i| i.instance_id == instance_id) else {
            return;
        };
        if instance.available == size {
            // Same early return, for the same reason, as `set_rescue_state`'s: a configure that
            // repeats a size the scene already resolved against must not claim the scene changed,
            // or every duplicate configure buys a whole `Scene::apply`.
            return;
        }
        instance.available = size;
        self.dirty.mark();
    }

    /// Resolves the last evaluation against the current instance set, setting rescue on failure.
    /// The second half of [`Self::run_startup_evaluation`]'s split; returns whether the apply
    /// succeeded, so the caller (`crate::wayland::run`) can tell a Candidate that must exit from
    /// one that may carry on.
    ///
    /// Takes no instance argument on purpose: [`Self::set_instances`] is the one place the set is
    /// written, and the same stored set is what [`Self::re_resolve_if_dirty`] and
    /// [`Self::handle_apply_pending`] resolve against. Two sources for it would let a startup
    /// apply and a later push disagree about which surfaces exist.
    pub fn apply_instances(&mut self) -> bool {
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        let (instances, locked) = (&self.instances, self.holds_session_lock);
        let applied = self.scene.apply_admitting(&output.surfaces, instances, &self.shaping, self.loader.lua(), |scene| {
            lock_stays_authenticatable(scene, instances, locked)
        });
        match applied {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &self.instances);
                // Nothing holds a lease, so nothing can be holding a subtree this apply
                // retired -- see `Scene::release_all_retired`, including when that stops
                // being true.
                self.scene.release_all_retired();
                self.set_rescue_state(false, "");
                // This apply resolved against every signal's *current* value, so anything marked
                // dirty before it is already accounted for -- notably `crate::wayland::run`'s
                // `set_screens` seed, which must run before the startup evaluation (docs/adr/0041
                // decision 2: a config looping over `screens` at startup would otherwise see an
                // empty list) and which marks the flag like any other live-signal write. Without
                // this, a clean startup would enter its poll loop dirty and buy one whole
                // redundant `Scene::apply` before drawing anything.
                //
                // Only on success: a failed apply leaves the prior state standing, and the flag
                // with it, so whatever marked it is still picked up by the next apply that works.
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
    /// resolved tree up in by the same `"{id}@{output}"` id its `TrackedSurface` carries. Private
    /// until docs/adr/0038 deleted the fixed role enum and made those two id spaces one -- before that
    /// there was no surface a lookup could hit (build-steps.md Phase 19 item 6).
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// This generation's `Lua`, for building the one argument `button`'s `on_click` takes
    /// (docs/adr/0050 decision 3). `crate::wayland::App` holds the resolved tree's
    /// `mlua::Function` but no VM to construct a `Table` in, and every other value it hands Lua
    /// today it got *from* Lua -- this is the first one it makes.
    ///
    /// Narrower than it looks: `Loader::lua` has been public within the crate all along, and the
    /// only thing this adds is a path to it that does not make `loader` itself public. Callers
    /// must not hold the borrow across the Lua call it feeds; see
    /// [`crate::wayland::App::fire_on_click`].
    pub fn lua(&self) -> &mlua::Lua {
        self.loader.lua()
    }

    /// Handles one inbound `SupervisorFrame`, decoded off the wire by [`pump`] and handed over by
    /// `crate::wayland::run`'s poll loop.
    ///
    /// Returns a [`FrameOutcome`]: `Handled` for every frame this can finish on its own, and one
    /// of the two hand-backs for the two it cannot. Both of those need state that lives on
    /// `crate::wayland::App` and not here -- the EGL and surface state a draw needs (§ 15.3), and
    /// SCTK's `SessionLockState` plus the lock surfaces a `SetSessionLock` needs (docs/adr/0042).
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
            // Handed straight back, for `ActivateDraw`'s reason and no other: `ext_session_lock_v1`
            // is a Wayland object, so every part of servicing this -- taking the lock, creating one
            // `ext_session_lock_surface_v1` per output, tearing them down again -- lives on
            // `crate::wayland::App` (docs/adr/0042, docs/adr/0052 decision 1). Nothing about it can
            // be decided here: whether the config even declares a `lock` surface is a question about
            // the tracked surface set, not about the scene this module owns.
            SupervisorFrame::SetSessionLock(SetSessionLock { locked }) => return FrameOutcome::SetSessionLock(locked),
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
        FrameOutcome::Handled
    }

    /// Runs one `Reevaluate` request: evaluates `shell.lua`, classifies the result against
    /// `state.applied_topology`, updates `state.pending` and the rescue signal, and queues the
    /// verdict on the outbound channel. `applied_topology == None` (no evaluation has produced
    /// one, e.g. after a startup evaluation failure) is treated as "not changed" -- there's
    /// nothing to protect, so the fresh evaluation is safe to stage as `pending` -- see the module
    /// doc comment point 3.
    ///
    /// The diff reads each spec's [`SurfaceFingerprint`](layout::node::SurfaceFingerprint) and
    /// nothing else, which is the swap-versus-in-place split itself (docs/adr/0038 decision 2,
    /// docs/adr/0049 decision 3, `CONTEXT.md`'s Topology change/Value change): an edit to a
    /// `margin`, a `keyboard_interactivity`, an `exclusive`, a size, or a `window`'s `title` is a
    /// request the protocol accepts on a live object, so it must report `Unchanged` and reload in
    /// place rather than respawning the process. Comparing whole specs would make every one of
    /// those a generation swap. What the fingerprint does catch, for every role, is a declaration
    /// appearing or disappearing.
    fn handle_reevaluate(&mut self, request: ReevaluateRequest) {
        let report = match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                let topology: Vec<SurfaceFingerprint> = specs.iter().map(SurfaceSpec::fingerprint).collect();
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
        let (instances, locked) = (&self.instances, self.holds_session_lock);
        match self.scene.apply_admitting(&output.surfaces, instances, &self.shaping, self.loader.lua(), |scene| {
            lock_stays_authenticatable(scene, instances, locked)
        }) {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &self.instances);
                self.scene.release_all_retired();
                // Both fields, on a successful apply only. `run_startup_evaluation` sets them a
                // step earlier, on a successful evaluation, because § 15.2 makes it hand its
                // specs back before any surface exists to resolve against. The two rules cannot
                // disagree here: an `ApplyPendingReload` only ever follows an `Unchanged` verdict,
                // which the diff reached by finding this exact topology already stored.
                self.state.applied_topology = Some(topology);
                // ADR-0044 decision 2's re-resolve target.
                self.state.applied_output = Some(output);
                // An in-place reload changed the retained scene, and until this line nothing told
                // the screen. `crate::wayland::run`'s poll loop only repaints -- and, since
                // build-steps.md Phase 20 items 1 and 5, only pushes each surface's `visible`,
                // in-place layer-shell fields and input region -- when `re_resolve_if_dirty`
                // reports a change, so an edit that reached here and stopped sat in memory until
                // some unrelated capability push happened to mark the flag. On a live session that
                // hid the bug (a push lands every few seconds) and on a static config it would not
                // have.
                //
                // The cost is one redundant `Scene::apply` on the next poll turn, paid once per
                // file save, which is a human-scale event. That is the right trade against
                // inventing a second "something changed" signal beside the one flag ADR-0044
                // decision 2 established -- the same argument `set_instance_size` already makes.
                // Safe here specifically because an `ApplyPendingReload` only ever follows an
                // `Unchanged` verdict: this generation's scene *is* the one that should be
                // mutated, which is the case this module's doc comment point 2 contrasts with a
                // `TopologyChanged` verdict.
                self.dirty.mark();
            }
            Err(err) => eprintln!("control-socket client: ApplyPendingReload's stored evaluation failed to apply: {err}"),
        }
    }

    /// Re-runs `Scene::apply` against `state.applied_output` when a live signal's `set` has
    /// marked the scene dirty since the last resolve (ADR-0044 decision 2, `CONTEXT.md`'s Dirty
    /// scene entry). Does not touch `shell.lua`: the retained `VirtualNode` tree in
    /// `applied_output` still holds the `Signal` handles Lua put there, so re-applying it reads
    /// their current values through decision 1's resolve-at-layout-time rule. What keeps those
    /// stale `mlua::Value`s readable is this client's own field ordering, not anything the values
    /// enforce themselves -- see [`ReloadState`]'s doc comment.
    ///
    /// `crate::wayland::run`'s poll loop calls this once per turn, *after* draining every pending
    /// inbound frame and *before* servicing that turn's `ActivateDraw`, not once per frame:
    /// `DirtyFlag::take` collapses however many pushes arrived in that drain into a single `true`,
    /// so a burst of `StateSnapshot`s costs one re-resolve, not one per push.
    ///
    /// Returns whether it actually re-resolved, which is what tells `crate::wayland::run`'s poll
    /// loop whether to repaint. `false` covers both "nothing was dirty" and "the re-resolve
    /// failed and the prior scene still stands", and both mean the same thing to the caller:
    /// nothing on screen needs redrawing.
    pub fn re_resolve_if_dirty(&mut self) -> bool {
        // `applied_output` is checked *before* the flag is taken, and that order is the whole
        // point. With nothing to re-resolve against (startup failed, or no reload has ever
        // landed) there is nothing this call can do, so consuming the flag would silently discard
        // the push that set it. Taking it first meant a config that failed its first apply
        // swallowed every subsequent push and stayed blank until an inotify edit forced a
        // re-evaluation. Left set, the flag is picked up by whatever applies next.
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        // Read and clear in one step, on the path that actually acts on it.
        if !self.dirty.take() {
            return false;
        }
        let (instances, locked) = (&self.instances, self.holds_session_lock);
        let applied = self.scene.apply_admitting(&output.surfaces, instances, &self.shaping, self.loader.lua(), |scene| {
            lock_stays_authenticatable(scene, instances, locked)
        });
        if let Err(err) = applied {
            // `Scene::apply` rolls back to its exact pre-call state on error (see its own doc
            // comment), so the prior good scene is still applied and still on screen. This is
            // deliberately not routed to `set_rescue_state`: rescue means shell.lua failed to
            // evaluate, and this is a capability push that some property's parser rejected (e.g.
            // a numeric field pushed where `content` wants a string). Entering rescue here would
            // flap the whole shell to an error screen over one capability's transient bad value,
            // which is a worse outcome than just keeping the last good frame.
            //
            // ponytail: logging is the only signal this gets. A config with a genuinely broken
            // signal-valued property (a `content` bound to a capability field that is sometimes
            // not a string) now logs this line on every push forever, with nothing user-visible
            // telling anyone something is wrong. The upgrade path is a distinct rescue-adjacent
            // channel for "a pushed value was rejected", separate from "the config didn't
            // evaluate" -- not built here since nothing has needed it yet.
            eprintln!("control-socket client: dirty-scene re-resolve failed, keeping the prior scene: {err}");
            return false;
        }
        // The one path where an undrained lease bag actually leaks at cadence: a re-resolve that
        // shortens a `children` list retires the tail on every poll turn that carries a push.
        // Same "nothing holds a lease" argument as the other two apply sites -- see
        // `Scene::release_all_retired`.
        self.scene.release_all_retired();
        true
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
        RendererFrame::LockReport(_) => "LockReport",
        RendererFrame::RequestReload => "RequestReload",
    }
}

/// Builds the `{ is_rescue, error_log }` table and hangs it off the `oblisk` table as
/// `oblisk.rescue` (§ 2.10). Returns the handle so later evaluations can update it.
///
/// A bare `lua::signal::Signal` and not a [`Capability`], for the same reason
/// [`register_screens_signal`] is: this is Renderer-sourced, has no `dispatch` on the Supervisor
/// side and no roster entry, so an `invoke` on it could only ever be a command the Supervisor
/// drops. Reading is identical either way, since `Capability` delegates `get`/`map` to a wrapped
/// `Signal`, so a config author sees one shape and only the writable things are writable.
fn register_rescue_signal(loader: &Loader, oblisk: &mlua::Table, dirty: DirtyFlag) -> mlua::Result<LiveSignalHandle> {
    let table = rescue_table(loader, false, "")?;
    let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Table(table), dirty);
    oblisk.set("rescue", signal)?;
    Ok(handle)
}

/// Registers the reactive `oblisk.screens` signal (docs/adr/0041 decision 2), seeded with
/// `initial`. ADR-0041 wrote the name with its namespace from the start; until Phase 25 item 3
/// there was no `oblisk` table to put it in and it was a bare global instead.
///
/// Deliberately outside `shared::CAPABILITIES` and outside `capabilities`, which is the
/// first exception to the shape ADR-0037 established and is stated as such in ADR-0041 decision 2:
/// this is sourced in the Renderer from `smithay_client_toolkit`'s `OutputState`, not pushed by
/// the Supervisor as a `StateSnapshot`, so the roster (which is the Supervisor's own dispatch and
/// push list) has nothing to say about it. It sits in the same table anyway, because the table is
/// what a config reads and § 2.15 names this `oblisk.screens` like everything else in § 2.
fn register_screens_signal(
    loader: &Loader,
    oblisk: &mlua::Table,
    dirty: DirtyFlag,
    initial: &serde_json::Value,
) -> mlua::Result<LiveSignalHandle> {
    let (signal, handle) = lua::signal::Signal::new_live(loader.to_lua_value(initial)?, dirty);
    oblisk.set("screens", signal)?;
    Ok(handle)
}

/// This Renderer binary's version as `{ major, minor, patch }` integers, from Cargo's own
/// `CARGO_PKG_VERSION_*` (build-steps.md Phase 25 item 4).
///
/// A plain table, not a signal: it cannot change while the process runs. It is registered on the
/// day the namespace is built rather than on the day a config needs it, because it is hostile to
/// retrofit -- a config written before any version exists has nothing to guard on, forever, and
/// the marginal cost here is one table.
///
/// The shape is deliberately not Quickshell's `Quickshell.hasVersion(major, minor, features)`.
/// That call carries a feature-name list, which answers a question Oblisk does not have yet and
/// costs a registry of feature names to maintain; three integers a config can compare are the
/// same guard without it.
///
/// The Renderer's version and not the Supervisor's, and they are the same number today because
/// the workspace versions both together. The day they diverge this is still the right one: it is
/// the process that hosts the VM and defines the API a config is written against.
fn version_table(loader: &Loader) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    let [major, minor, patch] = version_parts();
    table.set("major", major)?;
    table.set("minor", minor)?;
    table.set("patch", patch)?;
    Ok(table)
}

/// `expect` rather than a `0` fallback: Cargo derives these three from the `version` field it has
/// already parsed as semver, so a non-numeric one means the build is broken, and a version table
/// that quietly reads `0.0.0` is worse than not booting -- a config would guard on it and take
/// the wrong branch forever.
fn version_parts() -> [u32; 3] {
    [env!("CARGO_PKG_VERSION_MAJOR"), env!("CARGO_PKG_VERSION_MINOR"), env!("CARGO_PKG_VERSION_PATCH")]
        .map(|part| part.parse().expect("Cargo's CARGO_PKG_VERSION_* are the numeric components of an already-parsed semver"))
}

fn rescue_table(loader: &Loader, is_rescue: bool, error_log: &str) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    table.set("is_rescue", is_rescue)?;
    table.set("error_log", error_log)?;
    Ok(table)
}

/// Parses **every** declared surface by its own role and returns the whole roster (§ 6.1-6.4,
/// build-steps.md Phase 20 item 3, Phase 22 and Phase 23). Was `surfaces_topology`, returning only the swap
/// fingerprint, then `panel_specs`, which parsed all three roles and returned only the panels;
/// build-steps.md Phase 22 is what made every role's spec something a caller actually needs, so all
/// three come back now.
///
/// A surface whose fields don't type-check fails with [`lua::LoaderError::InvalidTopology`] -- a
/// distinct message from an actual top-level-return shape error, since conflating the two (as an
/// earlier version of this function did) produced a misleading `rescue.error_log`.
///
/// **Parsing every role here is the point, even for properties nothing on this path sends.** § 6.2's
/// and § 6.3's properties become requests that raise protocol errors -- a zero `anchor_rect` leaves
/// the positioner incomplete and `get_popup` answers `invalid_positioner`, a `max_size` under a
/// `min_size` answers `invalid_size` -- and a protocol error kills the connection and the whole
/// shell with it. Phase 20 settled that a config typo is a `layout::node::LayoutError` at
/// evaluation, never a protocol error at runtime, so the parse has to happen at the one point every
/// declaration passes through. Failing here puts the message in `rescue`'s `error_log` for a human
/// (§ 2.10, docs/adr/0046) instead of leaving a popup that silently refuses to open on some later
/// click.
///
/// **This is an evaluation-time, literal-only fast-fail, not the authoritative spec** for the two
/// roles whose properties are meant to move (docs/adr/0049's second amendment). It parses the
/// *unresolved* properties, so a `Signal` in a `window`'s `title` or a `popup`'s `anchor_rect` has
/// not been read at all when this runs. Such a property is **skipped** rather than rejected --
/// `layout::node::is_deferred_signal` is that skip, and every § 6.2/§ 6.3 parser consults it -- and
/// the spec it yields carries that parser's documented placeholder in its place. A *literal* is
/// validated here in full, so a typo fails fast. `crate::wayland::App::apply_resolved_state` builds
/// the authoritative [`WindowSpec`](layout::node::WindowSpec) and
/// [`PopupSpec`](layout::node::PopupSpec) from the *resolved* tree instead, where
/// `layout::node::resolve_properties` has already run exactly once for that pass (ADR-0044
/// decision 1), and it does so before anything is built from either.
///
/// The skip is what makes the amendment's own worked example compile: § 6.3 says `anchor_rect` is
/// "normally passed straight from the rect `button`'s `on_click` hands back", and docs/adr/0050
/// decision 3 spells that as `anchor_rect = menu_anchor` over a `state` signal. Before the skip
/// existed every parser answered a raw `Value::UserData` with a type error, so the one spelling the
/// docs prescribe failed the whole evaluation and a config had to write `menu_anchor:get()` --
/// freezing the rect at whatever the file last saw, which for a dropdown means opening over the
/// button clicked before the last reload.
///
/// Resolving here instead is not the upgrade path and never was: `resolve_properties` runs Lua
/// getters, and this function also runs on every monitor hotplug via
/// [`RendererClient::applied_surface_specs`], where a second read of every signal would both double
/// ADR-0021's per-getter budget and break the one-read-per-pass rule.
///
/// What *is* authoritative here is the roster and the fingerprint: which surfaces were declared, in
/// what order, with what role. That is exactly what a topology diff and an instance expansion need,
/// and none of it is a `Signal`'s to move -- `layout::node`'s structural-field rejection refuses one
/// in an `id`.
///
/// **§ 6.4's `lock` is authoritative here in full, and it is the only role that is** (docs/adr/0052
/// decision 2). The two-pass split above exists for properties that are meant to move; a `lock` has
/// none. Its property list is `id` and `child`, `id` is structural, and `child` is the scene's to
/// walk, so [`lock_spec`](layout::node::lock_spec) consults `is_deferred_signal` nowhere and there
/// is no second pass over a lock's properties to be the real one. The four properties it refuses
/// are refused on the *key*, so a config writing `visible = some_signal` on a lock screen fails
/// here exactly as `visible = false` does -- deferring that would be deferring a check on a
/// property that will never legally exist.
fn surface_specs(output: &lua::LoadOutput) -> Result<Vec<SurfaceSpec>, lua::LoaderError> {
    let invalid = |err: layout::node::LayoutError| lua::LoaderError::InvalidTopology(err.to_string());
    let mut specs = Vec::with_capacity(output.surfaces.len());
    for surface in &output.surfaces {
        specs.push(match surface.kind.as_str() {
            "panel" => SurfaceSpec::Panel(layout::node::panel_spec(&surface.properties).map_err(invalid)?),
            "window" => SurfaceSpec::Window(layout::node::window_spec(&surface.properties).map_err(invalid)?),
            "popup" => SurfaceSpec::Popup(layout::node::popup_spec(&surface.properties).map_err(invalid)?),
            "lock" => SurfaceSpec::Lock(layout::node::lock_spec(&surface.properties).map_err(invalid)?),
            // Unreachable: `lua::require_surface` admits exactly the four § 6 roles above and
            // rejects everything else. Named rather than left to a silent `_ => {}`, because that
            // arm would let a fifth role reach a generation unvalidated -- which is not
            // hypothetical, since `lock` spent Phase 22 as a role this match had no arm for.
            other => return Err(lua::LoaderError::InvalidTopology(format!("`{other}` is not a surface role"))),
        });
    }
    // **At most one `lock` in a config, and this is the only place that can say so.** Every other
    // § 6 role may be declared any number of times, so the check is a property of the surface *set*
    // rather than of any one spec, and this is the one function every declaration passes through on
    // both the startup path and the `Reevaluate` path -- rejecting here is what keeps a second
    // declaration from ever reaching a generation.
    //
    // The failure it prevents is unrecoverable rather than cosmetic.
    // `layout::instance::expand_instances` emits one instance per lock spec per output, so two
    // declarations make `crate::wayland::App::ensure_lock_surfaces` send two `get_lock_surface` for
    // the same `wl_output`, and `ext-session-lock-v1` is explicit: "Attempting to create more than
    // one lock surface for a given output is a duplicate_output protocol error." The compositor
    // disconnects the client, it does not unlock the session when a lock client dies, and the user
    // is left with a VT switch as the only way back in.
    //
    // There is also nothing coherent to admit. § 6.4 gives a `lock` no `monitor` and exactly one
    // surface per output, so "two lock screens" names no arrangement a compositor could show --
    // unlike two `panel`s, which are two strips of glass.
    let locks = specs.iter().filter(|spec| matches!(spec, SurfaceSpec::Lock(_))).count();
    if locks > 1 {
        return Err(lua::LoaderError::InvalidTopology(format!(
            "this config declares {locks} `lock` surfaces; § 6.4 gives a `lock` no `monitor` and exactly one surface per output, so a config may \
             declare at most one -- a second would ask the compositor for two lock surfaces on one output, which is `duplicate_output`, which kills \
             the connection with the session still locked"
        )));
    }
    Ok(specs)
}

// ponytail: this top-level evaluation is uncapped, unlike a `computed`/`map` closure's 5ms hook
// (docs/adr/0021, `renderer/src/lua/signal.rs`'s `set_hook`). docs/adr/0039 accepts this: a slow
// evaluation now blocks the Wayland dispatch thread it runs on (via `handle_reevaluate`, called
// from `wayland::run`'s poll loop, and via `run_startup_evaluation` before that loop even starts),
// with no configure handling and no way to set `app.exit` until it returns -- `while true do end`
// in `shell.lua` wedges the whole process. Upgrade path: extend ADR-0021's hook to cover
// `Loader::evaluate_file` itself, not just the closures it registers.
fn evaluate_and_specs(loader: &Loader, shell_lua_path: &Path) -> Result<(lua::LoadOutput, Vec<SurfaceSpec>), lua::LoaderError> {
    let output = loader.evaluate_file(shell_lua_path)?;
    let specs = surface_specs(&output)?;
    Ok((output, specs))
}

/// The veto `Scene::apply` runs on the finished scene while this process holds a session lock: the
/// locked session must still be one the user can authenticate out of.
///
/// **Why this exists at all.** `layout::node::SurfaceFingerprint::Lock` carries only the `id`, so
/// editing what is *inside* a `lock` -- its `child`, and therefore its password field -- diffs as
/// `Unchanged` and takes the in-place reload path, which the generation-swap gate deliberately does
/// not police. Saving a `shell.lua` that deletes the `textfield` while the lock screen is up would
/// therefore apply immediately, and the session becomes unauthenticatable the next time focus is
/// evaluated. The compositor does not unlock when a lock client dies, so the way out is a VT switch.
///
/// **It asks the apply what is on the glass, and holds no list of its own.** The arming side is one
/// `bool` (see [`RendererClient::holds_session_lock`]); the `lock` instances are read out of the
/// instance set this very apply is resolving, which `RendererClient::set_instances` replaces on
/// every monitor hotplug. A remembered list could not survive one: a lid closing onto a dock retires
/// `screen@eDP-1` and creates `screen@DP-1`, and `Scene` keeps the retired instance's tree, so a
/// snapshot taken when the lock was granted went on vouching for a fossil nothing can paint while
/// the live lock screen quietly lost its way out.
///
/// **`any`, not `all`, and it is the same rule the grant used.** `crate::wayland::App`'s
/// `set_session_lock` admits a lock when *any* declared instance is typable, because one lock
/// surface per output is the protocol's requirement and they all resolve from the same declaration.
/// A veto that demanded all of them would refuse every reload for the rest of a lock the guard had
/// already granted, which is the nuisance mirror of the strand above. An empty set fails, which is
/// the case where the reload removed the `lock` declaration outright.
///
/// **Restyling a live lock screen must keep working**, and this is why the veto asks the narrowest
/// possible question rather than freezing the tree. Painting the lock screen out of the config's own
/// Lua is the entire point of docs/adr/0052 decision 2; changing its colours, its clock, its
/// placeholder text or its layout is exactly the edit that should land while it is on the glass.
/// Removing the way out is the one edit that must not.
///
/// The predicate is `crate::wayland`'s `tree_can_authenticate`, not a copy of it, for the reason
/// that function's own doc comment gives: a second opinion about what makes a lock screen usable is
/// how a lock gets granted against a rule the keyboard does not follow.
fn lock_stays_authenticatable(scene: &Scene, instances: &[SurfaceInstance], holds_session_lock: bool) -> Result<(), layout::node::LayoutError> {
    if !holds_session_lock {
        return Ok(());
    }
    let mut locks = Vec::new();
    for instance in instances {
        let Some(tree) = scene.surface(&instance.instance_id) else {
            continue;
        };
        if tree.kind == "lock" {
            if crate::wayland::tree_can_authenticate(&tree) {
                return Ok(());
            }
            locks.push(instance.instance_id.as_str());
        }
    }
    Err(layout::node::invalid(
        "child",
        format!(
            "this evaluation leaves the locked session's `lock` surfaces {locks:?} with no single `textfield` carrying \
             `secure_submit = {{ capability = \"lock\", action = \"authenticate\" }}`, so the locked session would have no way back in \
             but a VT switch; the reload was refused and the lock screen that is on screen still stands (§ 6.4, docs/adr/0052 decision 3)"
        ),
    ))
}

/// Logs each surface *instance*'s resolved geometry after a successful `scene.apply` --
/// diagnostic visibility only, matching Phase 12's original `apply_to_scene` logging.
///
/// Iterates instances rather than declared surfaces since build-steps.md Phase 20 item 2: one
/// declared surface can be several instances, each resolved against a different size, so a
/// per-declaration line would print one of them and hide the rest. The instance id is also
/// exactly the key `Scene::surface` takes, which is what removes the old "has no resolvable `id`"
/// arm -- an instance id is already parsed and already unique by the time it reaches here.
fn log_applied_surfaces(scene: &Scene, instances: &[SurfaceInstance]) {
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
            None => eprintln!("layout resolved but surface {:?} is absent from the applied scene", instance.instance_id),
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

    /// Reads `rescue:get()`'s current `is_rescue`/`error_log` fields back out by evaluating a
    /// tiny probe script -- `LiveSignalHandle` only exposes `set`, so this is the only way to
    /// observe what a prior `set_rescue_state` call actually stored.
    fn rescue_state(loader: &Loader) -> (bool, String) {
        let output = loader
            .evaluate(r#"return panel { id = "_rescue_probe", layer = "Top", is_rescue = oblisk.rescue:get().is_rescue, error_log = oblisk.rescue:get().error_log }"#)
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
        let dirty = DirtyFlag::new();
        let loader = Loader::new(dirty.clone(), shell_lua_path.parent().unwrap()).unwrap();
        let process_registry = ProcessRegistry::new(0, outbound_tx.clone());
        loader.register_process(process_registry.clone()).unwrap();
        let commands = CommandSender::new(0, outbound_tx);
        let client =
            RendererClient::new(loader, shell_lua_path.to_path_buf(), ShapingHandle::spawn(), commands, process_registry, dirty).unwrap();
        (client, outbound_rx)
    }

    /// The one output every fixture below resolves against: `expand_instances` needs a real
    /// output list, and a single 1920x1080 `"TEST"` monitor keeps the instance ids readable
    /// (`"bar@TEST"`) while still exercising the real expansion path.
    fn test_outputs() -> Vec<OutputGeometry> {
        vec![OutputGeometry { name: "TEST".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }]
    }

    /// `crate::wayland::run`'s whole startup sequence in one call (`oblisk-supervisor-services-dbus.md`
    /// § 15.2's Candidate order): evaluate, expand the specs into instances, store them, apply.
    /// Returns whether the apply succeeded, the same thing `apply_instances` reports.
    fn run_startup(client: &mut RendererClient) -> bool {
        let Some(specs) = client.run_startup_evaluation() else {
            return false;
        };
        let instances = expand_instances(&specs, &test_outputs());
        client.set_instances(instances);
        client.apply_instances()
    }

    /// The instance set for a config that declares exactly `ids`, for the tests that seed
    /// `state.pending` by hand instead of going through [`run_startup`].
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

        let output = client.loader.evaluate(r#"return panel { id = "bar", layer = "Top", app_name = oblisk.audio:get().app_name }"#).unwrap();
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

        let output = client.loader.evaluate(r#"return panel { id = "bar", layer = "Top", active = oblisk.workspace:get().active }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("active").unwrap().as_integer(), Some(2));
    }

    #[test]
    fn every_rostered_capability_is_on_the_oblisk_table_and_reads_nil_before_its_first_snapshot() {
        // ADR-0037's uniform contract: a shell.lua reading any rostered capability at boot --
        // before the Supervisor's first push, or forever for a dormant one like sysinfo -- gets
        // a live signal reading nil, never an index-into-nil error into rescue. This is the
        // regression the frozen four-name hand-list allowed six times in a row, now also
        // asserting Phase 25 item 3's name: every one of them is `oblisk.<roster name>`.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::CAPABILITIES {
            let probe = format!(r#"return panel {{ id = "bar", layer = "Top", is_nil = oblisk.{capability}:get() == nil }}"#);
            let output = client.loader.evaluate(&probe).unwrap_or_else(|err| panic!("rostered capability {capability:?} is not on `oblisk`: {err}"));
            assert_eq!(
                output.surfaces[0].properties.get("is_nil").unwrap().as_boolean(),
                Some(true),
                "oblisk.{capability} should read nil before its first snapshot"
            );
        }
    }

    #[test]
    fn no_rostered_capability_is_left_as_a_bare_global() {
        // The other half of Phase 25 item 3, and the half a passing namespace test would not
        // catch: `set_global` never removes anything, so a leftover bare seed would keep working
        // and every config written against it would keep working too, until the day the name
        // collided with a node constructor the way `lock` did (docs/adr/0052 decision 1).
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for capability in shared::CAPABILITIES {
            // `lock` is excluded because § 6.4's node constructor legitimately owns that global,
            // which is the whole reason the namespace exists.
            if *capability == "lock" {
                continue;
            }
            let probe = format!(r#"return panel {{ id = "bar", layer = "Top", is_nil = {capability} == nil }}"#);
            let output = client.loader.evaluate(&probe).unwrap();
            assert_eq!(
                output.surfaces[0].properties.get("is_nil").unwrap().as_boolean(),
                Some(true),
                "{capability} is still a bare global; § 2 names it oblisk.{capability}"
            );
        }
    }

    #[test]
    fn an_unrostered_push_refuses_to_replace_a_name_the_oblisk_table_already_holds() {
        // `rescue` is the signal that reports config failures, so replacing it with an empty
        // capability would make the shell stop reporting its own breakage. `Table::set` over an
        // existing key is silent, which is how ADR-0052 decision 1's `lock` bug happened.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let snapshot = StateSnapshot { capability: "rescue".to_string(), revision: 1, payload: serde_json::json!({}) };
        let err = client.apply_state_snapshot(snapshot).unwrap_err().to_string();
        assert!(err.contains("already something else"), "the refusal must say why: {err}");

        // And the real `rescue` still reads its own table, not an empty capability.
        let output = client
            .loader
            .evaluate(r#"return panel { id = "bar", layer = "Top", intact = oblisk.rescue:get().is_rescue == false }"#)
            .unwrap();
        assert_eq!(output.surfaces[0].properties.get("intact").unwrap().as_boolean(), Some(true));
    }

    #[test]
    fn rescue_and_screens_moved_onto_the_same_table_as_the_roster() {
        // § 2.10 and § 2.15 name both of these `oblisk.*` like every capability, and ADR-0041
        // decision 2 said `screens` would move "along with all of them" once a table existed.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let probe = r#"return panel { id = "bar", layer = "Top",
            rescued = oblisk.rescue:get().is_rescue,
            screen_count = #oblisk.screens:get(),
            bare_rescue_gone = rescue == nil,
            bare_screens_gone = screens == nil }"#;
        let output = client.loader.evaluate(probe).unwrap();
        let props = &output.surfaces[0].properties;
        assert_eq!(props.get("rescued").unwrap().as_boolean(), Some(false));
        assert_eq!(props.get("screen_count").unwrap().as_integer(), Some(0));
        assert_eq!(props.get("bare_rescue_gone").unwrap().as_boolean(), Some(true));
        assert_eq!(props.get("bare_screens_gone").unwrap().as_boolean(), Some(true));
    }

    #[test]
    fn oblisk_version_is_three_integers_a_config_can_compare() {
        // Phase 25 item 4. The value matters less than the shape: a config guards with
        // `if oblisk.version.major > 0 or oblisk.version.minor >= 2 then`, so all three fields
        // have to be present and numeric on the day the first config is written.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        let probe = r#"return panel { id = "bar", layer = "Top",
            major = oblisk.version.major, minor = oblisk.version.minor, patch = oblisk.version.patch }"#;
        let output = client.loader.evaluate(probe).unwrap();
        let props = &output.surfaces[0].properties;
        let [major, minor, patch] = version_parts();
        assert_eq!(props.get("major").unwrap().as_integer(), Some(i64::from(major)));
        assert_eq!(props.get("minor").unwrap().as_integer(), Some(i64::from(minor)));
        assert_eq!(props.get("patch").unwrap().as_integer(), Some(i64::from(patch)));
    }

    #[test]
    fn oblisk_config_dir_is_the_directory_shell_lua_was_loaded_from() {
        // build-steps.md Phase 29 item 4: this is how a config names a wallpaper it ships beside
        // itself. Derived from the loaded path rather than re-resolved, so a Renderer started with
        // an explicit `shell.lua` cannot report a directory it is not reading from.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/opt/oblisk-config/shell.lua"));
        let probe = r#"return panel { id = "bar", layer = "Top", dir = oblisk.config_dir }"#;
        let output = client.loader.evaluate(probe).unwrap();
        let dir = output.surfaces[0].properties.get("dir").unwrap().as_string().unwrap();
        assert_eq!(dir.to_string_lossy(), "/opt/oblisk-config");
    }

    #[test]
    fn version_parts_are_the_crates_own_version() {
        // Guards the `expect` in `version_parts`: a `version` Cargo could not split into three
        // numbers would panic every Renderer at startup, and this fails the build instead.
        let [major, minor, patch] = version_parts();
        assert_eq!(format!("{major}.{minor}.{patch}"), env!("CARGO_PKG_VERSION"));
    }

    /// The regression build-steps.md Phase 25 item 3 could silently reintroduce, and the reason
    /// docs/adr/0052 decision 1 brought exactly one name of that item forward. `RendererClient::new`
    /// seeds the roster *after* `Loader::new` registered § 6.4's node constructors, so seeding
    /// `lock` as a bare global overwrites the constructor -- and a `set` over an existing global
    /// says nothing, so the failure surfaces as "attempt to call a userdata value" from the
    /// config's own `lock { ... }` line, pointing at the config rather than at the seed.
    ///
    /// Asserted after a whole generation is built, not after the seed alone, because the ordering
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
        let probe = client.loader.evaluate(
            r##"return panel {
                id = "_probe", layer = "Top",
                lock_kind = lock { id = "screen" }.kind,
                capability_type = type(oblisk.lock),
                attempts = oblisk.lock:get().attempts,
            }"##,
        ).unwrap();
        let props = &probe.surfaces[0].properties;
        assert_eq!(props.get("lock_kind").unwrap().as_string().unwrap().to_string_lossy(), "lock", "the global `lock` must still be § 6.4's node constructor");
        // And the capability is reachable, hydrated, under the name § 2 gives it.
        assert_eq!(props.get("capability_type").unwrap().as_string().unwrap().to_string_lossy(), "userdata");
        assert_eq!(props.get("attempts").unwrap().as_integer(), Some(2), "the `lock` StateSnapshot must reach `oblisk.lock`, not a bare global nothing registered");
    }

    /// The write half of the same object (build-steps.md Phase 25 item 1): a config's own
    /// `on_click` calling the lock action puts a real § 7.2 envelope on the outbound channel.
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
            .apply_state_snapshot(StateSnapshot { capability: "network".to_string(), revision: 1, payload: serde_json::json!({ "scanning": true }) })
            .unwrap();
        client
            .apply_state_snapshot(StateSnapshot { capability: "network".to_string(), revision: 2, payload: serde_json::json!({ "scanning": false }) })
            .unwrap();

        let output = client.loader.evaluate(r#"return panel { id = "bar", layer = "Top", scanning = oblisk.network:get().scanning }"#).unwrap();
        assert_eq!(
            output.surfaces[0].properties.get("scanning").unwrap().as_boolean(),
            Some(false),
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

    /// A `window` and a `popup` both declared alongside the panels, spelled the way
    /// `dev-config/oblisk/shell.lua` spells them.
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
        // The whole roster, not just the panels (build-steps.md Phase 22). `expand_instances` and
        // `create_surfaces` both branch on the variant, so a role that arrived untagged -- or was
        // dropped, as it was before this commit -- could only ever be bound as a layer surface.
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
        // docs/adr/0049 decision 3 and ADR-0001: a `window`'s Wayland object comes and goes inside
        // one generation, but its *declaration* is fixed for that generation's life. The panel-only
        // fingerprint this replaced could not see one appear, so adding a `window` reported
        // `Unchanged` and reloaded in place into a generation that had built no surface for it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology =
            Some(surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap().iter().map(SurfaceSpec::fingerprint).collect());

        write_shell_lua(dir.path(), three_roles_config());
        client.handle_reevaluate(ReevaluateRequest { sequence: 9 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 9 }));
        assert!(client.state.pending.is_none());
    }

    /// A lock screen whose `child` holds the one `secure_submit` field § 6.4 needs, wrapped in a
    /// `column` whose colour the second half of the test below edits.
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
        // Defect E. `SurfaceFingerprint::Lock` carries only the `id`, so an edit *inside* the lock
        // diffs as `Unchanged` and takes the in-place path, which the generation-swap gate
        // deliberately does not police. Deleting the password field while the lock is up therefore
        // applied straight into the tree on the glass, and the session had no way back in but a VT
        // switch -- the compositor does not unlock when a lock client dies.
        //
        // Both halves are the point. The restyle has to keep landing, because painting the lock
        // screen out of the config's own Lua is the whole of docs/adr/0052 decision 2; only the edit
        // that removes the way out is refused, and `Scene::apply`'s existing rollback is what makes
        // the refusal leave the live tree exactly as it was.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        // A restyle: same surfaces, same field, different colour.
        write_shell_lua(dir.path(), &lock_config("#204080FF"));
        client.handle_reevaluate(ReevaluateRequest { sequence: 20 });
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 20 }));
        client.handle_apply_pending(ApplyPendingReload { sequence: 20 });
        let restyled = client.scene.surface("screen@TEST").expect("the lock instance is still resolved");
        assert_eq!(
            restyled.children[0].properties.get("background").unwrap().as_string().unwrap().to_string_lossy(),
            "#204080FF",
            "restyling a live lock screen is the reason it is painted from Lua at all"
        );

        // The edit that must not land: same lock, no `textfield` under it.
        write_shell_lua(dir.path(), r##"return {
            panel { id = "bar", layer = "Top" },
            lock { id = "screen", child = column { background = "#204080FF", children = {} } },
        }"##);
        client.handle_reevaluate(ReevaluateRequest { sequence: 21 });
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 21 }));
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
        // The other side of the veto's arming, and the reason it is a stored instance set rather
        // than a standing rule: with no lock held there is nothing to be locked out of, so a config
        // may edit its lock screen down to nothing like any other surface. Freezing the tree
        // whenever a `lock` is merely *declared* would make the ordinary editing loop for a lock
        // screen impossible.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        write_shell_lua(dir.path(), r##"return {
            panel { id = "bar", layer = "Top" },
            lock { id = "screen", child = column { background = "#101010FF", children = {} } },
        }"##);
        client.handle_reevaluate(ReevaluateRequest { sequence: 22 });
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 22 }));
        client.handle_apply_pending(ApplyPendingReload { sequence: 22 });

        assert!(client.scene.surface("screen@TEST").unwrap().children[0].children.is_empty(), "an unlocked session's lock screen is ordinary");
    }

    #[test]
    fn the_lock_veto_follows_the_outputs_rather_than_the_instances_it_was_armed_with() {
        // CONFIRMED, and it strands the session. The veto used to be armed with the `lock` instance
        // ids that existed at `LockCommand::Acquire`. `crate::wayland::App::handle_output_change`
        // retires instances and creates new ones on every hotplug and never revisited that list, and
        // `Scene` leaves a retired instance's tree standing, so a lid closing onto a dock left the
        // veto validating `screen@TEST` -- a fossil nothing can paint -- while the live lock screen
        // was `screen@DP-1`. Deleting the password field then passed the veto, applied in place, and
        // the way out of the locked session was a VT switch.
        //
        // The apply below is the same sequence `handle_output_change` performs: re-expand the
        // applied specs against the outputs that exist now, store them, resolve.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), &lock_config("#101010FF"));
        let (mut client, mut outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        let specs = client.applied_surface_specs();
        let docked = vec![OutputGeometry { name: "DP-1".to_string(), size: layout::LogicalSize { width: 2560.0, height: 1440.0 } }];
        client.set_instances(expand_instances(&specs, &docked));
        assert!(client.apply_instances(), "the freshly plugged output resolves its own lock surface");

        write_shell_lua(dir.path(), r##"return {
            panel { id = "bar", layer = "Top" },
            lock { id = "screen", child = column { background = "#101010FF", children = {} } },
        }"##);
        client.handle_reevaluate(ReevaluateRequest { sequence: 30 });
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 30 }));
        client.handle_apply_pending(ApplyPendingReload { sequence: 30 });

        let live = client.scene.surface("screen@DP-1").expect("the lock instance on the output that is actually plugged in");
        assert_eq!(live.children[0].children.len(), 1, "the veto must ask what is on the glass now, not what was there when the lock was taken");
    }

    #[test]
    fn a_second_lock_declaration_is_refused_at_evaluation_naming_6_4() {
        // Defect C, and it strands the machine rather than merely misrendering.
        // `expand_instances` emits one instance per lock spec per output, so two `lock`
        // declarations make `ensure_lock_surfaces` send two `get_lock_surface` for the same
        // `wl_output` -- which `ext-session-lock-v1` calls out by name: "Attempting to create more
        // than one lock surface for a given output is a duplicate_output protocol error". The
        // compositor kills the connection *after* the lock is taken and does not unlock on client
        // death, so the only way back in is a VT switch. § 6.4 gives a lock no `monitor` and one
        // surface per output, so a second declaration has no coherent meaning to admit.
        //
        // Both entry points, because both reach a generation: the startup evaluation refuses to
        // hand any specs back, and a `Reevaluate` reports `Failed` rather than staging it.
        let dir = tempfile::tempdir().unwrap();
        let two_locks = r#"return { lock { id = "first" }, lock { id = "second" } }"#;
        let path = write_shell_lua(dir.path(), two_locks);
        let (mut client, mut outbound_rx) = test_client(&path);

        assert!(!run_startup(&mut client), "a config with two `lock` surfaces must not produce a generation");
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue, "the refusal has to be visible somewhere, and rescue is where an evaluation failure goes");
        assert!(error_log.contains("§ 6.4"), "the message must name the section that says one lock surface per output: {error_log}");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        client.state.applied_topology =
            Some(surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap().iter().map(SurfaceSpec::fingerprint).collect());
        write_shell_lua(dir.path(), two_locks);
        client.handle_reevaluate(ReevaluateRequest { sequence: 11 });

        assert!(
            matches!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence: 11, .. })),
            "an edit that adds a second lock must fail the reevaluation rather than be staged"
        );
        assert!(client.state.pending.is_none());
    }

    #[test]
    fn a_windows_title_is_an_in_place_field_rather_than_a_topology_one() {
        // The other side of that line, and the protocol is what decides it: `xdg-shell.xml` says a
        // `set_title`/`set_app_id` request may be sent after the toplevel is mapped, so a changed
        // title must not respawn the process to deliver it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return window { id = "settings", title = "Settings" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology =
            Some(surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap().iter().map(SurfaceSpec::fingerprint).collect());

        write_shell_lua(dir.path(), r#"return window { id = "settings", title = "Oblisk settings", app_id = "oblisk.settings" }"#);
        client.handle_reevaluate(ReevaluateRequest { sequence: 10 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 10 }));
        assert!(client.state.pending.is_some());
    }

    #[test]
    fn a_popup_with_a_zero_anchor_rect_fails_the_evaluation_and_names_the_property_in_rescue() {
        // The reason non-panel roles are parsed at evaluation at all (build-steps.md Phase 22).
        // § 6.3's `anchor_rect` feeds `xdg_positioner::set_anchor_rect`, and a zero size leaves the
        // positioner incomplete, which raises `invalid_positioner` at `get_popup` and takes the
        // whole Wayland connection with it. Phase 20 settled that a config typo is a `LayoutError`
        // at evaluation, never a protocol error at runtime, so this must land in `rescue`'s
        // `error_log` (§ 2.10, docs/adr/0046) with the property named.
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
        assert!(error_log.contains("anchor_rect"), "the human reading rescue's error_log needs the property named: {error_log}");
    }

    #[test]
    fn a_window_whose_max_size_is_below_its_min_size_fails_the_evaluation() {
        // The `window` half of the same rule: `set_max_size` raises `invalid_size` on a maximum
        // under the minimum, so `check_max_size_above_min` has to run somewhere a config author
        // can see it, and evaluation is that place.
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
        // Regression test for a CONFIRMED correctness finding: treating "nothing applied yet" as
        // an empty topology (rather than "no prior state to protect") made every subsequent
        // evaluation -- even a fix to a syntactically valid file -- permanently misclassify as
        // `TopologyChanged`, which nothing here ever applies, leaving the shell blank forever.
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
        client.state.applied_topology =
            Some(surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap().iter().map(SurfaceSpec::fingerprint).collect());

        client.handle_reevaluate(ReevaluateRequest { sequence: 5 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 5 }));
        assert!(matches!(&client.state.pending, Some((sequence, _, _)) if *sequence == 5));
    }

    #[test]
    fn handle_reevaluate_reports_topology_changed_and_does_not_store_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        // Seed a *different* applied topology (a different id) so the fresh evaluation reads as changed.
        client.state.applied_topology =
            Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology { id: "other".to_string(), layer: LayerKind::Top, anchor: Default::default(), monitor: "All".to_string(), namespace: "oblisk-other".to_string() })]);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
        assert!(client.state.pending.is_none(), "a topology-changed generation must not stage a pending apply");
    }

    #[test]
    fn editing_only_the_in_place_panel_fields_reports_unchanged_and_reloads_in_place() {
        // docs/adr/0038 decision 2 and `CONTEXT.md`'s Value change entry: `margin`, exclusive
        // zone, `keyboard_interactivity` and size are all requests layer-shell accepts on a live
        // surface, so editing one must reload in place. The diff therefore reads
        // `PanelSpec::topology` alone -- comparing whole specs would turn every one of these into
        // a generation swap, respawning the process to nudge a bar 4px sideways.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", margin = { left = 4 }, keyboard_interactivity = "None", exclusive = false, height = 32 }"#,
        );
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology =
            Some(surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap().iter().map(SurfaceSpec::fingerprint).collect());

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
        assert!(client.state.pending.is_some(), "an Unchanged verdict must stage the fresh evaluation for ApplyPendingReload");
    }

    #[test]
    fn editing_only_the_namespace_reports_topology_changed() {
        // The other side of the same line. `get_layer_surface` fixes the namespace at creation and
        // no request changes it on a live surface, so a namespace edit needs a new surface, which
        // means a new generation (`CONTEXT.md`, Topology change).
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology =
            Some(surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap().iter().map(SurfaceSpec::fingerprint).collect());

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", namespace = "my-bar" }"#);
        client.handle_reevaluate(ReevaluateRequest { sequence: 8 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 8 }));
        assert!(client.state.pending.is_none());
    }

    #[test]
    fn set_instance_size_replaces_one_instances_available_size_and_marks_the_scene_dirty() {
        // build-steps.md Phase 20 item 4: a `configure` is what finally makes the configured size
        // reachable, and it reuses ADR-0044 decision 2's one dirty flag rather than adding a
        // second "something changed" mechanism next to it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client), "startup must have applied");
        assert!(!client.dirty.take(), "a clean startup leaves the flag clear");
        assert_eq!(client.scene.surface("bar@TEST").unwrap().rect.height, 1080.0, "the first resolve uses the output's own size");

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
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 1920.0, height: 1080.0 });
        assert!(!client.dirty.take(), "a duplicate configure carrying the size already resolved against changes nothing");

        client.set_instance_size("no-such-surface@TEST", layout::LogicalSize { width: 10.0, height: 10.0 });
        assert!(!client.dirty.take(), "a configure for a surface no instance names must not dirty the whole scene");
    }

    #[test]
    fn one_surface_on_two_outputs_resolves_two_trees_against_two_different_sizes() {
        // The reason the retained scene keys by instance (`layout::scene::Scene`'s doc comment):
        // `monitor = "All"` across a laptop panel and a 4K external is two configured sizes, and
        // one tree per declared surface could only ever serve one of them.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", width = "Fill", height = "Fill" }"#);
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

        assert!(run_startup(&mut client), "an unmatched monitor is not an apply failure -- there is simply nothing to resolve");
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
        // Regression test for a minor correctness finding: a topology-field type error (e.g.
        // `anchor.top` not a boolean) used to be folded into `InvalidTopLevelReturn`'s fixed
        // "must be a `panel` node or an array of them" message, which is wrong for this case.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", anchor = { top = "yes" } }"#);
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
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let (output, specs) = evaluate_and_specs(&client.loader, &path).unwrap();
        client.set_instances(instances_for(&["bar"]));
        client.state.pending = Some((3, output, specs.iter().map(SurfaceSpec::fingerprint).collect()));

        client.handle_apply_pending(ApplyPendingReload { sequence: 3 });

        assert!(client.scene.surface("bar@TEST").is_some());
        assert!(client.state.pending.is_none());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
        // The half that reaches the screen: `crate::wayland::run`'s poll loop repaints and pushes
        // each surface's `visible`/in-place fields/input region only when `re_resolve_if_dirty`
        // reports a change, so an in-place reload that applied but left the flag clear would never
        // show up on a static config.
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

    // ADR-0044 decision 2, build-steps.md Phase 19 item 2: a `StateSnapshot` push marks the
    // scene dirty, and a dirty scene re-resolves against the last applied evaluation without
    // running shell.lua again. `workspace` is used as the pushed capability throughout (matching
    // `apply_state_snapshot_lazily_registers_an_unrostered_capabilitys_live_signal` above) because
    // it isn't in `shared::CAPABILITIES`, so pushing it before `run_startup_evaluation` is what
    // makes a `shell.lua` that references it bare (not `:get()`) evaluate at all.

    #[test]
    fn apply_state_snapshot_marks_the_scene_dirty() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        assert!(!client.dirty.take(), "a fresh client must not start dirty");

        let snapshot = StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({ "app_name": "Zen" }) };
        client.apply_state_snapshot(snapshot).unwrap();

        assert!(client.dirty.take(), "LiveSignalHandle::set must mark the shared scene-dirty flag");
    }

    #[test]
    fn re_resolve_if_dirty_applies_a_pushed_value_without_reading_shell_lua_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = oblisk.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot { capability: "workspace".to_string(), revision: 1, payload: serde_json::json!(true) })
            .unwrap();
        run_startup(&mut client);
        assert!(client.scene.surface("bar@TEST").unwrap().visible, "startup must have applied the pushed initial value");

        // Break the file so a real re-evaluation would fail -- proves the second half: the
        // re-resolve below reads the pushed value straight off the retained tree's live signal,
        // never touching this file again.
        std::fs::write(&path, "this is not lua").unwrap();

        client
            .apply_state_snapshot(StateSnapshot { capability: "workspace".to_string(), revision: 2, payload: serde_json::json!(false) })
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
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = oblisk.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot { capability: "workspace".to_string(), revision: 1, payload: serde_json::json!(true) })
            .unwrap();
        run_startup(&mut client);
        client
            .apply_state_snapshot(StateSnapshot { capability: "workspace".to_string(), revision: 2, payload: serde_json::json!(false) })
            .unwrap();

        client.re_resolve_if_dirty();
        assert!(!client.scene.surface("bar@TEST").unwrap().visible, "the first re-resolve must apply the pushed value");
        assert!(!client.dirty.take(), "re_resolve_if_dirty must clear the flag it consumed");

        // Replace `applied_output` directly (bypassing the push path, which would re-mark dirty)
        // with an evaluation that resolves `visible` to `true`. If a second `re_resolve_if_dirty`
        // call did any work at all, this would be visible; a true no-op leaves the scene exactly
        // as the first resolve left it.
        let poisoned_path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = true }"#);
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
        // ADR-0044 decision 2's "drain first, then re-resolve once": `wayland::run`'s poll loop
        // only calls `re_resolve_if_dirty` after draining every pending inbound frame, so several
        // pushes landing in one drain must coalesce into a single dirty read, not one per push.
        // `DirtyFlag` is a bool, not a counter, so this is provable directly: however many pushes
        // land before the flag is read, reading it reports "dirty" exactly once.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);

        for revision in 1..=5 {
            client
                .apply_state_snapshot(StateSnapshot { capability: "audio".to_string(), revision, payload: serde_json::json!({ "n": revision }) })
                .unwrap();
        }

        assert!(client.dirty.take(), "a burst of five pushes must have marked the flag");
        assert!(!client.dirty.take(), "the flag records only whether a push happened since the last check, not how many, so the burst coalesces into one turn's work");
    }

    #[test]
    fn a_push_that_makes_a_property_invalid_keeps_the_prior_scene_and_does_not_enter_rescue() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = oblisk.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot { capability: "workspace".to_string(), revision: 1, payload: serde_json::json!(true) })
            .unwrap();
        run_startup(&mut client);
        assert!(client.scene.surface("bar@TEST").unwrap().visible);
        assert_eq!(rescue_state(&client.loader), (false, String::new()));

        // `visible` requires a boolean (`parse_visible`); a table makes the re-resolve fail.
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
        // ADR-0044 decision 1's nil rule, from the Renderer's end: `run_startup_evaluation` runs
        // before `wayland::run`'s poll loop has drained one inbound frame, so every rostered
        // capability's signal still reads `nil` here. A config that binds one bare -- the exact
        // shape decision 1 exists to enable -- must still apply, taking each parser's absent
        // property default, rather than failing layout and leaving the shell blank.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", visible = oblisk.audio, child = rect { width = oblisk.network, height = 10, children = oblisk.tray } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        let bar = client.scene.surface("bar@TEST").expect("a bare rostered signal must not stop the config applying");
        assert!(bar.visible, "`visible = audio` with audio still nil must take parse_visible's default");
        assert!(bar.children[0].children.is_empty(), "`children = tray` with tray still nil must take parse_children's default");
        assert_eq!(rescue_state(&client.loader), (false, String::new()), "a startup that applies must not be in rescue");
    }

    #[test]
    fn a_text_node_bound_to_a_bare_rostered_signal_applies_at_startup_with_no_push_at_all() {
        // ADR-0044's headline example, and the reason build-steps.md Phase 19 item 6 gives
        // `content` a default: `text { content = oblisk.mpris.title }` must apply at boot even
        // though `title` still reads `nil` here, same as the sibling test above for `visible` and
        // `children`. Before item 6, `content` had no default and this rejected the whole tree.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", child = text { content = oblisk.audio } }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        let bar = client.scene.surface("bar@TEST").expect("a bare rostered signal on `content` must not stop the config applying");
        assert_eq!(
            layout::node::parse_content(&bar.children[0].properties).unwrap(),
            "",
            "`content = audio` with audio still nil must take parse_content's default"
        );
        assert_eq!(rescue_state(&client.loader), (false, String::new()), "a startup that applies must not be in rescue");
    }

    #[test]
    fn a_push_arriving_before_the_first_successful_apply_is_not_consumed_and_lost() {
        // `applied_output` is checked *before* the flag is taken: with nothing to re-resolve
        // against there is nothing this call can do with the flag, so consuming it would silently
        // discard the push. Taking it first meant a startup that failed to apply swallowed every
        // later push, and the shell stayed blank until an inotify edit forced a re-evaluation.
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (mut client, _outbound_rx) = test_client(&missing);
        run_startup(&mut client);
        assert!(client.state.applied_output.is_none(), "startup must have failed (no file)");

        client
            .apply_state_snapshot(StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({ "app_name": "Zen" }) })
            .unwrap();
        client.re_resolve_if_dirty();

        assert!(client.dirty.take(), "the push must still be pending for whatever applies next, not consumed by the early return");
    }

    #[test]
    fn repeated_re_resolves_that_retire_nodes_do_not_grow_the_lease_bag() {
        // `Scene::apply` now runs up to once per poll turn rather than once per config edit, and
        // `retire_child_first` pushes every removed subtree onto `Scene::retiring`, which nothing
        // in production drains (`Scene::release` has no caller, docs/adr/0023 item 7). A children
        // signal that alternates its length therefore leaked a `RetainedNode` -- and the
        // `mlua::Value` properties it holds -- at push cadence, in a process meant to run for a
        // whole session.
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
                .apply_state_snapshot(StateSnapshot { capability: "audio".to_string(), revision, payload: serde_json::json!(count) })
                .unwrap();
            client.re_resolve_if_dirty();
        }

        assert_eq!(client.scene.surface("bar@TEST").unwrap().children[0].children.len(), 1, "the last push shrank the row back to one child");
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
        // clear. The first poll turn then redid a whole `Scene::apply` -- retained-tree clone,
        // full walk, a blocking shaping round trip per text node -- for nothing.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", child = text { content = "hi" } }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        run_startup(&mut client);

        assert!(client.scene.surface("bar@TEST").is_some(), "startup must have applied");
        assert!(!client.dirty.take(), "an apply that succeeded resolved every signal at its current value, so nothing is stale");
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
        // docs/adr/0041 decision 1: no `variants` primitive, because Lua already has `for`. This
        // is the whole feature -- if the seed did not land before the evaluation, the loop would
        // run zero times and the config would declare nothing.
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
        // The seeded value `RendererClient::new` puts in the signal. A `nil` here would make
        // `ipairs` error and drop a config that never did anything wrong straight into rescue.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "Top", child = text { content = "screens: " .. #oblisk.screens:get() } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(run_startup(&mut client), "an unseeded `screens` must not fail the evaluation");
        let tree = client.scene.surface("bar@TEST").unwrap();
        assert_eq!(tree.children[0].properties.get("content").unwrap().as_string().unwrap().to_string_lossy(), "screens: 0");
    }

    #[test]
    fn set_screens_repeating_the_same_list_reports_no_change_and_leaves_the_scene_clean() {
        // `update_output` fires for changes `screens` does not carry, and the caller gates both
        // the surface reconcile and `request_reload` on this return value -- an unchanged re-push
        // must not buy a `Scene::apply` or a Supervisor round trip.
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
            client.scene.surface("bar@TEST").unwrap().children[0].properties.get("content").unwrap().as_string().unwrap().to_string_lossy()
        };
        assert_eq!(content(&client), "n=1");

        assert!(client.set_screens(screens_json(&["eDP-1", "DP-1"])), "a monitor appearing is an output change");
        assert!(client.re_resolve_if_dirty(), "and the scene must re-resolve against it without re-reading shell.lua");
        assert_eq!(content(&client), "n=2");
    }

    #[test]
    fn seeding_screens_before_the_startup_apply_still_leaves_the_scene_flag_clear() {
        // The seed is an ordinary live-signal write, so it marks the flag like any other; the
        // apply that immediately follows it resolved against that very value, so entering the
        // poll loop dirty would buy one whole redundant `Scene::apply` before anything is drawn.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", child = text { content = "hi" } }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(client.set_screens(screens_json(&["eDP-1"])));
        assert!(run_startup(&mut client));

        assert!(!client.dirty.take(), "the startup apply already resolved against the seeded screen list");
    }

    #[test]
    fn applied_surface_specs_returns_the_applied_declarations_without_reading_shell_lua_again() {
        // What a monitor hotplug re-expands against (docs/adr/0038 decision 3). Re-reading the
        // file here would cost an evaluation and race the `Reevaluate` the Supervisor is about to
        // send anyway, so the file is deleted mid-test to prove it is never touched.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", monitor = "All" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        run_startup(&mut client);
        std::fs::remove_file(&path).unwrap();

        let specs = client.applied_surface_specs();
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], SurfaceSpec::Panel(panel) if panel.topology.id == "bar" && panel.topology.monitor == "All"));
    }

    #[test]
    fn applied_surface_specs_is_empty_when_no_evaluation_has_ever_applied() {
        // A startup evaluation that failed declares nothing, so a hotplug adds no instance --
        // the honest answer rather than a panic on an output event.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/no/such/shell.lua"));
        assert!(client.applied_surface_specs().is_empty());
    }

    #[test]
    fn request_reload_queues_the_frame_the_supervisor_starts_a_cycle_from() {
        // docs/adr/0041 decision 4: the Renderer asks for a cycle rather than fabricating a
        // sequence, because `supervisor/src/main.rs`'s `is_current_reload` would drop the report
        // of any sequence it did not itself send.
        let (client, mut outbound_rx) = test_client(std::path::Path::new("/no/such/shell.lua"));
        client.request_reload();
        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::RequestReload);
    }

    #[test]
    fn a_topology_changed_reevaluate_leaves_the_scene_flag_clear() {
        // Worse than the wasted work above: a `TopologyChanged` verdict must leave this
        // generation's scene alone entirely (that case is a generation swap, Phase 14), but the
        // no-op `set_rescue_state(false, "")` on the success path left the flag set, so the next
        // poll turn re-applied `applied_output` to a scene the verdict says must not be mutated.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        client.state.applied_topology =
            Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology { id: "other".to_string(), layer: LayerKind::Top, anchor: Default::default(), monitor: "All".to_string(), namespace: "oblisk-other".to_string() })]);

        client.handle_reevaluate(ReevaluateRequest { sequence: 1 });

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
        assert!(!client.dirty.take(), "a topology-changed generation must not have its scene marked dirty by the verdict itself");
    }

    #[test]
    fn handle_frame_answers_a_reevaluate_frame_with_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);
        // A different applied topology so the fresh evaluation reads as changed -- proves the
        // dispatch/queue path, not `handle_reevaluate`'s own classification logic (already
        // covered by the tests above).
        client.state.applied_topology =
            Some(vec![SurfaceFingerprint::Panel(layout::node::SurfaceTopology { id: "other".to_string(), layer: LayerKind::Top, anchor: Default::default(), monitor: "All".to_string(), namespace: "oblisk-other".to_string() })]);

        assert_eq!(client.handle_frame(SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 })), FrameOutcome::Handled);

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
    }

    #[test]
    fn handle_frame_hands_an_activate_draw_nonce_back_to_the_wayland_loop() {
        // One of the two frames `handle_frame` can't service itself: drawing needs `wayland::App`'s
        // EGL and surface state, so the nonce goes back to the caller for `App::activate_draw`.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(client.handle_frame(SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 })), FrameOutcome::ActivateDraw(42));
    }

    #[test]
    fn handle_frame_hands_a_set_session_lock_back_to_the_wayland_loop_in_both_directions() {
        // The other one, and both directions matter: `locked = true` has to reach the Wayland
        // thread to be refused there when no `lock` surface is declared (docs/adr/0052 decision 3),
        // and `locked = false` is the only path in this process permitted to unlock at all
        // (docs/adr/0042). Swallowing either here would be silent.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(client.handle_frame(SupervisorFrame::SetSessionLock(SetSessionLock { locked: true })), FrameOutcome::SetSessionLock(true));
        assert_eq!(client.handle_frame(SupervisorFrame::SetSessionLock(SetSessionLock { locked: false })), FrameOutcome::SetSessionLock(false));
    }

    #[test]
    fn handle_frame_logs_and_continues_on_deselect_input_and_promote_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, mut outbound_rx) = test_client(&path);

        assert_eq!(client.handle_frame(SupervisorFrame::DeselectInput(DeselectInput { surface_id: "main_bar".to_string() })), FrameOutcome::Handled);
        assert_eq!(client.handle_frame(SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id: "main_bar".to_string() })), FrameOutcome::Handled);
        // A third, recognized frame to prove dispatch kept working (not stuck/panicked) after the
        // two inert ones above. No prior `applied_topology` is seeded, so this fresh evaluation
        // reports `Unchanged` (see the module doc comment point 3) -- the report's exact verdict
        // isn't this test's point, only that a real response arrives at all after the two inert
        // frames.
        assert_eq!(client.handle_frame(SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 9 })), FrameOutcome::Handled);

        assert_eq!(queued_frame(&mut outbound_rx), RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 9 }));
    }

    #[test]
    fn handle_frame_routes_process_output_and_exit_frames_to_the_registered_lua_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        // Register real out_cb/exit_cb through the real process.run global, exactly as
        // `renderer/src/lua/process.rs`'s own tests do -- the id (0, the first call on a fresh
        // registry) is what the inbound frames below address.
        client
            .loader
            .evaluate(
                r#"
                process.run("cmd", {}, function(line, stream) probe_line = line; probe_stream = stream end, function(code) probe_code = code end)
                return panel { id = "bar", layer = "Top" }
                "#,
            )
            .unwrap();

        let output_frame = SupervisorFrame::ProcessOutput(ProcessOutputLine { id: 0, stream: shared::ProcessStream::Stdout, line: "hello".to_string() });
        assert_eq!(client.handle_frame(output_frame), FrameOutcome::Handled);
        assert_eq!(client.handle_frame(SupervisorFrame::ProcessExited(ProcessExited { id: 0, code: Some(3) })), FrameOutcome::Handled);

        let output = client
            .loader
            .evaluate(r#"return panel { id = "bar", layer = "Top", line = probe_line, stream = probe_stream, code = probe_code }"#)
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
