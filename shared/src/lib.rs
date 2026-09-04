use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

pub mod framing;
mod paths;
mod secure_buffer;
pub use paths::{
    CHECK_ENV, CONFIG_DIR_ENV, GENERATION_ID_ENV, config_dir, control_socket_path, session_locked_flag_path,
    shell_lua_path,
};
pub use secure_buffer::SecureBuffer;
pub use zeroize::{Zeroize, Zeroizing};

/// The capability roster (ADR-0037; CONTEXT.md's Capability roster entry): every
/// snapshot-hydrated capability. Each variant's [`Capability::as_str`] name is the Lua name under
/// `oblisk.<name>` (§ 2) and the `capability` field of every command through it (§ 3.2): one
/// spelling for all three, so a config reading `oblisk.audio` cannot write to something else.
/// First read of `oblisk.<name>` starts that capability's controller on the Supervisor (ADR-0070);
/// the member reads `nil` until the first `StateSnapshot`, so an unread name costs nothing.
/// `idle` is absent (event-shaped, not snapshot state, ADR-0032); the Supervisor's `Startable`
/// covers it. `polkit` is on it (ADR-0114), and a `secure_submit` naming it starts it too.
///
/// An enum, not the `&[&str]` this replaces (ADR-0076): matched in the two places deciding whether
/// a capability starts and whether its commands dispatch, where strings once silently accepted an
/// unimplemented name, leaving its Lua member `nil` forever. Exhaustive matches turn that into a
/// build failure. Declared once here; [`Capability::ALL`] and [`Capability::as_str`] derive from
/// this one list, so a variant forgotten from `ALL` can no longer compile while silently missing
/// from the Lua namespace, the stubs and the schema check.
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

            /// The one wire/Lua spelling, matching this enum's `snake_case` serde rename so the
            /// JSON a `StateSnapshot` carries and the name a config indexes are the same string.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Capability::$variant => $name),+
                }
            }

            /// One line naming this capability, for the `oblisk.<name>` field in generated stubs
            /// (`supervisor/src/stubs.rs`); here, not the renderer, so a new variant needs one.
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
}

impl Capability {
    /// The roster entry a wire string names, or `None` off it. The Renderer sends these as free
    /// strings, so this is a trust boundary, not a lookup that cannot fail.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|capability| capability.as_str() == name)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
/// Guarded JSON-RPC 2.0 envelope wrapping a Lua write action. See docs/oblisk-idl-api-specs.md
/// §7.2.
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

/// Emitted by the Supervisor on system changes to hydrate active Lua signals. `capability` names
/// which live Lua signal this hydrates; `apply_state_snapshot` (`renderer/src/socket.rs`) routes
/// by this field (ADR-0029). `revision` is that capability's own state-version counter (ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub capability: String,
    pub revision: u32,
    pub payload: serde_json::Value,
}

/// Identifies a connection's generation. Sent first on every control-socket connection, so the
/// Supervisor addresses commands and pushes to the right generation, not one assumed peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionHandshake {
    pub generation_id: u32,
}

/// The `generation_id` a control client -- `oblisk set`, `oblisk toggle` -- hands over in its
/// [`ConnectionHandshake`] (ADR-0112). Not a generation: the Supervisor registers no outbound
/// channel for it and replays no snapshots to it, since the peer sends one frame and hangs up.
/// `u32::MAX` because generations count up from zero and a real one will never reach it.
pub const CONTROL_CLIENT_GENERATION: u32 = u32::MAX;

/// A write to one of the config's `state(name, initial)` signals from outside the shell (ADR-0112):
/// what `oblisk set launcher_open true` becomes on the wire. Sent by a control client to the
/// Supervisor as a [`RendererFrame`], forwarded to the authoritative generation as a
/// [`SupervisorFrame`], and applied there exactly as the config's own `signal:set()` would be --
/// marshal-checked, refused by name when no such state is declared. The one door a compositor
/// keybind has into a running config, and deliberately no wider than the config's own write path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetState {
    /// The `name` the config passed to `state(name, initial)`.
    pub name: String,
    pub write: StateWrite,
}

/// What a [`SetState`] does to the signal it names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StateWrite {
    /// Store this value. Converted to a Lua value the way a capability payload is.
    Set(serde_json::Value),
    /// Flip a boolean. Refused on any other value, since a keybind cannot know the current one
    /// and "toggle" means nothing else.
    Toggle,
}

/// Supervisor -> Renderer: re-evaluate `shell.lua` now. `sequence` is echoed back on every
/// response so a superseded round trip (a second file-change event before the first completes)
/// can be told apart from the current one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReevaluateRequest {
    pub sequence: u64,
}

/// Renderer -> Supervisor: the outcome of one [`ReevaluateRequest`]. The Renderer classifies
/// Unchanged-vs-TopologyChanged itself; the Supervisor only needs the verdict to dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReevaluateReport {
    Unchanged {
        sequence: u64,
    },
    TopologyChanged {
        sequence: u64,
    },
    /// `shell.lua` failed to evaluate. The Renderer keeps its prior scene and enters rescue state
    /// locally; `error` is for the Supervisor's own logging only.
    Failed {
        sequence: u64,
        error: String,
    },
}

