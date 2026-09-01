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
/// snapshot-hydrated capability. Each variant's [`Capability::as_str`] name is also the Lua name
/// it appears under, as `oblisk.<name>` (§ 2), and the `capability` field of every command
/// written through it (§ 3.2) -- one spelling for all three, so a config that reads `oblisk.audio`
/// cannot write to something else.
///
/// The Renderer hands a rostered name out on first read of `oblisk.<name>`, and that read is what
/// starts the capability's controller on the Supervisor (docs/adr/0070). Until its first
/// `StateSnapshot` the member reads `nil`, which is also what a name the config never reads costs:
/// nothing runs behind it. `idle` is deliberately absent: it's event-shaped, not snapshot state
/// (ADR-0032), and neither is `polkit`, which is reached from a `secure_submit` rather than a read
/// (docs/adr/0070 decision 5) -- the Supervisor's own `Startable` covers both.
///
/// An enum rather than the `&[&str]` this replaces (docs/adr/0076). The roster is matched on in
/// two places that decide whether a capability starts and whether its commands are dispatched, and
/// as strings both of them accepted a name nothing implemented, silently: the capability's Lua
/// member existed and stayed `nil` forever. Exhaustive matches make adding a variant a build
/// failure at exactly those two arms.
/// Declares the roster once and derives the enum, [`Capability::ALL`] and [`Capability::as_str`]
/// from that one list.
///
/// A macro rather than three hand-kept lists, because the failure it removes is specific: with
/// `ALL` written out separately, a variant added to the enum and to `as_str` but forgotten in
/// `ALL` compiled and passed every test, and was simply absent from the Lua namespace, the
/// generated stubs and the schema check -- all three of which iterate `ALL`. There is now nothing
/// to forget.
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

            /// One line naming what this capability is, for the `oblisk.<name>` field in the
            /// generated stubs (`supervisor/src/stubs.rs`). Here rather than in the renderer
            /// because this list is where a capability is declared, so a new variant cannot be
            /// added without writing one.
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
    Privacy => "privacy", "Who is holding the camera open right now. Empty means nobody is.",
    Updates => "updates", "Pending pacman upgrades, the progress of an install in flight, and whether the kernel changed under you.",
    Lock => "lock", "The session lock: whether it is held, whether a password is with PAM, and why the last attempt failed.",
    Battery => "battery", "UPower's display device: charge, what the battery is doing, and the time estimates when it has them.",
    System => "system", "The persisted state dictionary and a clock that ticks once a second.",
    Brightness => "brightness", "The screen backlight, as a percentage.",
    Workspaces => "workspaces", "The compositor's workspaces per output, and the focused toplevel window.",
    Power => "power", "power-profiles-daemon's platform profiles, plus whether you are on mains and how many watts are moving.",
    Applications => "applications", "The installed desktop entries, listed and indexed by the `app_id` a window reports.",
}

impl Capability {
    /// The roster entry a wire string names, or `None` for anything off it. The Renderer sends
    /// these as free strings, so this is a trust boundary, not a lookup that cannot fail.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|capability| capability.as_str() == name)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
/// Guarded JSON-RPC 2.0 envelope wrapping a Lua write action.
/// See docs/oblisk-idl-api-specs.md §7.2.
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

/// Emitted by the Supervisor on system changes to hydrate active Lua signals. `capability`
/// names which live Lua signal this hydrates; `renderer/src/socket.rs`'s `apply_state_snapshot`
/// routes by this field (docs/adr/0029). `revision` is that capability's own state-version
/// counter (ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub capability: String,
    pub revision: u32,
    pub payload: serde_json::Value,
}

/// Identifies a connection's generation. Sent as the first frame on every new control-socket
/// connection, so the Supervisor's listener can address commands and pushes to the right
/// generation instead of assuming exactly one peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionHandshake {
    pub generation_id: u32,
}

/// Supervisor -> Renderer: re-evaluate `shell.lua` now. `sequence` is echoed back on every
/// response so a superseded round trip (a second file-change event fires before the first
/// completes) can be told apart from the current one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReevaluateRequest {
    pub sequence: u64,
}

