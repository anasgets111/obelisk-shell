//! Supervisor-side Unix control-socket listener.
//!
//! Binds at `$XDG_RUNTIME_DIR/obelisk-shell.sock`, not world-writable `/tmp`, because it carries
//! secure textfield submissions (ADR-0005). Accepts simultaneous connections during a swap, with
//! Generation `N` and Candidate `N+1` registered by `generation_id`.
//!
//! Command-dispatch routing remains deferred (ADR-0020), including `lua-api.md`
//! § 3.2's ~30 write commands.
//! Decode inbound frames as `shared::RendererFrame` (ADR-0024) and forward them unchanged.
//!
//! A connection does not get to say which generation it is. The handshake's `generation_id` is a
//! claim, checked against the pid Supervisor recorded when it spawned that generation
//! ([`GenerationRegistry::expect_generation`]). `$XDG_RUNTIME_DIR` is `0700`, so only this user can
//! reach the socket at all and no privilege boundary is being crossed here -- but any process of
//! this user could previously claim generation 0 and *displace* the live Renderer from the
//! registry, taking its capability pushes and its lock-related frames with it. The pid check is
//! what makes the claim mean something; an in-process token would not, since the same user can
//! read `/proc/<pid>/environ`.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::framing::{self, FramingError};
use shared::{ConnectionHandshake, RendererFrame, SupervisorFrame};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, UnboundedSender};

/// How long a peer has to send its handshake before the connection is dropped.
///
/// Without a deadline a peer could connect and send nothing, holding a task and its buffers for as
/// long as it liked; a few hundred of those is a memory-exhaustion primitive that costs the peer
/// nothing. A Renderer writes its handshake as its first act after `connect`, so seconds are
/// generous even on a loaded machine.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a claim rechecks for its generation's expectation while it waits; see
/// [`GenerationRegistry::await_claim`]. Short, because this only ever runs during the few
/// milliseconds around a spawn.
const CLAIM_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Connections handled at once, across Renderers and control clients.
///
/// A swap has two Renderers live (§ 14.2) and `obelisk set` is one short-lived client at a time, so
/// the working set is single digits. This is sized to leave that room untouched while refusing the
/// unbounded accept loop that preceded it: past this, `accept` still runs -- the listener must not
/// wedge -- but the new connection is closed immediately.
const MAX_CONNECTIONS: usize = 64;

/// Decoded frames queued from all peers toward `main`'s loop.
///
/// Bounded with backpressure rather than a drop policy: `main` reads these in protocol order, and
/// PBA's evidence, reload reports and lock reports are each load-bearing (§ 14.2, ADR-0025), so a
/// dropped frame is a stalled handshake rather than a lost log line. A full queue instead parks
/// the one connection task that is producing faster than `main` consumes, which is the peer that
/// should be waiting.
const MAX_INBOUND_FRAMES: usize = 1024;

/// Frames queued for one peer before it is treated as wedged.
///
/// The outbound channel was unbounded, so a Renderer that stopped reading -- blocked on the GPU,
/// stopped in a debugger, or simply slower than a capability storm -- grew Supervisor's heap
/// without limit. Snapshot pushes coalesce at the Renderer, so a peer this far behind is not going
/// to catch up by being given more room. `send_to` reports the drop to its caller, which already
/// handles "this generation did not take the frame".
const MAX_OUTBOUND_FRAMES: usize = 1024;

/// Decoded frame tagged with its sending generation.
#[derive(Debug)]
pub struct InboundFrame {
    pub generation_id: u32,
    pub frame: RendererFrame,
}

/// Registry entry plus monotonic token identifying its connection. Two connections may claim one
/// `generation_id` in sequence (reconnect or duplicate `OBELISK_GENERATION_ID=0`, ADR-0020); the
/// token stops old cleanup from unregistering the newer entry.
struct Entry {
    token: u64,
    tx: mpsc::Sender<Vec<u8>>,
    /// Tears the connection down from outside its own task. Dropping the `Entry` alone does not:
    /// the writer would keep draining its queue into a peer that is not reading, and block there
    /// holding the socket open. `notify_one` rather than `notify_waiters` so the signal is kept if
    /// the task is not parked on it yet.
    hangup: Arc<tokio::sync::Notify>,
}

