use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod framing;
mod secure_buffer;
pub use secure_buffer::SecureBuffer;
pub use zeroize::Zeroize;

/// Where the control socket lives, derived from `$XDG_RUNTIME_DIR`. Both
/// `supervisor` (the listener) and `renderer` (the client) resolve this the same way, so it
/// lives here instead of being reimplemented on each side (build-steps.md Phase 9: not
/// `/tmp`, which is world-writable and unsuitable for a socket that will eventually carry
/// secure textfield submissions, ADR-0005).
pub fn control_socket_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("oblisk-shell.sock"))
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

/// Emitted by the Supervisor on system changes to hydrate active Lua signals.
/// `revision` is the capability's state-version counter (see ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub revision: u32,
    pub payload: serde_json::Value,
}

/// Identifies a connection's generation. Sent as the very first frame on every new
/// control-socket connection, before any other traffic, so the Supervisor's listener can
/// address commands and pushes to the right generation instead of assuming exactly one peer
/// (build-steps.md Phase 9; CONTEXT.md's Candidate and Authoritative generation entries).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionHandshake {
    pub generation_id: u32,
}

/// Supervisor -> Renderer: re-evaluate `shell.lua` now (build-steps.md Phase 13; CONTEXT.md,
/// Watcher). `sequence` is echoed back on every response so a superseded round trip (a second
/// file-change event fires before the first round trip completes) can be told apart from the
/// current one -- same correlation role `reload::run_pba`'s `nonce: u64` plays for
/// `ActivateDraw`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReevaluateRequest {
    pub sequence: u64,
}

/// Renderer -> Supervisor: the outcome of one [`ReevaluateRequest`]. The Renderer classifies
/// Unchanged-vs-TopologyChanged itself (it already holds both the old and new topology) -- the
/// Supervisor only needs the verdict to decide which dispatch branch to run (CONTEXT.md's
/// Watcher entry: "owns the swap-vs-in-place decision, not the reload's execution").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReevaluateReport {
    Unchanged { sequence: u64 },
    TopologyChanged { sequence: u64 },
    /// `shell.lua` failed to evaluate (syntax/runtime error, invalid top-level return, or a
    /// surface whose topology fields don't type-check). The Renderer has already kept its prior
    /// applied scene untouched and entered rescue state locally -- `error` is for the
    /// Supervisor's own logging only.
    Failed { sequence: u64, error: String },
}

/// Supervisor -> Renderer: apply the pending evaluation from the [`ReevaluateRequest`] carrying
/// this same `sequence` -- sent only after a [`ReevaluateReport::Unchanged`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyPendingReload {
    pub sequence: u64,
}

/// § 15.2 point 3 / build-steps.md Phase 8 step 4 ("Activate Draw"), now sent for real (Phase 14).
/// Matches `reload::CandidateLink::send_activate_draw`'s existing `nonce: u64` shape -- see
/// docs/adr/0019 for why this is not `CommandEnvelope` (wrong direction/shape).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivateDraw {
    pub nonce: u64,
}

/// § 15.2 points 2-3 ("Null-Buffer Staging"): the Candidate's one-time report that every tracked
/// Wayland surface has committed its null buffer and is staged, waiting for `ActivateDraw`.
/// `surfaces` is the exact set `reload::CandidateLink::recv_presentation_evidence` must later see
/// evidence for -- see docs/adr/0025 item 2 for why this is a surface_id, not a monitor id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadySignal {
    pub surfaces: Vec<String>,
}

/// § 15.3 point 4 ("Evidence Verification"), per surface (docs/adr/0019 item 5, ADR-0003).
/// One message per surface_id that received its `wp_presentation_feedback` `presented` event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresentationEvidence {
    pub nonce: u64,
    pub surface_id: String,
}

/// § 15.4 point 1 ("Input Deselection"): tells the superseded generation to stop treating
/// `surface_id` as authoritative. Real, called, currently-empty effect on the Renderer side --
/// see docs/adr/0025 item 4 (no real per-surface input-region/focus wiring exists yet).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeselectInput {
    pub surface_id: String,
}

