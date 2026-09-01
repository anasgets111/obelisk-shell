mod check;
mod image;
mod layout;
mod lua;
mod socket;
mod text;
mod wayland;

/// Two OS threads, two channels (ADR-0039, build-steps.md Phase 18). `wayland::run` owns the
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
    // `oblisk check` re-execs this binary rather than duplicating the loader in the Supervisor,
    // which has no `mlua`. Before any Wayland connection, because the point is that it needs none.
    if std::env::var_os(shared::CHECK_ENV).is_some() {
        let config_dir = shared::config_dir()?;
        return match check::run(&config_dir) {
            Ok(report) => {
                print!("{report}");
                Ok(())
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        };
    }

    // Not a command. Without the Supervisor this process has no control socket to connect to, no
    // generation id, no capability pushes and nobody to reap it, so it would map a bar that never
    // updates and never exits. Refusing here turns that into one line instead of a socket error
    // from a thread the user cannot see.
    //
    // The env var rather than the socket, because the check has to happen before the connection is
    // attempted, and `OBLISK_GENERATION_ID` is what `process::spawn_group_leader` sets on every
    // Renderer the Supervisor starts, boot and generation swap alike.
    if std::env::var_os(shared::GENERATION_ID_ENV).is_none() {
        eprintln!(
            "oblisk-renderer is not a command. The Supervisor starts it, one process per renderer \
             generation, and reaps it.\n\nRun `oblisk` instead. `oblisk --help` lists what it takes."
        );
        std::process::exit(2);
    }

    let (inbound_tx, inbound_rx) = std::sync::mpsc::channel::<shared::SupervisorFrame>();
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel::<shared::RendererFrame>();

    // Read once, handed to both threads: stamped into the connection handshake and into every
    // outbound `CommandEnvelope`/`SecureSubmit`.
    let generation_id = socket::generation_id_from_env();

    socket::spawn_client(generation_id, inbound_tx, outbound_rx);
    wayland::run(generation_id, inbound_rx, outbound_tx)
}