/// Supervisor -> Renderer: apply the pending evaluation from the [`ReevaluateRequest`] carrying
/// this same `sequence`, sent only after a [`ReevaluateReport::Unchanged`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyPendingReload {
    pub sequence: u64,
}

/// § 15.2 point 3 ("Activate Draw"). Not a `CommandEnvelope`: wrong direction/shape (ADR-0019).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivateDraw {
    pub nonce: u64,
}

/// § 15.2 points 2-3 ("Null-Buffer Staging"): the Candidate's one-time report that every tracked
/// Wayland surface staged its null buffer and awaits `ActivateDraw`. `surfaces` is a surface_id
/// list, not a monitor id list (ADR-0025).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadySignal {
    pub surfaces: Vec<String>,
}

/// § 15.3 point 4 ("Evidence Verification"): one message per surface_id that received its
/// `wp_presentation_feedback` `presented` event (ADR-0019).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresentationEvidence {
    pub nonce: u64,
    pub surface_id: String,
}

/// § 15.4 point 1 ("Input Deselection"): tells the superseded generation to stop treating
/// `surface_id` as authoritative. No per-surface input-region/focus wiring exists yet (ADR-0025).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeselectInput {
    pub surface_id: String,
}

/// § 15.4 point 2 ("Candidate Promotion"): tells the newly-promoted generation it now owns
/// `surface_id`. Currently inert for the same reason as `DeselectInput`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromoteGeneration {
    pub surface_id: String,
}

/// Which of a `process.run`-spawned child's streams one [`ProcessOutputLine`] came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

/// Supervisor -> Renderer: `"idled"` or `"resumed"`, one `ext_idle_notification_v1` event
/// (ADR-0032). `#[serde(rename)]` on each variant, not the derived `Idled`/`Resumed`: ADR-0032
/// pins the wire value to the protocol's own lowercase event names, not Rust's PascalCase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdleState {
    #[serde(rename = "idled")]
    Idled,
    #[serde(rename = "resumed")]
    Resumed,
}

/// Supervisor -> Renderer: one `ext_idle_notification_v1` `idled`/`resumed` event, fanned out to
/// `generation_id` (ADR-0032). `threshold_sec` is the listener's registration duration, which the
/// Renderer looks its callback up by, not through `StateSnapshot`/`revision`: idle is
/// event-shaped, not pollable state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdleEvent {
    pub generation_id: u32,
    pub threshold_sec: u64,
    pub state: IdleState,
}

/// Supervisor -> Renderer: one line of a `process.run`-spawned child's stdout/stderr. `id` is the
/// value the Renderer assigned in the spawning `"process"`/`"run"` `CommandEnvelope.id`: assigned
/// client-side rather than handed back by the Supervisor (ADR-0026).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessOutputLine {
    pub id: u64,
    pub stream: ProcessStream,
    pub line: String,
}

/// Supervisor -> Renderer: `id`'s `process.run`-spawned child has exited. `code` is absent when
/// [`std::process::ExitStatus::code()`] returns `None`: killed by signal, or never spawned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessExited {
    pub id: u64,
    pub code: Option<i32>,
}

/// Renderer -> Supervisor: one completed `textfield` `secure_submit` (ADR-0005/ADR-0009/ADR-0027).
/// `secret` is read once via `SecureBuffer`'s one sanctioned read (`expose_secret`), never through
/// [`CommandParams::arguments`], whose `Vec<serde_json::Value>` would leave a plaintext copy
/// `.zeroize()` can't reach. The Renderer zeroizes the source buffer once it's read into this
/// frame (`secure_submit_frame`, `renderer/src/wayland/mod.rs`) and this frame's own copy once its
/// wire write completes (`pump`, `renderer/src/socket.rs`). `Zeroize`/`ZeroizeOnDrop` back that
/// up: the frame crosses an unbounded, unwrapped channel, so a dropped-not-written path (a failed
/// send, or buffered when `outbound_rx` drops) must scrub `secret` too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SecureSubmit {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub secret: Vec<u8>,
}

/// Supervisor -> Renderer: take or release the `ext_session_lock_v1` session lock (ADR-0042,
/// ADR-0052 decision 1). One command for both directions, not a `Lock`/`Unlock` pair: the
/// Renderer just matches lock state to this flag and reports the outcome. Only `locked = false`
/// may call `unlock_and_destroy`, and only once the Supervisor's PAM worker returns
/// [`PamOutcome::Success`]: that guarantee lives at one call site, not trusted to the Renderer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetSessionLock {
    pub locked: bool,
}