/// § 15.4 point 2 ("Candidate Promotion"): tells the newly-promoted generation it now owns
/// `surface_id`. Same real-but-currently-inert status as `DeselectInput` -- see docs/adr/0025 item 4.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromoteGeneration {
    pub surface_id: String,
}

/// Which of a `process.run`-spawned child's streams one [`ProcessOutputLine`] came from. A real
/// enum, not a bare `"stdout"`/`"stderr"` string tag -- this codebase's Baseline Smells reject
/// that as Primitive Obsession, and [`ReevaluateReport`] already sets the "real enum" precedent
/// for a wire-level tag like this.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

/// Supervisor -> Renderer: one line of a `process.run`-spawned child's stdout/stderr
/// (`docs/oblisk-supervisor-services-dbus.md` § 12's "No Lua Blockage" half; `oblisk-idl-api-
/// specs.md` § 3.2/3.3). `id` is the same value the Renderer assigned in the `"process"`/`"run"`
/// `CommandEnvelope.id` that spawned it -- see docs/adr/0026 for why the id is assigned
/// client-side rather than handed back by the Supervisor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessOutputLine {
    pub id: u64,
    pub stream: ProcessStream,
    pub line: String,
}

/// Supervisor -> Renderer: `id`'s `process.run`-spawned child has exited. `code` is absent
/// exactly when [`std::process::ExitStatus::code()`] itself would return `None` -- killed by
/// signal (`process_handle:kill()`, or a superseded generation's own § 12 reap), or never spawned
/// at all (a `process.run` whose underlying spawn failed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessExited {
    pub id: u64,
    pub code: Option<i32>,
}

/// Every frame the Supervisor can push to a Renderer connection, adjacently tagged so a single
/// read loop can dispatch on `kind` without the connection needing a separate channel per
/// message shape. `content = "data"` (not internally-tagged) because [`ReevaluateReport`] is
/// itself an enum, which can't merge into an internally-tagged wrapper's flat object.
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
}

/// Every frame a Renderer connection can send to the Supervisor, same tagging scheme as
/// [`SupervisorFrame`]. `Command` is § 7.2's existing Lua-write-action envelope; `ReevaluateReport`
/// is Phase 13's new reload verdict; `ReadySignal`/`PresentationEvidence` are Phase 14's PBA
/// handshake reports -- all travel Renderer -> Supervisor, so they share one wire enum instead of
/// separately-typed read loops.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum RendererFrame {
    Command(CommandEnvelope),
    ReevaluateReport(ReevaluateReport),
    ReadySignal(ReadySignal),
    PresentationEvidence(PresentationEvidence),
}

/// `~/.config/oblisk/`, resolved via `$XDG_CONFIG_HOME` falling back to `$HOME/.config` (XDG
/// Base Directory order), hand-rolled rather than a new `dirs`-style dependency -- same
/// one-function reasoning `control_socket_path` already used for `$XDG_RUNTIME_DIR`. Both
/// `supervisor` (watches this directory) and `renderer` (reads `shell.lua` from it) resolve it
/// identically.
pub fn config_dir() -> io::Result<PathBuf> {
    if let Some(xdg_config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home).join("oblisk"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config").join("oblisk"))
}

/// `config_dir()` joined with `shell.lua` -- the file Phase 13 makes real (build-steps.md
/// Phase 13; `renderer/src/lua/mod.rs`'s and `renderer/src/socket.rs`'s doc comments both named
/// this as their own missing piece).
pub fn shell_lua_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("shell.lua"))
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
            revision: 42,
            payload: serde_json::json!({"volume": 0.75}),
        };

        let wire = serde_json::to_value(&snapshot).unwrap();
        let parsed: StateSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.revision, snapshot.revision);
        assert_eq!(parsed.payload, snapshot.payload);
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
        let frame = SupervisorFrame::StateSnapshot(StateSnapshot { revision: 1, payload: serde_json::json!({"volume": 0.5}) });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "StateSnapshot", "data": { "revision": 1, "payload": {"volume": 0.5} } }));

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
    fn shell_lua_path_is_config_dir_joined_with_shell_lua() {
        let path = shell_lua_path().unwrap();
        assert_eq!(path, config_dir().unwrap().join("shell.lua"));
        assert!(path.ends_with("oblisk/shell.lua"));
    }
}
