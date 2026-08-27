//! Real [`reload::CandidateLink`] implementation over the control socket (build-steps.md
//! Phase 14, closing docs/adr/0019 item 1/6). `reload::run_pba`'s handshake previously only ran
//! against a fake in `reload.rs`'s own tests -- [`SocketCandidateLink`] is the production
//! implementation `main.rs` drives it with.
//!
//! Borrows the Supervisor's shared `inbound_frames` channel for the duration of one in-flight
//! handshake, rather than routing frames through a dedicated per-candidate channel -- see
//! `main.rs`'s "why the main loop blocks synchronously for one swap" rationale
//! (docs/adr/0025). Any frame read here that isn't relevant to this handshake (wrong
//! `generation_id`, or a frame type this link doesn't care about -- e.g. `Command` or
//! `ReevaluateReport` from the still-authoritative generation, both plausible mid-handshake) is
//! logged and dropped, not routed anywhere else.

use std::time::Duration;

use shared::{ActivateDraw, ReadySignal, RendererFrame, StateSnapshot, SupervisorFrame};
use tokio::sync::mpsc;

use crate::reload::CandidateLink;
use crate::socket::{GenerationRegistry, InboundFrame, SendFrameError};

/// What a [`SocketCandidateLink`] call can fail with.
#[derive(Debug)]
pub enum SocketLinkError {
    /// [`GenerationRegistry::send_frame`] failed (serialize error, or no connection registered
    /// for the candidate generation).
    Send(SendFrameError),
    /// The shared inbound-frame channel closed while this link was waiting on it -- the
    /// Supervisor's control-socket listener task is gone, which is fatal to the whole process,
    /// not just this handshake.
    ConnectionClosed,
}

impl std::fmt::Display for SocketLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketLinkError::Send(err) => write!(f, "{err}"),
            SocketLinkError::ConnectionClosed => write!(f, "the inbound control-socket channel closed while waiting for a response"),
        }
    }
}

impl std::error::Error for SocketLinkError {}

/// The real [`CandidateLink`] implementation: `push_state_snapshot`/`send_activate_draw` go
/// through [`GenerationRegistry::send_frame`]; `recv_ready_signal`/`recv_presentation_evidence`
/// loop-and-filter `inbound` for frames tagged with `candidate_generation_id`, ignoring (with a
/// log line) anything else.
pub struct SocketCandidateLink<'a> {
    pub registry: GenerationRegistry,
    pub candidate_generation_id: u32,
    /// Borrowed for the duration of one in-flight handshake -- see the module doc comment.
    pub inbound: &'a mut mpsc::UnboundedReceiver<InboundFrame>,
}

impl SocketCandidateLink<'_> {
    /// Loops `self.inbound.recv()` until a frame from `self.candidate_generation_id` matches
    /// `extract`, logging and dropping everything else (wrong generation, or a frame `extract`
    /// itself declines). `ConnectionClosed` if the channel ends first. `extract`'s `Err` carries
    /// the rejected frame boxed, not inline -- `RendererFrame` is large enough (its `Command`
    /// variant embeds a whole `CommandEnvelope`) that clippy's `result_large_err` flags an
    /// unboxed `Result<T, RendererFrame>` closure return.
    async fn recv_matching<T>(&mut self, what: &str, mut extract: impl FnMut(RendererFrame) -> Result<T, Box<RendererFrame>>) -> Result<T, SocketLinkError> {
        loop {
            let InboundFrame { generation_id, frame } = self.inbound.recv().await.ok_or(SocketLinkError::ConnectionClosed)?;
            if generation_id != self.candidate_generation_id {
                eprintln!(
                    "SocketCandidateLink({what}): frame from generation {generation_id} dropped during an in-flight swap handshake for \
                     generation {} (wrong generation): {frame:?}",
                    self.candidate_generation_id
                );
                continue;
            }
            match extract(frame) {
                Ok(value) => return Ok(value),
                Err(frame) => eprintln!(
                    "SocketCandidateLink({what}): frame from generation {generation_id} dropped during an in-flight swap handshake \
                     (not a {what}): {frame:?}"
                ),
            }
        }
    }
}

/// How long to wait between retries of a `send_frame` that failed because the Candidate hasn't
/// registered a connection yet -- see [`SocketCandidateLink::push_state_snapshot`].
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_millis(20);

