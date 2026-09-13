//! Real [`reload::CandidateLink`] over the control socket (ADR-0019 item 1/6), driven by
//! `main.rs`'s `reload::run_swap` handshake.
//!
//! Borrows shared `inbound_frames` for one handshake rather than a per-candidate channel
//! (ADR-0025). Irrelevant frames are logged and dropped, not routed elsewhere.

use std::time::Duration;

use shared::{ActivateDraw, ReadySignal, RendererFrame, StateSnapshot, SupervisorFrame};
use tokio::sync::mpsc;

use crate::reload::CandidateLink;
use crate::socket::{GenerationRegistry, InboundFrame, SendFrameError};

/// [`SocketCandidateLink`] failure.
#[derive(Debug)]
pub enum SocketLinkError {
    /// [`GenerationRegistry::send_frame`] failed while serializing or finding the candidate
    /// connection.
    Send(SendFrameError),
    /// Shared inbound channel closed while waiting; the listener is gone, fatal to the process.
    ConnectionClosed,
}

impl std::fmt::Display for SocketLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketLinkError::Send(err) => write!(f, "{err}"),
            SocketLinkError::ConnectionClosed => {
                write!(f, "the inbound control-socket channel closed while waiting for a response")
            }
        }
    }
}

impl std::error::Error for SocketLinkError {}

/// `push_state_snapshot`/`send_activate_draw` use [`GenerationRegistry::send_frame`]; receive
/// methods filter `inbound` and set aside every frame not tagged for this candidate.
pub struct SocketCandidateLink<'a> {
    pub registry: GenerationRegistry,
    pub candidate_generation_id: u32,
    /// Borrowed for one in-flight handshake; see the module comment.
    pub inbound: &'a mut mpsc::Receiver<InboundFrame>,
    /// Frames this handshake consumed from the shared channel without being their reader, in
    /// arrival order, for the caller to hand back to the main loop (ADR-0156).
    ///
    /// ponytail: unbounded, and moving a frame here frees the slot `MAX_INBOUND_FRAMES` was
    /// holding it in, so a flood over the handshake window grows this without a cap of its own.
    /// The window is the `ready_timeout` in `SWAP_TIMINGS` (2s) and the flood would have to come
    /// from a Renderer this Supervisor spawned. Upgrade to a cap that drops the oldest and says
    /// so, if one ever fills.
    pub deferred: Vec<InboundFrame>,
}

/// A frame only [`SocketCandidateLink`] ever reads, so one arriving out of turn is stale or a
/// wire-protocol desync rather than work the main loop still owes someone. Everything else is
/// deferred instead of dropped.
fn is_handshake_frame(frame: &RendererFrame) -> bool {
    matches!(frame, RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_))
}

impl<'a> SocketCandidateLink<'a> {
    pub fn new(
        registry: GenerationRegistry,
        candidate_generation_id: u32,
        inbound: &'a mut mpsc::Receiver<InboundFrame>,
    ) -> Self {
        SocketCandidateLink { registry, candidate_generation_id, inbound, deferred: Vec::new() }
    }

    /// Receives until the candidate's frame matches `extract`. Returns `ConnectionClosed` if the
    /// channel ends. Box rejected frames because clippy's `result_large_err` flags an unboxed
    /// `RendererFrame` error.
    async fn recv_matching<T>(
        &mut self,
        what: &str,
        mut extract: impl FnMut(RendererFrame) -> Result<T, Box<RendererFrame>>,
    ) -> Result<T, SocketLinkError> {
        loop {
            let InboundFrame { generation_id, frame } =
                self.inbound.recv().await.ok_or(SocketLinkError::ConnectionClosed)?;
            if generation_id != self.candidate_generation_id {
                self.set_aside(what, generation_id, frame, "wrong generation");
                continue;
            }
            match extract(frame) {
                Ok(value) => return Ok(value),
                Err(frame) => self.set_aside(what, generation_id, *frame, &format!("not a {what}")),
            }
        }
    }

    /// Queues a frame this handshake is not the reader of, or drops it if nothing else reads it
    /// either.
    fn set_aside(&mut self, what: &str, generation_id: u32, frame: RendererFrame, why: &str) {
        if is_handshake_frame(&frame) {
            eprintln!(
                "SocketCandidateLink({what}): handshake frame from generation {generation_id} dropped during an \
                 in-flight swap handshake for generation {} ({why}): {frame:?}",
                self.candidate_generation_id
            );
            return;
        }
        self.deferred.push(InboundFrame { generation_id, frame });
    }
}

/// Delay between `send_frame` retries while the Candidate has no connection.
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_millis(20);

