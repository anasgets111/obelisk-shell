//! Supervisor-side Unix control-socket listener.
//!
//! Binds at `$XDG_RUNTIME_DIR/oblisk-shell.sock`, not world-writable `/tmp`, because it carries
//! secure textfield submissions (ADR-0005). Accepts simultaneous connections during a swap, with
//! Generation `N` and Candidate `N+1` registered by `generation_id`.
//!
//! Command-dispatch routing remains deferred (ADR-0020), including `oblisk-idl-api-specs.md`
//! § 3.2's ~30 write commands.
//! Decode inbound frames as `shared::RendererFrame` (ADR-0024) and forward them unchanged.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use shared::framing::{self, FramingError};
use shared::{ConnectionHandshake, RendererFrame, SupervisorFrame};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, UnboundedSender};

/// Decoded frame tagged with its sending generation.
#[derive(Debug)]
pub struct InboundFrame {
    pub generation_id: u32,
    pub frame: RendererFrame,
}

/// Registry entry plus monotonic token identifying its connection. Two connections may claim one
/// `generation_id` in sequence (reconnect or duplicate `OBLISK_GENERATION_ID=0`, ADR-0020); the
/// token stops old cleanup from unregistering the newer entry.
struct Entry {
    token: u64,
    tx: UnboundedSender<Vec<u8>>,
}

/// Live connections by `generation_id`, enabling targeted sends instead of broadcast.
#[derive(Clone, Default)]
pub struct GenerationRegistry {
    connections: Arc<Mutex<HashMap<u32, Entry>>>,
    next_token: Arc<AtomicU64>,
}

/// [`GenerationRegistry::send_frame`] delivery failure.
#[derive(Debug)]
pub enum SendFrameError {
    /// `serde_json::to_vec` failed.
    Serialize(serde_json::Error),
    /// No connection is registered for the target generation.
    NoConnection { generation_id: u32 },
}

impl std::fmt::Display for SendFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendFrameError::Serialize(err) => write!(f, "failed to serialize frame: {err}"),
            SendFrameError::NoConnection { generation_id } => {
                write!(f, "no connection registered for generation {generation_id}")
            }
        }
    }
}

impl std::error::Error for SendFrameError {}

impl GenerationRegistry {
    /// Queues raw `payload` for `generation_id`; returns `false` with no registration. The only
    /// raw-byte crossing into an outbound channel (ADR-0022); [`Self::send_frame`] builds on it.
    pub fn send_to(&self, generation_id: u32, payload: Vec<u8>) -> bool {
        let connections = self.connections.lock().unwrap();
        match connections.get(&generation_id) {
            Some(entry) => entry.tx.send(payload).is_ok(),
            None => false,
        }
    }

    /// Encodes and sends `frame` to `generation_id`; every `SupervisorFrame` send uses this.
    pub fn send_frame(&self, generation_id: u32, frame: &SupervisorFrame) -> Result<(), SendFrameError> {
        let payload = serde_json::to_vec(frame).map_err(SendFrameError::Serialize)?;
        if self.send_to(generation_id, payload) { Ok(()) } else { Err(SendFrameError::NoConnection { generation_id }) }
    }

    /// Registers `tx`, replacing the same id, and returns the token required by
    /// [`Self::unregister`] to remove only the current connection.
    pub(crate) fn register(&self, generation_id: u32, tx: UnboundedSender<Vec<u8>>) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.connections.lock().unwrap().insert(generation_id, Entry { token, tx });
        token
    }

    /// Removes only if `token` is still current; superseded cleanup cannot evict the live entry.
    fn unregister(&self, generation_id: u32, token: u64) {
        let mut connections = self.connections.lock().unwrap();
        if connections.get(&generation_id).is_some_and(|entry| entry.token == token) {
            connections.remove(&generation_id);
        }
    }

    #[cfg(test)]
    fn is_registered(&self, generation_id: u32) -> bool {
        self.connections.lock().unwrap().contains_key(&generation_id)
    }
}

