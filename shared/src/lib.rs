use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

pub mod framing;
mod paths;
mod secure_buffer;
pub use paths::{
    CHECK_ENV, CONFIG_ARG_ENV, CONFIG_DIR_ENV, EXIT_COMPOSITOR_GONE, GENERATION_ID_ENV, PROFILE_ENV, config_dir,
    control_socket_path, log_path, profile_interval, session_locked_flag_path, shell_lua_path,
};
pub use secure_buffer::SecureBuffer;
pub use zeroize::{Zeroize, Zeroizing};

/// The snapshot-hydrated capability roster (ADR-0037; CONTEXT.md). Each [`Capability::as_str`]
/// name is both the Lua `obelisk.<name>` member and command `capability` field, so
/// one spelling reaches one capability. Reading a name starts its Supervisor controller
/// (ADR-0070); it remains `nil` until the first `StateSnapshot`, so an unread name costs nothing.
/// `idle` is event-shaped, not snapshot state (ADR-0032), so the Supervisor's `Startable` covers
/// it. `polkit` is included (ADR-0114), and a naming `secure_submit` starts it too.
///
/// This enum replaces `&[&str]` (ADR-0076). Exhaustive matches cover the two decisions that
/// strings once let drift, by silently accepting an unimplemented name and leaving its Lua member
/// `nil` forever: starting a controller and dispatching its commands. The `roster!` list
/// generates [`Capability::ALL`] and [`Capability::as_str`], so a new variant missing from the Lua
/// namespace, stubs or schema check fails to compile instead of staying silently `nil`.
macro_rules! roster {
    ($($variant:ident => $name:literal, $blurb:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Capability {
            $($variant),+
        }

        impl Capability {
            /// Every variant, in the order the roster has always listed them.
            pub const ALL: &'static [Capability] = &[$(Capability::$variant),+];

            /// The shared wire/Lua spelling. It matches serde's `snake_case` rename.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Capability::$variant => $name),+
                }
            }

            /// The `obelisk.<name>` line for generated stubs (`supervisor/src/stubs.rs`). Kept here,
            /// not in the Renderer, so a new variant must provide one.
            pub const fn blurb(self) -> &'static str {
                match self {
                    $(Capability::$variant => $blurb),+
                }
            }
        }
    };
}

roster! {
    Audio => "audio", "PipeWire: master volume and mute, the output and input device lists, and one entry per application currently playing.",
    Network => "network", "NetworkManager: scan state and the access points the last scan found. Connecting is a command, not a field.",
    Bluetooth => "bluetooth", "BlueZ: adapter power, discovery state, and the connected and discovered device lists.",
    Tray => "tray", "StatusNotifierItem: every registered tray icon with its artwork, status and DBusMenu tree.",
    Notifications => "notifications", "The freedesktop notification server: the newest 20 live notifications and the do-not-disturb toggle.",
    Mpris => "mpris", "MPRIS: every media player on the bus, with track metadata, playback state and a position to extrapolate from.",
    Sysinfo => "sysinfo", "CPU, memory and swap load, plus hwmon temperatures. Sampled on a timer this capability owns.",
    Keyboard => "keyboard", "Lock-key state, the active layout, and the keyboard backlight where the machine has one.",
    Privacy => "privacy", "Who is using the camera, the microphone, or the screen right now. Empty means nobody is.",
    Updates => "updates", "Pending pacman upgrades, the progress of an install in flight, and whether the kernel changed under you.",
    Lock => "lock", "The session lock: whether it is held, whether a password is with PAM, and why the last attempt failed.",
    Polkit => "polkit", "The authentication request polkitd is waiting on: what for, whether a password is with PAM, and why the last attempt failed.",
    Battery => "battery", "UPower's display device: charge, what the battery is doing, and the time estimates when it has them.",
    System => "system", "A clock that ticks once a second.",
    Brightness => "brightness", "The screen backlight, as a percentage.",
    Workspaces => "workspaces", "The compositor's workspaces per output, and the focused toplevel window.",
    Power => "power", "power-profiles-daemon's platform profiles, plus whether you are on mains and how many watts are moving.",
    Applications => "applications", "The installed desktop entries, listed and indexed by the `app_id` a window reports.",
    Files => "files", "The files in each folder a config asked to watch, kept current through inotify.",
    Storage => "storage", "Every JSON file a config declared with `persistent_table`, keyed by its absolute path.",
    Idle => "idle", "Whether anything is holding the session awake, and which application it is. Its thresholds and the inhibit pair are methods on the same member.",
    Processes => "processes", "Every long-running program a config declared with `session_process`: whether it is up, since when, and how the last run ended.",
}

