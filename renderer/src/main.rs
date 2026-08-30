mod image;
mod layout;
mod lua;
mod socket;
mod text;
mod wayland;

/// Two OS threads, two channels (docs/adr/0039, build-steps.md Phase 18). `wayland::run` owns the
/// main thread: the Wayland dispatch loop, EGL, the Lua VM, the retained `Scene`, and the live
/// signals all live there, because `mlua::Lua` is `!Send` and the scene has to be reachable from
/// the thread that holds the GL context. `socket::spawn_client` owns one dedicated I/O thread with
/// its own current-thread tokio runtime and does nothing but framed I/O with the Supervisor's
/// control socket:
/// - inbound: every decoded `SupervisorFrame` is forwarded over a `std::sync::mpsc` channel and
///   drained by `wayland::run`'s poll loop on a bounded 15ms latency.
/// - outbound: every `RendererFrame` the Wayland thread produces is queued on a
///   `tokio::sync::mpsc` channel and written to the wire by the socket thread.
///   `UnboundedSender::send` is synchronous and non-blocking, so the Wayland thread can call it
///   directly without bridging.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (inbound_tx, inbound_rx) = std::sync::mpsc::channel::<shared::SupervisorFrame>();
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel::<shared::RendererFrame>();

    // Read once, handed to both threads: stamped into the connection handshake and into every
    // outbound `CommandEnvelope`/`SecureSubmit`.
    let generation_id = socket::generation_id_from_env();

    socket::spawn_client(generation_id, inbound_tx, outbound_rx);
    wayland::run(generation_id, inbound_rx, outbound_tx)
}