/// Removes a stale socket from crash/`SIGKILL` shutdown before binding; otherwise
/// `UnixListener::bind` returns `AddrInUse`. `main.rs` unlinks cleanly; this covers the rest.
fn bind(path: &Path) -> Result<UnixListener, io::Error> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Binds and starts the accept loop. Returns the registry, tagged inbound frames, and a channel
/// reporting registration completion so `main.rs` replays known `StateSnapshot`s immediately
/// instead of dropping a push during hydration.
pub fn spawn_listener(
    path: &Path,
) -> Result<(GenerationRegistry, mpsc::UnboundedReceiver<InboundFrame>, mpsc::UnboundedReceiver<u32>), io::Error> {
    let listener = bind(path)?;
    let registry = GenerationRegistry::default();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let (connected_tx, connected_rx) = mpsc::unbounded_channel();

    let accept_registry = registry.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let registry = accept_registry.clone();
                    let inbound_tx = inbound_tx.clone();
                    let connected_tx = connected_tx.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(stream, registry, inbound_tx, connected_tx).await {
                            eprintln!("control-socket connection ended: {err}");
                        }
                    });
                }
                // ponytail: no Supervisor-level process-restart primitive exists in this codebase
                // to recover into. Fatal accept errors stop the loop; the next change should
                // decide whether they must crash Supervisor.
                Err(err) => {
                    eprintln!("control-socket accept failed, listener stopped: {err}");
                    break;
                }
            }
        }
    });

    Ok((registry, inbound_rx, connected_rx))
}

/// Reads the handshake, registers the connection, forwards decoded `RendererFrame`s, and lets a
/// second task drain [`GenerationRegistry::send_to`] to the write half.
async fn handle_connection(
    stream: UnixStream,
    registry: GenerationRegistry,
    inbound_tx: UnboundedSender<InboundFrame>,
    connected_tx: UnboundedSender<u32>,
) -> Result<(), FramingError> {
    let (mut read_half, mut write_half) = stream.into_split();

    let handshake: ConnectionHandshake = framing::read_json_frame(&mut read_half).await?;
    let generation_id = handshake.generation_id;

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // ADR-0112 control clients have no generation: nothing is pushed or replayed, but frames flow
    // inbound.
    let control_client = generation_id == shared::CONTROL_CLIENT_GENERATION;
    let token = (!control_client).then(|| registry.register(generation_id, outbound_tx));
    // Best effort; a dropped receiver during shutdown needs no replay.
    if !control_client {
        let _ = connected_tx.send(generation_id);
    }

    let writer = tokio::spawn(async move {
        while let Some(payload) = outbound_rx.recv().await {
            if framing::write_frame(&mut write_half, &payload).await.is_err() {
                break;
            }
        }
    });

    loop {
        match framing::read_json_frame::<_, RendererFrame>(&mut read_half).await {
            Ok(frame) => {
                let _ = inbound_tx.send(InboundFrame { generation_id, frame });
            }
            Err(FramingError::Decode(err)) => {
                // Malformed frames do not kill the connection; transport failure does.
                eprintln!(
                    "control-socket frame from generation {generation_id} failed to decode as RendererFrame: {err}"
                );
            }
            Err(_) => break,
        }
    }

    if let Some(token) = token {
        registry.unregister(generation_id, token);
    }
    writer.abort();
    Ok(())
}