impl Capability {
    /// Resolves a Renderer-supplied wire string at the trust boundary, or returns `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|capability| capability.as_str() == name)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
/// Guarded JSON-RPC 2.0 envelope for a Lua write action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandEnvelope {
    pub jsonrpc: String,
    pub method: String,
    pub params: CommandParams,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandParams {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub arguments: Vec<serde_json::Value>,
    pub expected_revision: u32,
}

/// Supervisor update on system changes that hydrates active Lua signals. `apply_state_snapshot`
/// (`renderer/src/socket.rs`) routes by `capability` (ADR-0029); `revision` is that capability's
/// state-version counter (ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub capability: String,
    pub revision: u32,
    pub payload: serde_json::Value,
}

/// First frame on every control-socket connection, identifying the generation for commands and
/// pushes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionHandshake {
    pub generation_id: u32,
}

/// Control clients (`obelisk set`, `obelisk toggle`) use this `generation_id` in
/// [`ConnectionHandshake`] (ADR-0112). It is not a generation: the Supervisor registers no
/// outbound channel or snapshot replay for a one-frame peer that hangs up. `u32::MAX` because
/// generations count up from zero and a real one will never reach it.
pub const CONTROL_CLIENT_GENERATION: u32 = u32::MAX;

/// External write to a config `state(name, initial)` signal (ADR-0112), such as
/// `obelisk set launcher_open true`. A control client sends it as [`RendererFrame`], the
/// Supervisor forwards it as [`SupervisorFrame`] to the authoritative generation, and that
/// generation applies the same marshal checks as `signal:set()`, refusing undeclared names. This
/// is the compositor keybind's only write path into a running config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetState {
    /// Name passed to `state(name, initial)`.
    pub name: String,
    pub write: StateWrite,
}

/// Operation applied by [`SetState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StateWrite {
    /// Store this value, converted to Lua like a capability payload.
    Set(serde_json::Value),
    /// Flip a boolean. Refused on any other value, since a keybind cannot know the current one
    /// and "toggle" means nothing else.
    Toggle,
    /// `obelisk toggle <name> <value>`: store this value, unless the state already holds it, in
    /// which case restore the initial the config declared. One keybind opens and closes a modal
    /// whose state is the name of the one showing (`state("modal", "")`).
    ToggleTo(serde_json::Value),
}

/// `obelisk call <name> [json...]`: one call into a config-exported `action(name, fn)` (ADR-0197).
///
/// `name` is opaque and never split. `"rec.toggle"` is one key; the dot groups for a reader the way
/// a Lua module path does, and nothing here parses it, so an action may contain any character its
/// config wrote.
///
/// Distinct from [`CommandParams`], which is the config calling *out* to a capability and carries a
/// `generation_id` and `expected_revision` describing the Renderer's view of that capability. An
/// external caller has neither and needs neither.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Call {
    /// Assigned by the Supervisor, not the client: it owns the pending table and the reply route,
    /// and a client id would let one peer answer another's call.
    pub id: u64,
    pub name: String,
    pub arguments: Vec<serde_json::Value>,
}

/// The answer to one [`Call`], carrying `id` back so the Supervisor can find the peer that waits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallResult {
    pub id: u64,
    pub outcome: CallOutcome,
}

/// What a [`Call`] produced. A handler returning nothing and one returning `nil` are both
/// `Returned(null)`: Lua cannot tell them apart, and inventing a difference here would invent one
/// in every config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CallOutcome {
    /// The handler ran and returned this. `null` is a value, not an absence.
    Returned(serde_json::Value),
    /// No such action, a handler that raised, or a returned value that would not marshal. A
    /// returned `{ error = ... }` table is **not** this: it is a config returning a table.
    Failed(String),
}

