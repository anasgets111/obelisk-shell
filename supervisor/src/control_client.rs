//! Client half of `oblisk set` and `oblisk toggle` (ADR-0112): connect to the running Supervisor,
//! send a handshake and one [`shared::SetState`] frame, then disconnect.
//!
//! Separate from `socket.rs`, the listener: this is the only external connector, running from a
//! compositor keybind's `spawn` with no runtime, config directory, or D-Bus.

use std::error::Error;

use shared::framing::write_json_frame;
use shared::{CONTROL_CLIENT_GENERATION, ConnectionHandshake, RendererFrame, SetState};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Delivers `set` to the shell or reports connection failure. The Supervisor forwards it to the
/// onscreen generation, which applies or refuses it by name on its stderr. No reply returns here:
/// keybinds have nowhere to show one, and this process can only observe that the shell is absent.
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