impl CandidateLink for SocketCandidateLink<'_> {
    type Error = SocketLinkError;

    fn expect_candidate_pid(&mut self, pid: Option<u32>) {
        match pid {
            Some(pid) => self.registry.expect_generation(self.candidate_generation_id, pid),
            None => self.registry.forget_generation(self.candidate_generation_id),
        }
    }

    /// Retries `NoConnection`: the just-spawned Candidate needs real process/Wayland/EGL/Lua
    /// startup and socket-connect time. The loop is unbounded alone but `drive_handshake` wraps it
    /// in `timeout(ready_timeout, ..)`. Observed live; tests register fakes instantly.
    ///
    /// Sends one `StateSnapshot` per snapshot (ADR-0029), not just the first; retry matters only
    /// for the first frame.
    async fn push_state_snapshot(&mut self, snapshots: &[StateSnapshot]) -> Result<(), Self::Error> {
        for snapshot in snapshots {
            let frame = SupervisorFrame::StateSnapshot(snapshot.clone());
            loop {
                match self.registry.send_frame(self.candidate_generation_id, &frame) {
                    Ok(()) => break,
                    Err(crate::socket::SendFrameError::NoConnection { .. }) => {
                        tokio::time::sleep(CONNECTION_RETRY_INTERVAL).await;
                    }
                    Err(err) => return Err(SocketLinkError::Send(err)),
                }
            }
        }
        Ok(())
    }

    async fn recv_ready_signal(&mut self) -> Result<Vec<String>, Self::Error> {
        self.recv_matching("ReadySignal", |frame| match frame {
            RendererFrame::ReadySignal(ReadySignal { surfaces }) => Ok(surfaces),
            other => Err(Box::new(other)),
        })
        .await
    }

    async fn send_activate_draw(&mut self, nonce: u64) -> Result<(), Self::Error> {
        self.registry
            .send_frame(self.candidate_generation_id, &SupervisorFrame::ActivateDraw(ActivateDraw { nonce }))
            .map_err(SocketLinkError::Send)
    }

    async fn recv_presentation_evidence(&mut self, nonce: u64) -> Result<String, Self::Error> {
        self.recv_matching("PresentationEvidence", |frame| match frame {
            RendererFrame::PresentationEvidence(evidence) if evidence.nonce == nonce => Ok(evidence.surface_id),
            // A mismatched nonce is stale from an aborted attempt, not protocol desync; skip it.
            other => Err(Box::new(other)),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use shared::{CommandEnvelope, CommandParams, PresentationEvidence};

    use super::*;
    use crate::socket::InboundFrame;

    fn snapshot() -> StateSnapshot {
        StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({}) }
    }

    fn command_frame() -> RendererFrame {
        RendererFrame::Command(CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "ExecuteCommand".to_string(),
            params: CommandParams {
                generation_id: 9,
                capability: "audio".to_string(),
                action: "set_volume".to_string(),
                arguments: vec![],
                expected_revision: 1,
            },
            id: 1,
        })
    }

    /// Registry with a fake connection and receiver for asserting `send_frame` output.
    fn registry_with_connection(generation_id: u32) -> (GenerationRegistry, mpsc::Receiver<Vec<u8>>) {
        let registry = GenerationRegistry::default();
        let (tx, rx) = mpsc::channel(16);
        registry.register(generation_id, tx, std::sync::Arc::new(tokio::sync::Notify::new()));
        (registry, rx)
    }

    #[tokio::test]
    async fn push_state_snapshot_sends_a_state_snapshot_frame_to_the_candidate_generation() {
        let (registry, mut rx) = registry_with_connection(9);
        let (_inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        link.push_state_snapshot(&[snapshot()]).await.expect("send must succeed");

        let payload = rx.recv().await.expect("frame must have been queued");
        let frame: SupervisorFrame = serde_json::from_slice(&payload).unwrap();
        assert_eq!(frame, SupervisorFrame::StateSnapshot(snapshot()));
    }

    /// Regression from a real spawned Candidate: the old method failed immediately with
    /// `NoConnection` before the Candidate registered its connection.
    #[tokio::test]
    async fn push_state_snapshot_retries_until_the_candidate_connection_registers() {
        let registry = GenerationRegistry::default();
        let (_inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry.clone(), 9, &mut inbound_rx);

        let late_registry = registry.clone();
        let (tx, mut rx) = mpsc::channel(16);
        tokio::spawn(async move {
            tokio::time::sleep(CONNECTION_RETRY_INTERVAL * 3).await;
            late_registry.register(9, tx, std::sync::Arc::new(tokio::sync::Notify::new()));
        });

        tokio::time::timeout(Duration::from_secs(2), link.push_state_snapshot(&[snapshot()]))
            .await
            .expect("must not hang forever waiting for the connection")
            .expect("must eventually succeed once the connection registers");

        let payload = rx.recv().await.expect("frame must have been queued once the connection existed");
        let frame: SupervisorFrame = serde_json::from_slice(&payload).unwrap();
        assert_eq!(frame, SupervisorFrame::StateSnapshot(snapshot()));
    }

    #[tokio::test]
    async fn send_activate_draw_sends_an_activate_draw_frame_with_the_given_nonce() {
        let (registry, mut rx) = registry_with_connection(9);
        let (_inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        link.send_activate_draw(42).await.expect("send must succeed");

        let payload = rx.recv().await.expect("frame must have been queued");
        let frame: SupervisorFrame = serde_json::from_slice(&payload).unwrap();
        assert_eq!(frame, SupervisorFrame::ActivateDraw(shared::ActivateDraw { nonce: 42 }));
    }

    #[tokio::test]
    async fn recv_ready_signal_returns_the_surfaces_from_the_matching_generation() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }),
            })
            .await
            .unwrap();

        let surfaces = link.recv_ready_signal().await.expect("ready signal must resolve");
        assert_eq!(surfaces, vec!["main_bar".to_string()]);
    }

    #[tokio::test]
    async fn recv_ready_signal_skips_an_irrelevant_frame_arriving_before_the_relevant_one() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        // Wrong generation, still authoritative, reporting something unrelated.
        inbound_tx
            .send(InboundFrame {
                generation_id: 0,
                frame: RendererFrame::ReevaluateReport(shared::ReevaluateReport::Unchanged { sequence: 1 }),
            })
            .await
            .unwrap();
        // Right generation, wrong frame type.
        inbound_tx.send(InboundFrame { generation_id: 9, frame: command_frame() }).await.unwrap();
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["overlay_canvas".to_string()] }),
            })
            .await
            .unwrap();

        let surfaces =
            link.recv_ready_signal().await.expect("ready signal must resolve despite irrelevant frames first");
        assert_eq!(surfaces, vec!["overlay_canvas".to_string()]);
    }

    #[tokio::test]
    async fn recv_presentation_evidence_skips_a_mismatched_nonce_then_matches() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::PresentationEvidence(PresentationEvidence {
                    nonce: 1,
                    surface_id: "stale".to_string(),
                }),
            })
            .await
            .unwrap();
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::PresentationEvidence(PresentationEvidence {
                    nonce: 2,
                    surface_id: "main_bar".to_string(),
                }),
            })
            .await
            .unwrap();

        let surface_id = link.recv_presentation_evidence(2).await.expect("evidence for nonce 2 must resolve");
        assert_eq!(surface_id, "main_bar");
    }

    /// The lockout of 2026-09-07 (ADR-0156). A Candidate evaluates `shell.lua` before it signals
    /// ready, so every `obelisk.<capability>` the config reads queues a `StartCapability` that
    /// reaches this link ahead of the `ReadySignal` it is waiting for. Dropping those left the
    /// Supervisor with no `lock` controller behind a lock screen that still took keystrokes.
    #[tokio::test]
    async fn a_start_capability_sent_before_the_ready_signal_is_kept_for_the_main_loop() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        for capability in ["lock", "audio"] {
            inbound_tx
                .send(InboundFrame {
                    generation_id: 9,
                    frame: RendererFrame::StartCapability { capability: capability.to_string() },
                })
                .await
                .unwrap();
        }
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }),
            })
            .await
            .unwrap();

        link.recv_ready_signal().await.expect("the ready signal still resolves past the starts");

        let started: Vec<String> = link
            .deferred
            .iter()
            .filter_map(|held| match &held.frame {
                RendererFrame::StartCapability { capability } => Some(capability.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started, vec!["lock".to_string(), "audio".to_string()], "both, in the order they arrived");
    }

    /// The authoritative generation keeps running during a swap, and what it sends is work this
    /// Supervisor still owes it, not handshake traffic for a candidate it knows nothing about.
    #[tokio::test]
    async fn a_frame_from_another_generation_is_kept_rather_than_dropped_for_being_the_wrong_one() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        inbound_tx.send(InboundFrame { generation_id: 8, frame: command_frame() }).await.unwrap();
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }),
            })
            .await
            .unwrap();

        link.recv_ready_signal().await.expect("the ready signal still resolves");

        assert_eq!(link.deferred.len(), 1, "the other generation's command is held: {:?}", link.deferred);
        assert_eq!(link.deferred[0].generation_id, 8, "and it is handed back tagged with its own generation");
    }

    /// Only this link reads a `ReadySignal` or `PresentationEvidence`, so one arriving out of turn
    /// has no second reader to be handed to; holding it would send the main loop a frame whose only
    /// handler logs it as a desync.
    #[tokio::test]
    async fn a_handshake_frame_out_of_turn_is_still_dropped_because_nothing_else_reads_one() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        inbound_tx
            .send(InboundFrame {
                generation_id: 8,
                frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["stale".to_string()] }),
            })
            .await
            .unwrap();
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::PresentationEvidence(PresentationEvidence {
                    nonce: 1,
                    surface_id: "stale".to_string(),
                }),
            })
            .await
            .unwrap();
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }),
            })
            .await
            .unwrap();

        link.recv_ready_signal().await.expect("the ready signal must resolve past both");

        assert!(link.deferred.is_empty(), "neither is owed to anyone: {:?}", link.deferred);
    }

    #[tokio::test]
    async fn recv_ready_signal_reports_connection_closed_when_the_channel_ends() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(16);
        drop(inbound_tx);
        let mut link = SocketCandidateLink::new(registry, 9, &mut inbound_rx);

        let err = link.recv_ready_signal().await.expect_err("a closed channel must not resolve Ok");
        assert!(matches!(err, SocketLinkError::ConnectionClosed));
    }
}