/// Supervisor -> Renderer: re-evaluate `shell.lua`. Echo `sequence` in every response so a second
/// file-change event before the first completes cannot be mistaken for the current round trip.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReevaluateRequest {
    pub sequence: u64,
}

/// Renderer -> Supervisor: the [`ReevaluateRequest`] outcome. The Renderer classifies
/// Unchanged vs TopologyChanged; the Supervisor dispatches on that verdict.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReevaluateReport {
    Unchanged {
        sequence: u64,
    },
    TopologyChanged {
        sequence: u64,
    },
    /// `shell.lua` failed. The Renderer keeps its prior scene and enters rescue state; `error` is
    /// only for Supervisor logging.
    Failed {
        sequence: u64,
        error: String,
    },
}

/// Supervisor -> Renderer: apply the pending evaluation with the same `sequence`, only after
/// [`ReevaluateReport::Unchanged`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyPendingReload {
    pub sequence: u64,
}

/// Tells the Candidate to compile and draw its first GPU frame. Not a `CommandEnvelope` because its
/// direction and shape differ (ADR-0019).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivateDraw {
    pub nonce: u64,
}

/// The Candidate's one-time report that every tracked Wayland surface staged its null buffer and
/// awaits `ActivateDraw`. `surfaces` lists surface IDs, not monitor IDs (ADR-0025).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadySignal {
    pub surfaces: Vec<String>,
}

/// One message per surface ID after its `wp_presentation_feedback` `presented` event (ADR-0019).
/// This is the Candidate's report; the all-surfaces barrier is in `reload::run_swap`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresentationEvidence {
    pub nonce: u64,
    pub surface_id: String,
}

/// Makes the superseded generation stop treating `surface_id` as authoritative. Per-surface
/// input-region/focus wiring does not exist yet (ADR-0025).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeselectInput {
    pub surface_id: String,
}

/// Gives the new generation ownership of `surface_id`. Currently inert for the same reason as
/// `DeselectInput`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromoteGeneration {
    pub surface_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

/// Supervisor -> Renderer: one `ext_idle_notification_v1` event, with wire values `"idled"` and
/// `"resumed"` (ADR-0032). `#[serde(rename)]` pins the protocol's lowercase names instead of
/// Rust's derived PascalCase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdleState {
    #[serde(rename = "idled")]
    Idled,
    #[serde(rename = "resumed")]
    Resumed,
}

/// Supervisor -> Renderer: an `ext_idle_notification_v1` event fanned out to `generation_id`
/// (ADR-0032). The Renderer finds the callback by `threshold_sec`, the listener's registration
/// duration, not through `StateSnapshot`/`revision`; idle is event-shaped, not pollable state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdleEvent {
    pub generation_id: u32,
    pub threshold_sec: u64,
    pub state: IdleState,
}

/// Supervisor -> Renderer: one stdout/stderr line from a `process.run` child. `id` is the
/// Renderer-assigned spawning `"process"`/`"run"` `CommandEnvelope.id`, not a Supervisor result
/// (ADR-0026).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessOutputLine {
    pub id: u64,
    pub stream: ProcessStream,
    pub line: String,
}

/// Supervisor -> Renderer: the `process.run` child for `id` exited. `code` is absent when
/// [`std::process::ExitStatus::code()`] returns `None`, including signal kills and no spawn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessExited {
    pub id: u64,
    pub code: Option<i32>,
}

/// Renderer -> Supervisor: completed `textfield` `secure_submit` (ADR-0005/ADR-0009/ADR-0027).
/// `secret` is read once through `SecureBuffer::expose_secret`, never through
/// [`CommandParams::arguments`], whose `Vec<serde_json::Value>` would leave a plaintext copy
/// `.zeroize()` cannot reach. The Renderer zeroizes the source in `secure_submit_frame`
/// (`renderer/src/wayland/mod.rs`) and this copy after `pump` writes it
/// (`renderer/src/socket.rs`). Because the frame crosses an unbounded, unwrapped channel,
/// `Zeroize`/`ZeroizeOnDrop` also scrub failed sends and buffered frames when `outbound_rx` drops.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SecureSubmit {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub secret: Vec<u8>,
}