/// Renderer -> Supervisor: the outcome of one [`ReevaluateRequest`]. The Renderer classifies
/// Unchanged-vs-TopologyChanged itself; the Supervisor only needs the verdict to pick a dispatch
/// branch, not to run the reload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReevaluateReport {
    Unchanged {
        sequence: u64,
    },
    TopologyChanged {
        sequence: u64,
    },
    /// `shell.lua` failed to evaluate. The Renderer has already kept its prior applied scene
    /// untouched and entered rescue state locally -- `error` is for the Supervisor's own
    /// logging only.
    Failed {
        sequence: u64,
        error: String,
    },
}

/// Supervisor -> Renderer: apply the pending evaluation from the [`ReevaluateRequest`] carrying
/// this same `sequence` -- sent only after a [`ReevaluateReport::Unchanged`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyPendingReload {
    pub sequence: u64,
}

/// § 15.2 point 3 ("Activate Draw"). Not a `CommandEnvelope` -- wrong direction/shape
/// (docs/adr/0019).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivateDraw {
    pub nonce: u64,
}

/// § 15.2 points 2-3 ("Null-Buffer Staging"): the Candidate's one-time report that every tracked
/// Wayland surface has committed its null buffer and is staged, waiting for `ActivateDraw`.
/// `surfaces` is a surface_id list, not a monitor id list (docs/adr/0025 item 2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadySignal {
    pub surfaces: Vec<String>,
}

/// § 15.3 point 4 ("Evidence Verification"): one message per surface_id that received its
/// `wp_presentation_feedback` `presented` event (docs/adr/0019 item 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresentationEvidence {
    pub nonce: u64,
    pub surface_id: String,
}

/// § 15.4 point 1 ("Input Deselection"): tells the superseded generation to stop treating
/// `surface_id` as authoritative. No per-surface input-region/focus wiring exists yet to hand
/// this to (docs/adr/0025 item 4).
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
/// (docs/adr/0032). `#[serde(rename)]` on each variant, not the derived `Idled`/`Resumed`:
/// ADR-0032 pins the wire value to the protocol's own lowercase event names, not Rust's
/// PascalCase spelling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdleState {
    #[serde(rename = "idled")]
    Idled,
    #[serde(rename = "resumed")]
    Resumed,
}

/// Supervisor -> Renderer: one `ext_idle_notification_v1` `idled`/`resumed` event, fanned out to
/// `generation_id` (docs/adr/0032). `threshold_sec` is the duration this event's listener was
/// created for -- the Renderer looks up its own registered callback by this value, not through
/// the `StateSnapshot`/`revision` path: idle is event-shaped, not pollable state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdleEvent {
    pub generation_id: u32,
    pub threshold_sec: u64,
    pub state: IdleState,
}

/// Supervisor -> Renderer: one line of a `process.run`-spawned child's stdout/stderr. `id` is
/// the same value the Renderer assigned in the `"process"`/`"run"` `CommandEnvelope.id` that
/// spawned it -- assigned client-side rather than handed back by the Supervisor (docs/adr/0026).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessOutputLine {
    pub id: u64,
    pub stream: ProcessStream,
    pub line: String,
}

/// Supervisor -> Renderer: `id`'s `process.run`-spawned child has exited. `code` is absent
/// exactly when [`std::process::ExitStatus::code()`] itself would return `None` -- killed by
/// signal, or never spawned at all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessExited {
    pub id: u64,
    pub code: Option<i32>,
}

/// Renderer -> Supervisor: one completed `textfield` `secure_submit` (ADR-0005/ADR-0009/ADR-0027).
/// `secret` is read once from a `shared::SecureBuffer` via its one sanctioned read
/// (`expose_secret`) -- never routed through [`CommandParams::arguments`], whose
/// `Vec<serde_json::Value>` would leave an intermediate plaintext copy `.zeroize()` can never
/// reach. The Renderer zeroizes the source `SecureBuffer` the instant it is read into this frame
/// (`renderer/src/wayland/mod.rs`'s `secure_submit_frame`) and this frame's own plaintext copy the
/// instant its wire write completes (`renderer/src/socket.rs`'s `pump`). `Zeroize`/`ZeroizeOnDrop`
/// are a backstop: this frame crosses an unbounded channel with no wrapper protecting it, so any
/// path that drops it instead of writing it -- send failing because the socket thread is gone, or
/// still buffered when `outbound_rx` is dropped -- must scrub `secret` on drop too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SecureSubmit {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub secret: Vec<u8>,
}

