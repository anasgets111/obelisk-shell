use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

pub mod framing;
mod paths;
mod secure_buffer;
pub use paths::{config_dir, control_socket_path, session_locked_flag_path, shell_lua_path};
pub use secure_buffer::SecureBuffer;
pub use zeroize::{Zeroize, Zeroizing};

/// The capability roster (ADR-0037; CONTEXT.md's Capability roster entry): every
/// snapshot-hydrated capability name. Each name is also the Lua name it appears under, as
/// `oblisk.<name>` (§ 2), and the `capability` field of every command written through it (§ 3.2)
/// -- one string for all three, so a config that reads `oblisk.audio` cannot write to something
/// else.
///
/// The Renderer seeds every rostered name onto `oblisk` at construction, so each reads `nil`
/// until its first `StateSnapshot` (including `sysinfo`, dormant until `sysinfo:configure`).
/// The Supervisor's `push_snapshot` debug-asserts membership, so an off-roster capability fails
/// loudly in development rather than as an index-into-nil error in a user's `shell.lua`. `idle`
/// is deliberately absent: it's event-shaped, not snapshot state (ADR-0032).
pub const CAPABILITIES: &[&str] = &[
    "audio", "network", "bluetooth", "tray", "notifications", "mpris", "sysinfo", "keyboard", "privacy", "updates", "lock", "battery", "system",
    "brightness", "workspaces", "power",
];

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
    Unchanged { sequence: u64 },
    TopologyChanged { sequence: u64 },
    /// `shell.lua` failed to evaluate. The Renderer has already kept its prior applied scene
    /// untouched and entered rescue state locally -- `error` is for the Supervisor's own
    /// logging only.
    Failed { sequence: u64, error: String },
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
        assert_eq!(wire, serde_json::json!({ "kind": "PromoteGeneration", "data": { "surface_id": "overlay_canvas" } }));

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn renderer_frame_ready_signal_is_adjacently_tagged() {
        let frame = RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string(), "overlay_canvas".to_string()] });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "ReadySignal", "data": { "surfaces": ["main_bar", "overlay_canvas"] } }));

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn renderer_frame_presentation_evidence_is_adjacently_tagged() {
        let frame = RendererFrame::PresentationEvidence(PresentationEvidence { nonce: 7, surface_id: "wallpaper_layer@DP-1".to_string() });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "PresentationEvidence", "data": { "nonce": 7, "surface_id": "wallpaper_layer@DP-1" } }));

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
        let mut submit =
            SecureSubmit { generation_id: 4, capability: "polkit".to_string(), action: "authenticate".to_string(), secret: b"hunter2".to_vec() };

        submit.zeroize();

        assert_eq!(submit.secret, Vec::<u8>::new());
    }

    #[test]
    fn supervisor_frame_process_output_is_adjacently_tagged() {
        let frame = SupervisorFrame::ProcessOutput(ProcessOutputLine { id: 3, stream: ProcessStream::Stdout, line: "hello".to_string() });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "ProcessOutput", "data": { "id": 3, "stream": "Stdout", "line": "hello" } }));

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