/// Hand-written `Debug` prints only `secret`'s length. `RendererFrame` derives `Debug`, and two
/// log lines call `SocketCandidateLink::recv_matching` (`supervisor/src/reload_link.rs`) on
/// rejected frames; deriving here would put a mid-swap password in the journal. `ZeroizeOnDrop`
/// keeps plaintext from outliving its read, but a formatter or future `{:?}` path can defeat it.
impl std::fmt::Debug for SecureSubmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureSubmit")
            .field("generation_id", &self.generation_id)
            .field("capability", &self.capability)
            .field("action", &self.action)
            .field("secret", &format_args!("<{} bytes redacted>", self.secret.len()))
            .finish()
    }
}

/// Supervisor -> Renderer: take or release `ext_session_lock_v1` (ADR-0042, ADR-0052 decision 1).
/// One flag covers both directions; the Renderer matches its lock state to the flag and reports
/// the outcome. Only `locked = false` may call `unlock_and_destroy`, and only after the
/// Supervisor's PAM worker returns [`PamOutcome::Success`]; the Renderer does not enforce it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetSessionLock {
    pub locked: bool,
}

/// Renderer report of the lock state (ADR-0052 decision 4). The Supervisor gates generation swaps
/// on it (ADR-0042): "never acquired" differs from "acquired then torn down" even though both end
/// unlocked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LockOutcome {
    /// `ext_session_lock_v1::locked` arrived; lock surfaces are up and swaps are blocked.
    Locked,
    /// Never acquired: no `lock` node (ADR-0052 decision 3), immediate compositor `finished`, or
    /// no advertised `ext_session_lock_manager_v1`.
    Refused(String),
    /// `finished` after `Locked`: the compositor tore it down through its secure mechanism, not a
    /// denial or a Supervisor request.
    Finished,
    /// `unlock_and_destroy` ran for `SetSessionLock { locked: false }`.
    Unlocked,
}

/// Renderer -> Supervisor: one [`LockOutcome`] per lock state change. The connection already
/// carries the generation id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockReport {
    pub outcome: LockOutcome,
}

/// Supervisor -> Renderer frames, adjacently tagged so one read loop dispatches on `kind`.
/// `content = "data"` is required because [`ReevaluateReport`] is itself an enum and cannot merge
/// into an internally-tagged flat object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum SupervisorFrame {
    StateSnapshot(StateSnapshot),
    Reevaluate(ReevaluateRequest),
    ApplyPendingReload(ApplyPendingReload),
    ActivateDraw(ActivateDraw),
    DeselectInput(DeselectInput),
    PromoteGeneration(PromoteGeneration),
    ProcessOutput(ProcessOutputLine),
    ProcessExited(ProcessExited),
    IdleEvent(IdleEvent),
    SetSessionLock(SetSessionLock),
    /// A control client's `state` write, forwarded to the authoritative generation (ADR-0112).
    SetState(SetState),
    /// A control client's `obelisk call`, forwarded to the authoritative generation (ADR-0197).
    Call(Call),
    /// That call's answer, routed back to the waiting control client (ADR-0197).
    CallResult(CallResult),
}