/// What became of the lock, reported by the Renderer that holds it (ADR-0052 decision 4). The
/// Supervisor gates generation swaps on this (ADR-0042): "never acquired" needs different handling
/// from "acquired then torn down" even though both end unlocked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LockOutcome {
    /// `ext_session_lock_v1::locked` arrived. Lock surfaces are up and swaps are blocked.
    Locked,
    /// Never acquired: no `lock` node declared (ADR-0052 decision 3), the compositor denied the
    /// request with an immediate `finished`, or `ext_session_lock_manager_v1` isn't advertised.
    Refused(String),
    /// `finished` after a `Locked`: the compositor tore the lock down through its own secure
    /// mechanism. Not a denial, and not something the Supervisor asked for.
    Finished,
    /// `unlock_and_destroy` was called, in response to a `SetSessionLock { locked: false }`.
    Unlocked,
}

/// Renderer -> Supervisor: one [`LockOutcome`] per lock state change. The connection already
/// carries the sending generation's id, so this doesn't repeat it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockReport {
    pub outcome: LockOutcome,
}

/// Every frame the Supervisor can push to a Renderer connection, adjacently tagged so one read
/// loop can dispatch on `kind`. `content = "data"`, not internally-tagged, because
/// [`ReevaluateReport`] is itself an enum and can't merge into an internally-tagged flat object.
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
    /// A control client's write to a `state` signal, forwarded to the authoritative generation
    /// (ADR-0112).
    SetState(SetState),
}

/// Every frame a Renderer connection can send to the Supervisor, tagged like [`SupervisorFrame`].
/// `Command` is § 7.2's Lua-write-action envelope; `ReevaluateReport` is the reload verdict;
/// `ReadySignal`/`PresentationEvidence` are the PBA handshake reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum RendererFrame {
    Command(CommandEnvelope),
    ReevaluateReport(ReevaluateReport),
    ReadySignal(ReadySignal),
    PresentationEvidence(PresentationEvidence),
    SecureSubmit(SecureSubmit),
    LockReport(LockReport),
    /// Not from a Renderer: `oblisk set`/`oblisk toggle` connects as
    /// [`CONTROL_CLIENT_GENERATION`] and sends this one frame (ADR-0112). In this enum because the
    /// listener decodes every peer's frames as one type, and a second peer type for one variant
    /// would be a second decoder.
    SetState(SetState),
    /// Asks the Supervisor to *start* a reload cycle: bump the sequence it owns and send the
    /// [`ReevaluateRequest`] carrying it (ADR-0041 decision 4). Carries no sequence itself: only
    /// the Supervisor holds `next_sequence`, and `is_current_reload` (`supervisor/src/main.rs`)
    /// drops any report whose sequence isn't the last one it sent, so a fabricated one is simply
    /// discarded.
    RequestReload,
    /// Asks the Supervisor to construct `capability`'s controller, sent the first time this
    /// generation's config reads `oblisk.<capability>` (ADR-0070 decision 1) or a scene's
    /// `secure_submit` names it (decision 5). Carries no generation id, same reason as
    /// [`Self::RequestReload`]: the socket knows the sender. Idempotent: an existing name is
    /// logged and dropped (decision 3).
    StartCapability {
        capability: String,
    },
}

/// The Supervisor's own PAM worker subprocess's one-shot result, written once to the worker's
/// stdout as a single `shared::framing` JSON frame when its PAM conversation ends (ADR-0028).
/// Crosses a different process boundary than `RendererFrame`/`SupervisorFrame` (Supervisor <-> its
/// re-exec'd PAM worker), so it's never reused as one. No `OtherError` variant: anything that
/// isn't a PAM-level outcome (spawn failure, pipe I/O error, a wedged worker, an undecodable
/// frame) surfaces as an `io::Result::Err` from `supervisor::pam_worker::exchange_over` instead.
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
    fn command_envelope_matches_idl_wire_format() {
        // Exact example from docs/oblisk-idl-api-specs.md §7.2.
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
        // unit variant, and the decoder must accept that shape.
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

    /// A bare `SecureSubmit` crosses an unbounded channel with no wrapper protecting it, so this
    /// type must scrub its own `secret` on zeroize/drop.
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
        // `ALL` and `as_str` come from one `roster!` list, so this cannot catch a variant missing
        // from one of them -- there is no way to write that. What it does pin is `from_name`
        // agreeing with `as_str`, which is what the two wire-facing matches depend on.
        assert_eq!(Capability::ALL.len(), 21, "a variant was added or removed; check every iterator over ALL");
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
        // A `StateSnapshot`'s `capability` field is written from `as_str` and read by configs;
        // if serde ever disagreed, a payload would arrive under a name nothing is listening on.
        for capability in Capability::ALL {
            let json = serde_json::to_string(capability).unwrap();
            assert_eq!(json, format!("\"{}\"", capability.as_str()));
        }
    }

    #[test]
    fn a_name_that_is_not_on_the_roster_resolves_to_nothing() {
        // `process` is addressable in a command envelope but is not a capability and never starts.
        assert_eq!(Capability::from_name("process"), None);
        assert_eq!(Capability::from_name("screens"), None);
        assert_eq!(Capability::from_name(""), None);
        assert_eq!(Capability::from_name("Audio"), None);
    }
}