/// Live connections by `generation_id`, enabling targeted sends instead of broadcast, plus the pid
/// Supervisor expects each generation to connect from.
#[derive(Clone, Default)]
pub struct GenerationRegistry {
    connections: Arc<Mutex<HashMap<u32, Entry>>>,
    next_token: Arc<AtomicU64>,
    /// `generation_id` to the pid of the Renderer Supervisor spawned for it. Written at spawn,
    /// before the child can have connected, and read once per handshake.
    expected_pids: Arc<Mutex<HashMap<u32, u32>>>,
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
        let mut wedged = None;
        let connections = self.connections.lock().unwrap();
        let sent = match connections.get(&generation_id) {
            // `try_send` rather than `send`: this is called from synchronous code all over
            // Supervisor, and a full queue means the peer is wedged, not that the caller should
            // wait for it. A refusal reads the same as no connection, which every caller already
            // handles.
            Some(entry) => match entry.tx.try_send(payload) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Dropping this one frame and carrying on would be the wrong recovery: the
                    // frames that matter here are lock and reload traffic, and a Renderer that
                    // silently misses one is a locked session with no lock screen. Ending the
                    // connection instead puts it through the departure path Supervisor already
                    // has, which respawns the generation and replays every snapshot.
                    eprintln!(
                        "generation {generation_id} has not read {MAX_OUTBOUND_FRAMES} queued frames; treating it as \
                         wedged and closing its connection so it is respawned rather than left missing frames"
                    );
                    wedged = Some(Arc::clone(&entry.hangup));
                    false
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            },
            None => false,
        };
        drop(connections);
        if let Some(hangup) = wedged {
            self.connections.lock().unwrap().remove(&generation_id);
            // Removing the entry only stops new frames queueing; the connection task has to be told
            // to stop, or its writer sits blocked writing to a peer that is not reading. Closing
            // the socket makes the Renderer's own loop see its Supervisor go away, and it exits
            // (`renderer/src/wayland/mod.rs`, `EXIT_SUPERVISOR_GONE`), which is the departure this
            // Supervisor already knows how to respawn from.
            hangup.notify_one();
        }
        sent
    }

    /// Encodes and sends `frame` to `generation_id`; every `SupervisorFrame` send uses this.
    pub fn send_frame(&self, generation_id: u32, frame: &SupervisorFrame) -> Result<(), SendFrameError> {
        let payload = serde_json::to_vec(frame).map_err(SendFrameError::Serialize)?;
        if self.send_to(generation_id, payload) { Ok(()) } else { Err(SendFrameError::NoConnection { generation_id }) }
    }

    /// Records the pid Supervisor spawned for `generation_id`, which is the only pid allowed to
    /// claim it. Called immediately after the spawn returns, so it is always in place before the
    /// child has had time to start, let alone connect.
    ///
    /// Replaces any previous entry: generation ids are handed out monotonically, and a reused id
    /// would mean a new process is the rightful holder anyway.
    pub fn expect_generation(&self, generation_id: u32, pid: u32) {
        self.expected_pids.lock().unwrap().insert(generation_id, pid);
    }

    /// Drops a generation's expectation once its Renderer is gone, so a dead generation's id
    /// cannot be claimed by whatever the kernel gives that pid to next.
    pub fn forget_generation(&self, generation_id: u32) {
        self.expected_pids.lock().unwrap().remove(&generation_id);
    }

    /// Whether `pid` may claim `generation_id`, or `None` if no expectation is recorded yet.
    ///
    /// The three states matter: recorded-and-matching admits, recorded-and-different refuses, and
    /// not-yet-recorded is neither. See [`Self::await_claim`].
    fn may_claim(&self, generation_id: u32, pid: u32) -> Option<bool> {
        self.expected_pids.lock().unwrap().get(&generation_id).map(|expected| *expected == pid)
    }

    /// [`Self::may_claim`], waiting for the expectation to appear if it has not yet.
    ///
    /// Supervisor records a pid immediately after the spawn returns, but "immediately" is still
    /// after: the child is runnable from the moment `spawn` returns, so a Renderer that started
    /// unusually fast could connect first and be refused, and a refused boot Renderer is a shell
    /// that never starts. Waiting closes that window without weakening the check, because an
    /// impostor waits too and is then refused on the pid it does not have.
    async fn await_claim(&self, generation_id: u32, pid: u32) -> bool {
        let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            if let Some(verdict) = self.may_claim(generation_id, pid) {
                return verdict;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(CLAIM_POLL_INTERVAL).await;
        }
    }

    /// Registers `tx`, replacing the same id, and returns the token required by
    /// [`Self::unregister`] to remove only the current connection.
    pub(crate) fn register(
        &self,
        generation_id: u32,
        tx: mpsc::Sender<Vec<u8>>,
        hangup: Arc<tokio::sync::Notify>,
    ) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.connections.lock().unwrap().insert(generation_id, Entry { token, tx, hangup });
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
) -> Result<(GenerationRegistry, mpsc::Receiver<InboundFrame>, mpsc::UnboundedReceiver<u32>), io::Error> {
    let listener = bind(path)?;
    let registry = GenerationRegistry::default();
    let (inbound_tx, inbound_rx) = mpsc::channel(MAX_INBOUND_FRAMES);
    let (connected_tx, connected_rx) = mpsc::unbounded_channel();

    let accept_registry = registry.clone();
    // Counts connections being handled, so the accept loop can refuse rather than spawn without
    // limit. `Arc` because each connection task decrements it on the way out, however it exits.
    let live_connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    // Keep accepting past the cap and close immediately: a listener that stops
                    // accepting cannot be recovered, and leaving connections queued in the kernel
                    // backlog would make a legitimate Renderer wait behind the flood.
                    if live_connections.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
                        live_connections.fetch_sub(1, Ordering::Relaxed);
                        eprintln!(
                            "control-socket: {MAX_CONNECTIONS} connections are already open, so this one was closed \
                             without being read"
                        );
                        drop(stream);
                        continue;
                    }
                    let registry = accept_registry.clone();
                    let inbound_tx = inbound_tx.clone();
                    let connected_tx = connected_tx.clone();
                    let live = Arc::clone(&live_connections);
                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(stream, registry, inbound_tx, connected_tx).await {
                            eprintln!("control-socket connection ended: {err}");
                        }
                        live.fetch_sub(1, Ordering::Relaxed);
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
    inbound_tx: mpsc::Sender<InboundFrame>,
    connected_tx: UnboundedSender<u32>,
) -> Result<(), FramingError> {
    // Read the peer's identity before its first byte: `SO_PEERCRED` is stamped by the kernel at
    // connect, so nothing the connection sends afterwards can change it.
    let peer_pid = stream.peer_cred().ok().and_then(|cred| cred.pid()).map(|pid| pid as u32);
    let (mut read_half, mut write_half) = stream.into_split();

    // A peer that connects and says nothing holds this task and its buffers forever without the
    // deadline; see `HANDSHAKE_TIMEOUT`.
    let handshake: ConnectionHandshake =
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, framing::read_json_frame(&mut read_half)).await {
            Ok(handshake) => handshake?,
            Err(_) => {
                eprintln!(
                    "control-socket: a peer sent no handshake within {}s and was disconnected",
                    HANDSHAKE_TIMEOUT.as_secs()
                );
                return Ok(());
            }
        };
    let generation_id = handshake.generation_id;

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(MAX_OUTBOUND_FRAMES);
    // ADR-0112 control clients have no generation: nothing is pushed or replayed, but frames flow
    // inbound.
    let control_client = generation_id == shared::CONTROL_CLIENT_GENERATION;
    if !control_client {
        // A Renderer claim has to come from the process Supervisor spawned for that generation.
        // Refusing is a quiet disconnect: nothing this connection says afterwards is trustworthy,
        // and a detailed answer only tells a prober which generation ids are live.
        let Some(pid) = peer_pid else {
            eprintln!(
                "control-socket: refusing a claim on generation {generation_id} from a peer whose \
                 credentials could not be read"
            );
            return Ok(());
        };
        if !registry.await_claim(generation_id, pid).await {
            eprintln!(
                "control-socket: refusing pid {pid}'s claim on generation {generation_id}; \
                 that generation belongs to another process, and accepting would hand this \
                 connection its capability pushes"
            );
            return Ok(());
        }
    }
    let hangup = Arc::new(tokio::sync::Notify::new());
    let token = (!control_client).then(|| registry.register(generation_id, outbound_tx, Arc::clone(&hangup)));
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

    let read_loop = async {
        loop {
            match framing::read_json_frame::<_, RendererFrame>(&mut read_half).await {
                Ok(frame) => {
                    if let Some(refusal) = refuse_frame(control_client, generation_id, &frame) {
                        eprintln!("control-socket: dropped a frame from generation {generation_id}: {refusal}");
                        continue;
                    }
                    // Awaited, not dropped: parking this peer is the backpressure
                    // `MAX_INBOUND_FRAMES` exists for. An error means `main` is gone.
                    if inbound_tx.send(InboundFrame { generation_id, frame }).await.is_err() {
                        break;
                    }
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
    };
    tokio::pin!(read_loop);
    tokio::select! {
        () = &mut read_loop => {}
        () = hangup.notified() => {
            eprintln!("control-socket: hanging up on generation {generation_id} so it exits and can be respawned");
        }
    }

    if let Some(token) = token {
        registry.unregister(generation_id, token);
    }
    writer.abort();
    Ok(())
}

/// Why a decoded frame must not be forwarded, or `None` to forward it.
///
/// Two rules, both about a peer describing itself rather than being described:
///
/// 1. A control client (`obelisk set`/`obelisk toggle`, ADR-0112) is any process of this user and is
///    never a Renderer. It sends exactly one frame kind, so it may send exactly that one. Without
///    this it could submit a `SecureSubmit` to PAM or drive `Command`s as though it were the shell.
/// 2. A frame that names a generation must name its own. The pid check at handshake stops a peer
///    claiming another generation's *connection*; this stops it claiming another generation inside
///    a frame body, which is the same lie one layer down.
///
/// Dropping matches the existing treatment of a frame that fails to decode: the connection
/// survives, the frame does not.
fn refuse_frame(control_client: bool, generation_id: u32, frame: &RendererFrame) -> Option<String> {
    if control_client {
        return match frame {
            RendererFrame::SetState(_) => None,
            // `RendererFrame` derives `Debug` and `SecureSubmit` redacts its own secret, so this
            // cannot print a password.
            other => Some(format!("a control client may only send SetState, not {other:?}")),
        };
    }
    let claimed = match frame {
        RendererFrame::Command(envelope) => Some(envelope.params.generation_id),
        RendererFrame::SecureSubmit(submit) => Some(submit.generation_id),
        // The rest carry no generation: the socket identifies the sender.
        _ => None,
    };
    claimed
        .filter(|claimed| *claimed != generation_id)
        .map(|claimed| format!("it names generation {claimed}, but this connection is generation {generation_id}"))
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

    fn command_frame(generation_id: u32) -> RendererFrame {
        RendererFrame::Command(shared::CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "capability.invoke".to_string(),
            id: 1,
            params: shared::CommandParams {
                generation_id,
                capability: "audio".to_string(),
                action: "set_volume".to_string(),
                arguments: Vec::new(),
                expected_revision: 0,
            },
        })
    }

    #[test]
    fn a_control_client_may_send_only_the_frame_the_cli_actually_sends() {
        // Otherwise `obelisk set`'s socket is also a way to submit to PAM or drive capability
        // commands as though it were the shell.
        let set_state = RendererFrame::SetState(shared::SetState {
            name: "launcher_open".to_string(),
            write: shared::StateWrite::Toggle,
        });
        assert!(refuse_frame(true, shared::CONTROL_CLIENT_GENERATION, &set_state).is_none());

        let refusal = refuse_frame(true, shared::CONTROL_CLIENT_GENERATION, &command_frame(0))
            .expect("a control client must not be able to send Command");
        assert!(refusal.contains("only send SetState"), "{refusal}");
        assert!(refuse_frame(true, shared::CONTROL_CLIENT_GENERATION, &RendererFrame::RequestReload).is_some());
    }

    #[test]
    fn a_renderer_may_not_name_a_generation_other_than_its_own_inside_a_frame() {
        // The handshake check stops a peer claiming another generation's connection; this stops the
        // same lie one layer down, in a frame body.
        assert!(refuse_frame(false, 2, &command_frame(2)).is_none(), "its own generation is fine");
        let refusal = refuse_frame(false, 2, &command_frame(7)).expect("a mismatched generation must be refused");
        assert!(refusal.contains("generation 7"), "{refusal}");
        assert!(refusal.contains("generation 2"), "{refusal}");
    }

    #[test]
    fn frames_that_name_no_generation_are_forwarded_because_the_socket_identifies_the_sender() {
        assert!(refuse_frame(false, 3, &RendererFrame::RequestReload).is_none());
        assert!(refuse_frame(false, 3, &RendererFrame::StartCapability { capability: "audio".to_string() }).is_none());
    }

    #[test]
    fn a_generation_may_be_claimed_only_by_the_pid_supervisor_spawned_for_it() {
        let registry = GenerationRegistry::default();
        assert_eq!(registry.may_claim(0, 4242), None, "an unrecorded generation is not yet decidable");
        registry.expect_generation(0, 4242);
        assert_eq!(registry.may_claim(0, 4242), Some(true));
        assert_eq!(registry.may_claim(0, 4243), Some(false), "another process must not displace the Renderer");
        assert_eq!(registry.may_claim(1, 4242), None, "the binding is per generation, not per process");
    }

    #[test]
    fn a_reaped_generations_id_stops_being_claimable() {
        // Pids are recycled; a stale expectation would eventually match something unrelated.
        let registry = GenerationRegistry::default();
        registry.expect_generation(2, 99);
        registry.forget_generation(2);
        assert_eq!(registry.may_claim(2, 99), None);
    }

    #[tokio::test]
    async fn a_wedged_peer_is_hung_up_on_rather_than_quietly_missing_frames() {
        // Dropping one frame and carrying on would be the wrong recovery: these are lock and
        // reload frames, and a Renderer that silently misses one is a locked session with no lock
        // screen. It is hung up on instead, which makes it exit and be respawned.
        let registry = GenerationRegistry::default();
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(MAX_OUTBOUND_FRAMES);
        let hangup = std::sync::Arc::new(tokio::sync::Notify::new());
        registry.register(5, tx, std::sync::Arc::clone(&hangup));
        // `_rx` is never read, so the queue fills.
        for _ in 0..MAX_OUTBOUND_FRAMES {
            assert!(registry.send_to(5, vec![0]), "everything up to the cap is accepted");
        }

        assert!(!registry.send_to(5, vec![0]), "the frame past the cap is refused");
        assert!(!registry.is_registered(5), "and the wedged peer is unregistered");
        // `notify_one` leaves a permit, so this resolves even though nothing was parked on it when
        // the hangup fired -- which is the case the connection task depends on.
        tokio::time::timeout(std::time::Duration::from_millis(50), hangup.notified())
            .await
            .expect("the connection task must be told to close, or its writer sits blocked");
    }

    #[tokio::test]
    async fn bind_removes_a_stale_socket_file_left_by_a_prior_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obelisk-shell.sock");

        let first = UnixListener::bind(&path).unwrap();
        drop(first); // Simulate an unclean shutdown: the socket file is left on disk.
        assert!(path.exists());

        let second = bind(&path);
        assert!(second.is_ok(), "bind must clear a stale socket file, not fail with AddrInUse");
    }

    #[test]
    fn unregister_ignores_a_stale_token_from_a_superseded_connection() {
        let registry = GenerationRegistry::default();
        let (tx_a, _rx_a) = mpsc::channel(MAX_OUTBOUND_FRAMES);
        let (tx_b, _rx_b) = mpsc::channel(MAX_OUTBOUND_FRAMES);

        let token_a = registry.register(5, tx_a, std::sync::Arc::new(tokio::sync::Notify::new()));
        let token_b = registry.register(5, tx_b, std::sync::Arc::new(tokio::sync::Notify::new())); // A second connection claims the same generation.
        assert!(registry.is_registered(5));

        // Old cleanup must not evict the newer live connection.
        registry.unregister(5, token_a);
        assert!(registry.is_registered(5), "a stale unregister must not remove the live connection's entry");

        registry.unregister(5, token_b);
        assert!(!registry.is_registered(5), "unregistering with the current token must remove the entry");
    }

    /// Every test peer is the test process itself, so the generations it claims have to be bound to
    /// this pid the way Supervisor binds a Renderer's at spawn.
    fn expect_this_process(registry: &GenerationRegistry, generation_ids: &[u32]) {
        for generation_id in generation_ids {
            registry.expect_generation(*generation_id, std::process::id());
        }
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
        let path = dir.path().join("obelisk-shell.sock");
        let (registry, _inbound, _connected) = spawn_listener(&path).unwrap();
        expect_this_process(&registry, &[1, 2]);

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
        let path = dir.path().join("obelisk-shell.sock");
        let (registry, _inbound, mut connected) = spawn_listener(&path).unwrap();
        expect_this_process(&registry, &[7]);

        let mut client = UnixStream::connect(&path).await.unwrap();
        framing::write_json_frame(&mut client, &ConnectionHandshake { generation_id: 7 }).await.unwrap();

        assert_eq!(connected.recv().await, Some(7), "the connected channel must report the handshake's generation_id");
    }

    #[tokio::test]
    async fn spawn_listener_forwards_a_decoded_command_envelope_tagged_with_its_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obelisk-shell.sock");
        let (registry, mut inbound, _connected) = spawn_listener(&path).unwrap();
        expect_this_process(&registry, &[5]);

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
        let path = dir.path().join("obelisk-shell.sock");
        let (registry, mut inbound, _connected) = spawn_listener(&path).unwrap();
        expect_this_process(&registry, &[5]);

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