/// Renderer -> Supervisor frames, tagged like [`SupervisorFrame`]. `Command` is the Lua-write
/// envelope; `ReevaluateReport` is the reload verdict; `ReadySignal`/`PresentationEvidence` are
/// generation swap reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum RendererFrame {
    Command(CommandEnvelope),
    ReevaluateReport(ReevaluateReport),
    ReadySignal(ReadySignal),
    PresentationEvidence(PresentationEvidence),
    SecureSubmit(SecureSubmit),
    LockReport(LockReport),
    /// Control-client frame, not a Renderer frame: `obelisk set`/`obelisk toggle` uses
    /// [`CONTROL_CLIENT_GENERATION`] (ADR-0112). It stays in this enum because the listener has one
    /// decoder for every peer; a separate peer type would duplicate it.
    SetState(SetState),
    /// Control-client frame like [`Self::SetState`]: `obelisk call` (ADR-0197). Its `id` is zero on
    /// the way in; the Supervisor assigns the real one when it forwards.
    Call(Call),
    /// A generation answering a forwarded [`Call`] (ADR-0197).
    CallResult(CallResult),
    /// Starts a reload cycle: the Supervisor bumps its sequence and sends the
    /// [`ReevaluateRequest`] (ADR-0041 decision 4). This carries no sequence; only the Supervisor
    /// owns `next_sequence`, and `is_current_reload` (`supervisor/src/main.rs`) drops reports that
    /// do not match the last one sent.
    RequestReload,
    /// Idempotently starts `capability`'s controller when this generation first reads
    /// `obelisk.<capability>` (ADR-0070 decision 1) or a scene's `secure_submit` names it (decision
    /// 5). No generation ID is needed because the socket identifies the sender, as with
    /// [`Self::RequestReload`]. An existing name is logged and dropped (decision 3).
    StartCapability {
        capability: String,
    },
}