impl CandidateLink for SocketCandidateLink<'_> {
    type Error = SocketLinkError;

    /// Retries on `SendFrameError::NoConnection`, rather than failing on the first attempt: the
    /// Candidate was *just* spawned by `process::spawn_group_leader` a moment before `run_pba`
    /// calls this, and real process/Wayland/EGL/Lua-VM startup plus the initial socket connect
    /// and handshake take real wall-clock time -- there is no synchronization point that
    /// guarantees the Candidate has registered with `GenerationRegistry` yet. Unbounded on its
    /// own, but safe: `drive_handshake` wraps this whole call in one `timeout(ready_timeout, ..)`.
    /// Found live (a real spawned process racing a real socket connect) rather than in review --
    /// this module's own tests, and `reload.rs`'s `FakeCandidateLink`, never modeled a
    /// not-yet-connected Candidate, since a fake registers instantly.
    ///
    /// Sends one `StateSnapshot` frame per entry in `snapshots` (docs/adr/0029: every known
    /// capability, not just audio) -- the retry-on-`NoConnection` loop only matters for the
    /// first frame in practice, since a successful send means the Candidate is registered and
    /// every later frame in the same batch will succeed immediately too.
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
        self.registry.send_frame(self.candidate_generation_id, &SupervisorFrame::ActivateDraw(ActivateDraw { nonce })).map_err(SocketLinkError::Send)
    }

    async fn recv_presentation_evidence(&mut self, nonce: u64) -> Result<String, Self::Error> {
        self.recv_matching("PresentationEvidence", |frame| match frame {
            RendererFrame::PresentationEvidence(evidence) if evidence.nonce == nonce => Ok(evidence.surface_id),
            // A mismatched nonce is a stale message from an aborted prior attempt, not a
            // protocol desync -- logged-and-skipped by `recv_matching`'s `Err` path exactly
            // like an irrelevant frame type, not treated as an error.
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

    /// A `GenerationRegistry` with one fake connection registered for `generation_id`, plus the
    /// receiver end so a test can assert what `send_frame` actually wrote.
    fn registry_with_connection(generation_id: u32) -> (GenerationRegistry, mpsc::UnboundedReceiver<Vec<u8>>) {
        let registry = GenerationRegistry::default();
        let (tx, rx) = mpsc::unbounded_channel();
        registry.register(generation_id, tx);
        (registry, rx)
    }

    #[tokio::test]
    async fn push_state_snapshot_sends_a_state_snapshot_frame_to_the_candidate_generation() {
        let (registry, mut rx) = registry_with_connection(9);
        let (_inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let mut link = SocketCandidateLink { registry, candidate_generation_id: 9, inbound: &mut inbound_rx };

        link.push_state_snapshot(&[snapshot()]).await.expect("send must succeed");

        let payload = rx.recv().await.expect("frame must have been queued");
        let frame: SupervisorFrame = serde_json::from_slice(&payload).unwrap();
        assert_eq!(frame, SupervisorFrame::StateSnapshot(snapshot()));
    }

    /// Regression test: found running Phase 14 live against a real spawned Candidate process
    /// (not a fake) for the first time -- `push_state_snapshot` used to fail immediately with
    /// `NoConnection` because it ran before the just-spawned Candidate had finished starting up
    /// and registering its connection.
    #[tokio::test]
    async fn push_state_snapshot_retries_until_the_candidate_connection_registers() {
        let registry = GenerationRegistry::default();
        let (_inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let mut link = SocketCandidateLink { registry: registry.clone(), candidate_generation_id: 9, inbound: &mut inbound_rx };

        let late_registry = registry.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(CONNECTION_RETRY_INTERVAL * 3).await;
            late_registry.register(9, tx);
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
        let (_inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let mut link = SocketCandidateLink { registry, candidate_generation_id: 9, inbound: &mut inbound_rx };

        link.send_activate_draw(42).await.expect("send must succeed");

        let payload = rx.recv().await.expect("frame must have been queued");
        let frame: SupervisorFrame = serde_json::from_slice(&payload).unwrap();
        assert_eq!(frame, SupervisorFrame::ActivateDraw(shared::ActivateDraw { nonce: 42 }));
    }

    #[tokio::test]
    async fn recv_ready_signal_returns_the_surfaces_from_the_matching_generation() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let mut link = SocketCandidateLink { registry, candidate_generation_id: 9, inbound: &mut inbound_rx };

        inbound_tx
            .send(InboundFrame { generation_id: 9, frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["main_bar".to_string()] }) })
            .unwrap();

        let surfaces = link.recv_ready_signal().await.expect("ready signal must resolve");
        assert_eq!(surfaces, vec!["main_bar".to_string()]);
    }

    #[tokio::test]
    async fn recv_ready_signal_skips_an_irrelevant_frame_arriving_before_the_relevant_one() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let mut link = SocketCandidateLink { registry, candidate_generation_id: 9, inbound: &mut inbound_rx };

        // Wrong generation (the still-authoritative one reporting something unrelated).
        inbound_tx
            .send(InboundFrame {
                generation_id: 0,
                frame: RendererFrame::ReevaluateReport(shared::ReevaluateReport::Unchanged { sequence: 1 }),
            })
            .unwrap();
        // Right generation, wrong frame type.
        inbound_tx.send(InboundFrame { generation_id: 9, frame: command_frame() }).unwrap();
        inbound_tx
            .send(InboundFrame { generation_id: 9, frame: RendererFrame::ReadySignal(ReadySignal { surfaces: vec!["overlay_canvas".to_string()] }) })
            .unwrap();

        let surfaces = link.recv_ready_signal().await.expect("ready signal must resolve despite irrelevant frames first");
        assert_eq!(surfaces, vec!["overlay_canvas".to_string()]);
    }

    #[tokio::test]
    async fn recv_presentation_evidence_skips_a_mismatched_nonce_then_matches() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let mut link = SocketCandidateLink { registry, candidate_generation_id: 9, inbound: &mut inbound_rx };

        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::PresentationEvidence(PresentationEvidence { nonce: 1, surface_id: "stale".to_string() }),
            })
            .unwrap();
        inbound_tx
            .send(InboundFrame {
                generation_id: 9,
                frame: RendererFrame::PresentationEvidence(PresentationEvidence { nonce: 2, surface_id: "main_bar".to_string() }),
            })
            .unwrap();

        let surface_id = link.recv_presentation_evidence(2).await.expect("evidence for nonce 2 must resolve");
        assert_eq!(surface_id, "main_bar");
    }

    #[tokio::test]
    async fn recv_ready_signal_reports_connection_closed_when_the_channel_ends() {
        let (registry, _rx) = registry_with_connection(9);
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        drop(inbound_tx);
        let mut link = SocketCandidateLink { registry, candidate_generation_id: 9, inbound: &mut inbound_rx };

        let err = link.recv_ready_signal().await.expect_err("a closed channel must not resolve Ok");
        assert!(matches!(err, SocketLinkError::ConnectionClosed));
    }
}