/// Supervisor -> Renderer: take or release the `ext_session_lock_v1` session lock (ADR-0042,
/// ADR-0052 decision 1). One command for both directions rather than a `Lock`/`Unlock` pair: the
/// Renderer's job is the same either way, make the lock state match this flag and report what
/// happened.
///
/// `locked = false` is the *only* thing that may call `unlock_and_destroy`, and the Supervisor
/// sends it only after its PAM worker returned [`PamOutcome::Success`] -- that guarantee lives at
/// that one call site, not as a rule the Renderer is trusted to keep.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetSessionLock {
    pub locked: bool,
}

/// What became of the lock, reported by the Renderer that holds it (ADR-0052 decision 4). The
/// Supervisor gates generation swaps on this (ADR-0042), and "never acquired" needs different
/// handling from "acquired then torn down" even though both end unlocked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LockOutcome {
    /// `ext_session_lock_v1::locked` arrived. Lock surfaces are up and swaps are blocked.
    Locked,
    /// The lock was never acquired: no `lock` node declared (ADR-0052 decision 3), the compositor
    /// denied the request with an immediate `finished`, or `ext_session_lock_manager_v1` isn't
    /// advertised at all.
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
/// loop can dispatch on `kind`. `content = "data"` (not internally-tagged) because
/// [`ReevaluateReport`] is itself an enum, which can't merge into an internally-tagged wrapper's
/// flat object.
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
}

/// Every frame a Renderer connection can send to the Supervisor, same tagging scheme as
/// [`SupervisorFrame`]. `Command` is § 7.2's Lua-write-action envelope; `ReevaluateReport` is the
/// reload verdict; `ReadySignal`/`PresentationEvidence` are the PBA handshake reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum RendererFrame {
    Command(CommandEnvelope),
    ReevaluateReport(ReevaluateReport),
    ReadySignal(ReadySignal),
    PresentationEvidence(PresentationEvidence),
    SecureSubmit(SecureSubmit),
    LockReport(LockReport),
    /// Asks the Supervisor to *start* a reload cycle for this generation: bump the sequence it
    /// owns and send the [`ReevaluateRequest`] carrying it (docs/adr/0041 decision 4).
    ///
    /// Carries no sequence: the Supervisor is the only holder of `next_sequence`, and
    /// `supervisor/src/main.rs`'s `is_current_reload` drops any report whose sequence isn't the
    /// one it most recently sent, so a Renderer that fabricated one would have its own
    /// `ReevaluateReport` discarded as stale.
    RequestReload,
    /// Asks the Supervisor to construct `capability`'s controller, sent the first time this
    /// generation's config reads `oblisk.<capability>` (docs/adr/0070 decision 1) or applies a
    /// scene whose `secure_submit` names it (decision 5).
    ///
    /// Carries no generation id in the payload for the same reason [`Self::RequestReload`] carries
    /// no sequence: the socket already knows which generation wrote the frame. Idempotent -- a
    /// name whose controller exists is logged and dropped, since every generation sends its own
    /// starts (decision 3).
    StartCapability {
        capability: String,
    },
}

/// The Supervisor's own PAM worker subprocess's one-shot result, written once to the worker's
/// stdout as a single `shared::framing` JSON frame when its PAM conversation ends (ADR-0028).
/// Crosses a different process boundary than `RendererFrame`/`SupervisorFrame` (Supervisor <->
/// its own re-exec'd PAM worker), so it is never reused as one. No `OtherError` variant: every
/// failure that isn't a PAM-level outcome (spawn failure, pipe I/O error, a wedged worker timing
/// out, an undecodable frame) surfaces as an `io::Result::Err` from
/// `supervisor::pam_worker::exchange_over` instead, outside this enum entirely.
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
        assert_eq!(Capability::ALL.len(), 17, "a variant was added or removed; check every iterator over ALL");
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
        // `idle` and `polkit` are startable but deliberately off the roster; both must miss here.
        assert_eq!(Capability::from_name("idle"), None);
        assert_eq!(Capability::from_name("polkit"), None);
        assert_eq!(Capability::from_name(""), None);
        assert_eq!(Capability::from_name("Audio"), None);
    }
}