/// One-shot result from the Supervisor's re-exec'd PAM worker, written once to worker stdout as a
/// `shared::framing` JSON frame when its PAM conversation ends (ADR-0028). It crosses a different
/// process boundary from `RendererFrame`/`SupervisorFrame`, so is not reused as one. There is no
/// `OtherError`: spawn failure, pipe I/O, a wedged worker or an undecodable frame returns
/// `io::Result::Err` from `supervisor::pam_worker::exchange_over`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PamOutcome {
    Success,
    StartFailed(String),
    AuthFailed,
    MaxTries,
    PamError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secure_submit_never_formats_its_secret() {
        let submit = SecureSubmit {
            generation_id: 3,
            capability: "polkit".into(),
            action: "authenticate".into(),
            secret: b"hunter2".to_vec(),
        };

        // Whole rejected frames go through this `Debug` via the enum too.
        let rendered = format!("{:?}", RendererFrame::SecureSubmit(submit));

        assert!(rendered.contains("<7 bytes redacted>"), "the length is the only thing worth logging: {rendered}");
        assert!(!rendered.contains("104"), "a byte of the plaintext reached the formatter: {rendered}");
        assert!(rendered.contains("polkit"), "everything that is not the secret still prints: {rendered}");
    }

    #[test]
    fn command_envelope_matches_idl_wire_format() {
        let wire = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "ExecuteCommand",
            "params": {
                "generation_id": 4,
                "capability": "audio",
                "action": "set_volume",
                "arguments": [0.75],
                "expected_revision": 42
            },
            "id": 105
        });

        let envelope: CommandEnvelope = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(envelope.jsonrpc, "2.0");
        assert_eq!(envelope.method, "ExecuteCommand");
        assert_eq!(envelope.id, 105);
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "audio");
        assert_eq!(envelope.params.action, "set_volume");
        assert_eq!(envelope.params.expected_revision, 42);

        assert_eq!(serde_json::to_value(&envelope).unwrap(), wire);
    }

    #[test]
    fn state_snapshot_round_trips() {
        let snapshot = StateSnapshot {
            capability: "audio".to_string(),
            revision: 42,
            payload: serde_json::json!({"volume": 0.75}),
        };

        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire, serde_json::json!({ "capability": "audio", "revision": 42, "payload": {"volume": 0.75} }));
        let parsed: StateSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn state_snapshot_routes_by_capability_not_just_payload_shape() {
        // ADR-0029: `capability` tells two structurally identical payloads apart.
        let audio = StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({}) };
        let network = StateSnapshot { capability: "network".to_string(), revision: 1, payload: serde_json::json!({}) };
        assert_ne!(audio, network);
    }

    #[test]
    fn connection_handshake_round_trips() {
        let handshake = ConnectionHandshake { generation_id: 3 };
        let wire = serde_json::to_value(handshake).unwrap();
        assert_eq!(wire, serde_json::json!({ "generation_id": 3 }));

        let parsed: ConnectionHandshake = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, handshake);
    }

    #[test]
    fn supervisor_frame_state_snapshot_is_adjacently_tagged() {
        let frame = SupervisorFrame::StateSnapshot(StateSnapshot {
            capability: "audio".to_string(),
            revision: 1,
            payload: serde_json::json!({"volume": 0.5}),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "StateSnapshot", "data": { "capability": "audio", "revision": 1, "payload": {"volume": 0.5} } })
        );

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_reevaluate_is_adjacently_tagged() {
        let frame = SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 7 });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "Reevaluate", "data": { "sequence": 7 } }));

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_apply_pending_reload_is_adjacently_tagged() {
        let frame = SupervisorFrame::ApplyPendingReload(ApplyPendingReload { sequence: 9 });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "ApplyPendingReload", "data": { "sequence": 9 } }));

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn renderer_frame_command_is_adjacently_tagged() {
        let envelope = CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "ExecuteCommand".to_string(),
            params: CommandParams {
                generation_id: 4,
                capability: "audio".to_string(),
                action: "set_volume".to_string(),
                arguments: vec![serde_json::json!(0.75)],
                expected_revision: 42,
            },
            id: 105,
        };
        let frame = RendererFrame::Command(envelope.clone());
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["kind"], "Command");
        assert_eq!(wire["data"]["method"], "ExecuteCommand");

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        match parsed {
            RendererFrame::Command(parsed_envelope) => assert_eq!(parsed_envelope.id, envelope.id),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn renderer_frame_reevaluate_report_variants_round_trip() {
        for report in [
            ReevaluateReport::Unchanged { sequence: 1 },
            ReevaluateReport::TopologyChanged { sequence: 2 },
            ReevaluateReport::Failed { sequence: 3, error: "syntax error".to_string() },
        ] {
            let frame = RendererFrame::ReevaluateReport(report.clone());
            let wire = serde_json::to_value(&frame).unwrap();
            assert_eq!(wire["kind"], "ReevaluateReport");

            let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, RendererFrame::ReevaluateReport(report));
        }
    }

    #[test]
    fn supervisor_frame_activate_draw_is_adjacently_tagged() {
        let frame = SupervisorFrame::ActivateDraw(ActivateDraw { nonce: 42 });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "ActivateDraw", "data": { "nonce": 42 } }));

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_deselect_input_is_adjacently_tagged() {
        let frame = SupervisorFrame::DeselectInput(DeselectInput { surface_id: "main_bar".to_string() });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "DeselectInput", "data": { "surface_id": "main_bar" } }));

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_promote_generation_is_adjacently_tagged() {
        let frame = SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id: "overlay_canvas".to_string() });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "PromoteGeneration", "data": { "surface_id": "overlay_canvas" } })
        );

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn renderer_frame_ready_signal_is_adjacently_tagged() {
        let frame = RendererFrame::ReadySignal(ReadySignal {
            surfaces: vec!["main_bar".to_string(), "overlay_canvas".to_string()],
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "ReadySignal", "data": { "surfaces": ["main_bar", "overlay_canvas"] } })
        );

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn renderer_frame_presentation_evidence_is_adjacently_tagged() {
        let frame = RendererFrame::PresentationEvidence(PresentationEvidence {
            nonce: 7,
            surface_id: "wallpaper_layer@DP-1".to_string(),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "PresentationEvidence", "data": { "nonce": 7, "surface_id": "wallpaper_layer@DP-1" } })
        );

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn renderer_frame_request_reload_is_a_bare_kind_with_no_data() {
        // The one payload-free frame either direction has: serde omits `data` entirely for a
        // unit variant, so the decoder must accept this shape.
        let wire = serde_json::to_value(RendererFrame::RequestReload).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "RequestReload" }));

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, RendererFrame::RequestReload);
    }

    #[test]
    fn renderer_frame_secure_submit_is_adjacently_tagged() {
        let frame = RendererFrame::SecureSubmit(SecureSubmit {
            generation_id: 4,
            capability: "polkit".to_string(),
            action: "authenticate".to_string(),
            secret: b"hunter2".to_vec(),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "kind": "SecureSubmit",
                "data": { "generation_id": 4, "capability": "polkit", "action": "authenticate", "secret": [104, 117, 110, 116, 101, 114, 50] }
            })
        );

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    /// An unbounded channel carries bare `SecureSubmit` values, so the type must scrub `secret`
    /// on zeroize/drop.
    #[test]
    fn zeroizing_a_secure_submit_clears_its_secret() {
        let mut submit = SecureSubmit {
            generation_id: 4,
            capability: "polkit".to_string(),
            action: "authenticate".to_string(),
            secret: b"hunter2".to_vec(),
        };

        submit.zeroize();

        assert_eq!(submit.secret, Vec::<u8>::new());
    }

    #[test]
    fn supervisor_frame_process_output_is_adjacently_tagged() {
        let frame = SupervisorFrame::ProcessOutput(ProcessOutputLine {
            id: 3,
            stream: ProcessStream::Stdout,
            line: "hello".to_string(),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "ProcessOutput", "data": { "id": 3, "stream": "Stdout", "line": "hello" } })
        );

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_process_exited_is_adjacently_tagged() {
        for code in [Some(3), None] {
            let frame = SupervisorFrame::ProcessExited(ProcessExited { id: 3, code });
            let wire = serde_json::to_value(&frame).unwrap();
            assert_eq!(wire, serde_json::json!({ "kind": "ProcessExited", "data": { "id": 3, "code": code } }));

            let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, frame);
        }
    }

    #[test]
    fn supervisor_frame_idle_event_is_adjacently_tagged() {
        for (state, wire_state) in [(IdleState::Idled, "idled"), (IdleState::Resumed, "resumed")] {
            let frame = SupervisorFrame::IdleEvent(IdleEvent { generation_id: 4, threshold_sec: 30, state });
            let wire = serde_json::to_value(&frame).unwrap();
            assert_eq!(
                wire,
                serde_json::json!({ "kind": "IdleEvent", "data": { "generation_id": 4, "threshold_sec": 30, "state": wire_state } })
            );

            let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, frame);
        }
    }

    #[test]
    fn pam_outcome_round_trips_a_unit_and_a_data_carrying_variant() {
        for outcome in [PamOutcome::AuthFailed, PamOutcome::StartFailed("pam_start failed".to_string())] {
            let wire = serde_json::to_value(&outcome).unwrap();
            let parsed: PamOutcome = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, outcome);
        }
    }
}

