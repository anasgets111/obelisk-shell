#[cfg_attr(not(test), macro_use)]
extern crate shared;

mod check;
mod image;
mod layout;
mod lua;
mod socket;
mod text;
mod wake;
mod wayland;

/// Two OS threads, two channels (ADR-0039). `wayland::run` owns Wayland dispatch, EGL, the Lua VM,
/// retained `Scene`, and live signals because `mlua::Lua` is `!Send` and the scene must reach the
/// GL-context thread. `socket::spawn_client` owns a dedicated I/O thread with a current-thread
/// Tokio runtime and only does framed control-socket I/O:
/// - inbound `SupervisorFrame`s cross `std::sync::mpsc`; `wayland::run` drains them and the socket
///   thread wakes its eventfd after each frame (ADR-0124).
/// - outbound `RendererFrame`s queue on `tokio::sync::mpsc` and go to the wire there.
///   `UnboundedSender::send` is synchronous and non-blocking, so Wayland sends directly.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Before meaningful allocation (ADR-0123), pin glibc's mmap threshold. Above it, glibc maps
    // allocations and returns them on free; below it, heap memory only shrinks from the top. The
    // default threshold rises after freeing a mapped 10 MB decode, putting the next decode on the
    // heap under later small allocations. Six wallpaper changes measured 22 MB -> 64 MB heap.
    // Pinning it makes image decodes and SVG rasters transient mappings; cost: one `mmap` per
    // allocation over 1 MB, not a per-frame path.
    //
    // SAFETY: a plain FFI call with two integers, before any thread exists.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20);
    }
    // `obelisk check` re-execs this binary because the Supervisor has no `mlua`, before any Wayland
    // connection because checking needs none.
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

    // Not a command. Without the Supervisor there is no socket, generation id, capability push, or
    // reaper; this would map a bar that never updates or exits. Refuse with one visible line.
    //
    // Check the env var before connecting. `process::spawn_group_leader` sets
    // `OBELISK_GENERATION_ID` on every Renderer, at boot and generation swap.
    if std::env::var_os(shared::GENERATION_ID_ENV).is_none() {
        eprintln!(
            "obelisk-renderer is not a command. The Supervisor starts it, one process per renderer \
             generation, and reaps it.\n\nRun `obelisk` instead. `obelisk --help` lists what it takes."
        );
        std::process::exit(2);
    }

    // Bounded, so a Supervisor pushing faster than the Wayland thread can drain cannot grow this
    // process's heap instead of Supervisor's; see `socket::INBOUND_CAPACITY`.
    let (inbound_tx, inbound_rx) =
        tokio::sync::mpsc::channel::<shared::SupervisorFrame>(crate::socket::INBOUND_CAPACITY);
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel::<shared::RendererFrame>();

    // Read once for both threads: stamp the handshake and every outbound
    // `CommandEnvelope`/`SecureSubmit`.
    let generation_id = socket::generation_id_from_env();

    let waker = wake::Waker::new()?;
    socket::spawn_client(generation_id, inbound_tx, outbound_rx, waker.clone());
    wayland::run(generation_id, inbound_rx, outbound_tx, waker)
}