/// Sends `frame`, logging rather than propagating failure. `NoConnection` is expected before boot
/// Renderer registration; `connected.recv()` replays `last_snapshots`, so no early push is lost.
pub(crate) fn send_frame_logged(registry: &GenerationRegistry, generation_id: u32, frame: &SupervisorFrame) {
    let Err(err) = registry.send_frame(generation_id, frame) else {
        return;
    };
    // This routine failure once logged thirteen `failed to push` lines with full payloads before
    // the Renderer drew a frame, drowning out meaningful failures.
    if let (SupervisorFrame::StateSnapshot(snapshot), SendFrameError::NoConnection { .. }) = (frame, &err) {
        eprintln!(
            "generation {generation_id} has not connected yet, so {} revision {} waits for the replay",
            snapshot.capability, snapshot.revision
        );
        return;
    }
    eprintln!("failed to push {frame:?} to generation {generation_id}: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_removes_a_stale_socket_file_left_by_a_prior_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");

        let first = UnixListener::bind(&path).unwrap();
        drop(first); // Simulate an unclean shutdown: the socket file is left on disk.
        assert!(path.exists());

        let second = bind(&path);
        assert!(second.is_ok(), "bind must clear a stale socket file, not fail with AddrInUse");
    }

    #[test]
    fn unregister_ignores_a_stale_token_from_a_superseded_connection() {
        let registry = GenerationRegistry::default();
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();

        let token_a = registry.register(5, tx_a);
        let token_b = registry.register(5, tx_b); // A second connection claims the same generation.
        assert!(registry.is_registered(5));

        // Old cleanup must not evict the newer live connection.
        registry.unregister(5, token_a);
        assert!(registry.is_registered(5), "a stale unregister must not remove the live connection's entry");

        registry.unregister(5, token_b);
        assert!(!registry.is_registered(5), "unregistering with the current token must remove the entry");
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !condition() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition did not become true in time");
    }

    #[tokio::test]
    async fn spawn_listener_registers_two_simultaneous_connections_by_generation_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");
        let (registry, _inbound, _connected) = spawn_listener(&path).unwrap();

        let mut client_a = UnixStream::connect(&path).await.unwrap();
        framing::write_json_frame(&mut client_a, &ConnectionHandshake { generation_id: 1 }).await.unwrap();
        let mut client_b = UnixStream::connect(&path).await.unwrap();
        framing::write_json_frame(&mut client_b, &ConnectionHandshake { generation_id: 2 }).await.unwrap();

        wait_until(|| registry.is_registered(1) && registry.is_registered(2)).await;

        assert!(registry.send_to(1, b"to-one".to_vec()));
        assert!(registry.send_to(2, b"to-two".to_vec()));
        assert!(!registry.send_to(99, b"nobody".to_vec()), "no connection is registered for generation 99");

        assert_eq!(framing::read_frame(&mut client_a).await.unwrap(), b"to-one");
        assert_eq!(framing::read_frame(&mut client_b).await.unwrap(), b"to-two");
    }

    #[tokio::test]
    async fn spawn_listener_reports_a_generation_id_on_the_connected_channel_once_registered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");
        let (_registry, _inbound, mut connected) = spawn_listener(&path).unwrap();

        let mut client = UnixStream::connect(&path).await.unwrap();
        framing::write_json_frame(&mut client, &ConnectionHandshake { generation_id: 7 }).await.unwrap();

        assert_eq!(connected.recv().await, Some(7), "the connected channel must report the handshake's generation_id");
    }

    #[tokio::test]
    async fn spawn_listener_forwards_a_decoded_command_envelope_tagged_with_its_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");
        let (_registry, mut inbound, _connected) = spawn_listener(&path).unwrap();

        let mut client = UnixStream::connect(&path).await.unwrap();
        framing::write_json_frame(&mut client, &ConnectionHandshake { generation_id: 5 }).await.unwrap();

        let envelope = shared::CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "ExecuteCommand".to_string(),
            params: shared::CommandParams {
                generation_id: 5,
                capability: "audio".to_string(),
                action: "set_volume".to_string(),
                arguments: vec![serde_json::json!(0.5)],
                expected_revision: 1,
            },
            id: 1,
        };
        framing::write_json_frame(&mut client, &RendererFrame::Command(envelope)).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), inbound.recv())
            .await
            .expect("inbound command did not arrive in time")
            .expect("inbound channel closed unexpectedly");

        assert_eq!(received.generation_id, 5);
        match received.frame {
            RendererFrame::Command(envelope) => {
                assert_eq!(envelope.params.capability, "audio");
                assert_eq!(envelope.params.action, "set_volume");
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_listener_forwards_a_decoded_reevaluate_report_tagged_with_its_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");
        let (_registry, mut inbound, _connected) = spawn_listener(&path).unwrap();

        let mut client = UnixStream::connect(&path).await.unwrap();
        framing::write_json_frame(&mut client, &ConnectionHandshake { generation_id: 5 }).await.unwrap();

        let report = shared::ReevaluateReport::Unchanged { sequence: 3 };
        framing::write_json_frame(&mut client, &RendererFrame::ReevaluateReport(report.clone())).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), inbound.recv())
            .await
            .expect("inbound report did not arrive in time")
            .expect("inbound channel closed unexpectedly");

        assert_eq!(received.generation_id, 5);
        assert_eq!(received.frame, RendererFrame::ReevaluateReport(report));
    }
}