#[cfg(test)]
mod capability_tests {
    use super::Capability;

    #[test]
    fn every_entry_round_trips_through_its_name() {
        // One `roster!` list makes omission from `ALL` or `as_str` unrepresentable; this pins
        // `from_name` agreeing with the two wire-facing matches.
        assert_eq!(Capability::ALL.len(), 22, "a variant was added or removed; check every iterator over ALL");
        for capability in Capability::ALL {
            assert_eq!(Capability::from_name(capability.as_str()), Some(*capability));
        }
    }

    #[test]
    fn every_name_is_unique_so_two_variants_cannot_claim_one_lua_member() {
        let mut names: Vec<&str> = Capability::ALL.iter().map(|capability| capability.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two capabilities share a name; `from_name` would resolve only the first");
    }

    #[test]
    fn the_serde_spelling_is_the_same_string_as_as_str() {
        // `StateSnapshot::capability` is written from `as_str` and read by configs; if serde
        // ever disagreed, a payload would arrive under a name nothing is listening on.
        for capability in Capability::ALL {
            let json = serde_json::to_string(capability).unwrap();
            assert_eq!(json, format!("\"{}\"", capability.as_str()));
        }
    }

    #[test]
    fn a_name_that_is_not_on_the_roster_resolves_to_nothing() {
        // `process` is command-addressable, not a capability, and never starts. It sits one
        // letter from `processes`, which is a capability, and the two route through different
        // arms of `main.rs`; a config's `process.run` reaching the session-process controller
        // would spawn something nothing reaps per generation.
        assert_eq!(Capability::from_name("process"), None);
        assert_eq!(Capability::from_name("processes"), Some(Capability::Processes));
        assert_eq!(Capability::from_name("screens"), None);
        assert_eq!(Capability::from_name(""), None);
        assert_eq!(Capability::from_name("Audio"), None);
    }
}
