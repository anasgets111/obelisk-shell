use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

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

/// The capability roster (ADR-0037; CONTEXT.md's Capability roster entry): every
/// snapshot-hydrated capability name. The Renderer seeds one live Lua signal per rostered name
/// at construction, so each global exists from a generation's first evaluation and reads `nil`
/// until its first `StateSnapshot` arrives -- uniformly, including `sysinfo`, which stays `nil`
/// indefinitely until `sysinfo:configure` wakes its dormant pollers. The Supervisor's
/// `push_snapshot` debug-asserts membership, so a capability added there without a roster entry
/// fails on its first push in development instead of as an undefined-global error in a user's
/// `shell.lua` at boot. `idle` is deliberately absent: it's event-shaped, not snapshot state
/// (ADR-0032).
pub const CAPABILITIES: &[&str] =
    &["audio", "network", "bluetooth", "tray", "notifications", "mpris", "sysinfo", "keyboard", "privacy", "updates"];

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
/// `capability` names which live Lua signal this hydrates (e.g. `"audio"`, `"network"`) --
/// `renderer/src/socket.rs`'s `apply_state_snapshot` routes by this field instead of hardcoding
/// one global signal (docs/adr/0029; `CONTEXT.md`'s Capability/Dependency snapshot entries).
/// `revision` is that capability's own state-version counter (see ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub capability: String,
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

/// Supervisor -> Renderer: `"idled"` or `"resumed"`, one `ext_idle_notification_v1` event
/// (docs/adr/0032). A real enum, not a bare string tag -- [`ProcessStream`]'s own doc comment
/// already sets this codebase's "Baseline Smells reject that as Primitive Obsession" precedent
/// for a wire-level state tag like this one. `#[serde(rename)]` on each variant, not the derived
/// `Idled`/`Resumed`, because ADR-0032 pins the wire value to the protocol's own lowercase event
/// names (`"idled"`/`"resumed"`), not Rust's PascalCase variant spelling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdleState {
    #[serde(rename = "idled")]
    Idled,
    #[serde(rename = "resumed")]
    Resumed,
}

/// Supervisor -> Renderer: one `ext_idle_notification_v1` `idled`/`resumed` event, fanned out to
/// `generation_id` (docs/adr/0032; CONTEXT.md's Idle threshold entry). `threshold_sec` is the
/// distinct duration this event's listener was created for -- the Renderer looks up its own
/// registered callback by this value. Dispatched straight to that callback, not through the
/// `StateSnapshot`/`revision` signal-table path: idle is event-shaped, not pollable state
/// (ADR-0032's own reasoning for why this needed a new `SupervisorFrame` variant instead of
/// reusing `StateSnapshot`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdleEvent {
    pub generation_id: u32,
    pub threshold_sec: u64,
    pub state: IdleState,
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

/// Renderer -> Supervisor: one completed `textfield` `secure_submit` (build-steps.md Phase 15
/// item 2; ADR-0005/ADR-0009/ADR-0027). `secret` is the accumulated `wp-text-input-v3` input,
/// read once from a `shared::SecureBuffer` via its one sanctioned read (`expose_secret`) --
/// never routed through [`CommandParams::arguments`], whose `Vec<serde_json::Value>` would leave
/// an intermediate plaintext copy `.zeroize()` can never reach (ADR-0027, ADR-0014). The Renderer
/// `.zeroize()`s the source `SecureBuffer` the instant it has been read into this frame (see
/// `renderer/src/wayland/mod.rs`'s `secure_submit_frame`) and this frame's own plaintext copy the
/// instant its wire write completes (`renderer/src/socket.rs`'s `pump`). `Zeroize`/`ZeroizeOnDrop`
/// *are* this type's job too, as a backstop: a bare `SecureSubmit` now crosses an unbounded
/// channel on its own (docs/adr/0039 collapsed the old `SecureSubmitPayload` wrapper whose
/// `SecureBuffer` field used to carry this protection), so every path that drops the frame instead
/// of writing it -- the outbound send failing because the socket thread is gone, or a frame still
/// buffered when `outbound_rx` itself is dropped -- must scrub `secret` on drop, not just on the
/// happy-path write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SecureSubmit {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub secret: Vec<u8>,
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
    IdleEvent(IdleEvent),
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
    SecureSubmit(SecureSubmit),
}

/// The Supervisor's own PAM worker subprocess's one-shot result, written once to the
/// worker's stdout as a single `shared::framing` JSON frame when its PAM conversation ends
/// (ADR-0028) -- never reused as a `RendererFrame`/`SupervisorFrame` variant; this crosses a
/// completely different process boundary (Supervisor <-> its own re-exec'd PAM worker, not
/// Supervisor <-> Renderer). Mirrors Quickshell's own PAM exit-code taxonomy (ADR-0028), minus
/// its `OtherError` case: every failure that isn't a PAM-level outcome (spawn failure, a pipe
/// I/O error, a wedged worker timing out, an undecodable frame) surfaces as an `io::Result::Err`
/// from `supervisor::pam_worker::exchange_over` instead, outside this enum entirely -- there's
/// no code path left that would ever construct a sixth "something else went wrong" variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PamOutcome {
    Success,
    StartFailed(String),
    AuthFailed,
    MaxTries,
    PamError(String),
}

