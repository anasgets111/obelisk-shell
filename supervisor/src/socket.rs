//! Supervisor-side Unix control-socket listener.
//!
//! Binds at `$XDG_RUNTIME_DIR/oblisk-shell.sock`, not `/tmp` -- world-writable and unsuitable
//! for a socket that will carry secure textfield submissions (ADR-0005). Accepts more than
//! one live connection at once: during a generation swap, Generation `N` and Candidate `N+1`
//! are both connected simultaneously, each registered by `generation_id`.
//!
//! Deliberately deferred (docs/adr/0020): the command-dispatch routing table
//! (`oblisk-idl-api-specs.md` § 3.2's ~30 write commands). Inbound frames are decoded as
//! `shared::RendererFrame` (docs/adr/0024) and forwarded to the caller as-is.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use shared::framing::{self, FramingError};
use shared::{ConnectionHandshake, RendererFrame, SupervisorFrame};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, UnboundedSender};

/// One frame received from a connection, decoded and tagged with the generation that sent it.
#[derive(Debug)]
pub struct InboundFrame {
    pub generation_id: u32,
    pub frame: RendererFrame,
}

/// A registry entry paired with a monotonic token identifying which connection registered it.
/// Needed because two connections can legitimately claim the same `generation_id` in sequence
/// (a reconnect, or duplicate `OBLISK_GENERATION_ID=0` defaults, docs/adr/0020 item 5):
/// without the token, the old connection's cleanup would unregister the new one's live entry.
struct Entry {
    token: u64,
    tx: UnboundedSender<Vec<u8>>,
}

/// Live connections, keyed by `generation_id`, so a later caller can push a frame to a
/// specific generation instead of broadcasting to whichever peer happens to be connected.
#[derive(Clone, Default)]
pub struct GenerationRegistry {
    connections: Arc<Mutex<HashMap<u32, Entry>>>,
    next_token: Arc<AtomicU64>,
}

/// Why [`GenerationRegistry::send_frame`] failed to deliver a frame.
#[derive(Debug)]
pub enum SendFrameError {
    /// `serde_json::to_vec` failed on the frame itself.
    Serialize(serde_json::Error),
    /// No connection is currently registered for the target generation.
    NoConnection { generation_id: u32 },
}

impl std::fmt::Display for SendFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendFrameError::Serialize(err) => write!(f, "failed to serialize frame: {err}"),
            SendFrameError::NoConnection { generation_id } => write!(f, "no connection registered for generation {generation_id}"),
        }
    }
}

impl std::error::Error for SendFrameError {}

impl GenerationRegistry {
    /// Queues `payload` for delivery to `generation_id`'s connection. Returns `false` if no
    /// connection is registered for that generation. The one place raw bytes cross into a
    /// connection's outbound channel (docs/adr/0022); [`Self::send_frame`] builds on this.
    pub fn send_to(&self, generation_id: u32, payload: Vec<u8>) -> bool {
        let connections = self.connections.lock().unwrap();
        match connections.get(&generation_id) {
            Some(entry) => entry.tx.send(payload).is_ok(),
            None => false,
        }
    }

    /// Encodes and sends `frame` to `generation_id`'s connection. The one place every
    /// `SupervisorFrame` send goes through.
    pub fn send_frame(&self, generation_id: u32, frame: &SupervisorFrame) -> Result<(), SendFrameError> {
        let payload = serde_json::to_vec(frame).map_err(SendFrameError::Serialize)?;
        if self.send_to(generation_id, payload) {
            Ok(())
        } else {
            Err(SendFrameError::NoConnection { generation_id })
        }
    }

    /// Registers `tx` for `generation_id`, replacing any prior connection registered under the
    /// same id, and returns a token that must be passed back to [`Self::unregister`] so only
    /// the connection that's still current gets removed.
    pub(crate) fn register(&self, generation_id: u32, tx: UnboundedSender<Vec<u8>>) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.connections.lock().unwrap().insert(generation_id, Entry { token, tx });
        token
    }

    /// Removes `generation_id`'s entry only if it's still the one registered under `token` --
    /// a superseded connection's cleanup no-ops instead of evicting the newer, live entry.
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

/// Binds the listener at `path`, first removing a stale socket file left behind by an
/// unclean prior shutdown (a crash, `SIGKILL`) -- `UnixListener::bind` fails with `AddrInUse`
/// on an existing path otherwise. `main.rs`'s clean-shutdown path unlinks `path` itself; this
/// is defense-in-depth for the unclean case.
fn bind(path: &Path) -> Result<UnixListener, io::Error> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Binds the control socket at `path` and spawns the accept loop as a background task. Returns
/// the [`GenerationRegistry`], a channel receiving every inbound frame tagged with its
/// sender's generation, and a channel reporting each `generation_id` the instant its
/// connection finishes registering -- so `main.rs` can replay a capability's already-known
/// `StateSnapshot`s the moment a generation connects, rather than dropping a push mid-hydration.
pub fn spawn_listener(path: &Path) -> Result<(GenerationRegistry, mpsc::UnboundedReceiver<InboundFrame>, mpsc::UnboundedReceiver<u32>), io::Error> {
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
                // ponytail: no accept-loop restart policy exists yet -- there's no supervisor-
                // level process-restart primitive in this codebase to recover into. A fatal
                // accept error just stops the loop; the next phase to touch this should decide
                // whether that needs to crash the whole Supervisor instead of going silent.
                Err(err) => {
                    eprintln!("control-socket accept failed, listener stopped: {err}");
                    break;
                }
            }
        }
    });

    Ok((registry, inbound_rx, connected_rx))
}

/// Reads the connection's handshake, registers it, then loops decoding inbound frames as
/// `RendererFrame` and forwarding them, while a second task drains anything queued for this
/// generation via [`GenerationRegistry::send_to`] out over the write half.
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
    let token = registry.register(generation_id, outbound_tx);
    // Best-effort: a dropped receiver (mid-shutdown) just means no snapshot replay is needed.
    let _ = connected_tx.send(generation_id);

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
                // A malformed frame doesn't kill the connection -- only a transport failure does.
                eprintln!("control-socket frame from generation {generation_id} failed to decode as RendererFrame: {err}");
            }
            Err(_) => break,
        }
    }

    registry.unregister(generation_id, token);
    writer.abort();
    Ok(())
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

        // The first connection's cleanup runs after it's already been superseded: it must not
        // evict the second, still-live connection's entry.
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
