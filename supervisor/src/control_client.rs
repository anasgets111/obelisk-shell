//! The client half of `oblisk set` and `oblisk toggle` (ADR-0112): one connection to the running
//! Supervisor's control socket, a handshake, one [`shared::SetState`] frame, and out.
//!
//! Here rather than in `socket.rs`, which is the listener: this is the only code in the workspace
//! that *connects* to that socket from outside a Renderer, and it runs in a process that has no
//! runtime, no config directory and no D-Bus -- a compositor keybind's `spawn`.

use std::error::Error;

use shared::framing::write_json_frame;
use shared::{CONTROL_CLIENT_GENERATION, ConnectionHandshake, RendererFrame, SetState};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Delivers `set` to the shell, or says why it could not. The Supervisor forwards it to the
/// generation on screen, which applies it or refuses it by name on its own stderr; nothing comes
/// back here, since a keybind has nowhere to show an answer and the only failure this process can
/// see is the shell not running.
pub fn send(set: SetState) -> Result<(), Box<dyn Error>> {
    let path = shared::control_socket_path()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut stream = UnixStream::connect(&path)
            .await
            .map_err(|err| format!("cannot reach the shell at {}: {err} (is oblisk running?)", path.display()))?;
        write_json_frame(&mut stream, &ConnectionHandshake { generation_id: CONTROL_CLIENT_GENERATION }).await?;
        write_json_frame(&mut stream, &RendererFrame::SetState(set)).await?;
        stream.shutdown().await?;
        Ok(())
    })
}