/// The workspace's tracked dev config, baked in at compile time so it resolves the same from any
/// working directory. `CARGO_MANIFEST_DIR` is this crate's own directory, so the workspace root
/// is one level up.
#[cfg(debug_assertions)]
const DEV_CONFIG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../dev-config/oblisk");

/// `~/.config/oblisk/`, resolved via `$XDG_CONFIG_HOME` falling back to `$HOME/.config` (XDG
/// Base Directory order), hand-rolled rather than a new `dirs`-style dependency -- same
/// one-function reasoning `control_socket_path` already used for `$XDG_RUNTIME_DIR`. Both
/// `supervisor` (watches this directory) and `renderer` (reads `shell.lua` from it) resolve it
/// identically.
///
/// A debug build looks in the workspace's `dev-config/oblisk/` first, so `cargo run -p supervisor`
/// boots against the tracked dev config with no environment set up. Before this, a bare
/// `cargo run` read `~/.config/oblisk/shell.lua`, which does not exist on a developer's machine,
/// and the run came up with no config at all. The surfaces still appeared, because at the time
/// they were created from a fixed Rust-owned role set rather than from the config, so the only
/// symptom was one `failed to read shell.lua` line in a wall of startup logging. docs/adr/0038
/// closed that hole from the other end: a config that fails to load now produces no surfaces at
/// all, which is loud.
///
/// `$XDG_CONFIG_HOME` still wins in both builds, which is what makes the dev branch safe to add
/// rather than a second source of truth: it is how a debug build tests against a real config
/// directory, and it keeps the existing `XDG_CONFIG_HOME=dev-config` invocation working
/// unchanged. Release builds never see the dev branch at all -- `debug_assertions` is off, so
/// `DEV_CONFIG_DIR` is not even compiled in, and no build-machine path reaches a shipped binary.
pub fn config_dir() -> io::Result<PathBuf> {
    if let Some(xdg_config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home).join("oblisk"));
    }

    // Checked rather than assumed: a debug binary run away from the tree it was built in (moved,
    // copied to another machine, `cargo install --debug`) has a `DEV_CONFIG_DIR` pointing at
    // nothing. Falling through to the XDG path there is what keeps such a binary usable instead
    // of failing on a path only the build machine ever had.
    #[cfg(debug_assertions)]
    if std::fs::metadata(DEV_CONFIG_DIR).is_ok() {
        return Ok(PathBuf::from(DEV_CONFIG_DIR));
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
        // ADR-0029: two capabilities can carry structurally identical payloads -- `capability`
        // is what tells them apart, not the payload's own shape.
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

    /// Regression test for a CONFIRMED correctness finding: a bare `SecureSubmit` now crosses an
    /// unbounded channel on its own (docs/adr/0039 collapsed the old `SecureSubmitPayload`
    /// wrapper), so this type must scrub its own `secret` on zeroize/drop rather than relying on
    /// a wrapper that no longer exists.
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

    #[test]
    fn shell_lua_path_is_config_dir_joined_with_shell_lua() {
        let path = shell_lua_path().unwrap();
        assert_eq!(path, config_dir().unwrap().join("shell.lua"));
        assert!(path.ends_with("oblisk/shell.lua"));
    }

    /// The dev branch is only worth anything if the baked-in path is real, and a `concat!` of
    /// `CARGO_MANIFEST_DIR` is exactly the kind of thing that compiles fine while pointing at
    /// nothing. This is what catches the crate being moved or the workspace being restructured
    /// without this constant following: it reads the file `config_dir` exists to find, rather
    /// than asserting on the string.
    #[cfg(debug_assertions)]
    #[test]
    fn the_baked_in_dev_config_path_holds_a_real_shell_lua() {
        let shell_lua = PathBuf::from(DEV_CONFIG_DIR).join("shell.lua");
        assert!(
            std::fs::read_to_string(&shell_lua).is_ok(),
            "DEV_CONFIG_DIR points at {DEV_CONFIG_DIR:?}, which has no readable shell.lua -- \
             a debug build resolves its config through this constant"
        );
    }

    /// `$XDG_CONFIG_HOME` must keep winning in a debug build, which is the whole reason the dev
    /// branch is safe: it is how a debug build is pointed at a real config directory, and it is
    /// what keeps the existing `XDG_CONFIG_HOME=dev-config` invocation resolving to the same
    /// place it always did. Without this the dev branch would be a second source of truth that
    /// silently overrides the documented one.
    ///
    /// Sets a process-global for the duration, so it is deliberately the only test here that
    /// touches the environment. The one other `config_dir` caller above asserts on a suffix both
    /// branches share, so it stays correct whichever way it interleaves with this.
    #[test]
    fn xdg_config_home_still_wins_over_the_dev_config_directory() {
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/oblisk-config-dir-test") };
        let resolved = config_dir().unwrap();
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-config-dir-test/oblisk"));
    }
}
