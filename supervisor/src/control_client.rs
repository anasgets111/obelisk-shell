//! Client half of `obelisk set`, `obelisk toggle` (ADR-0112) and `obelisk call` (ADR-0197):
//! connect to the running Supervisor and send a handshake and one frame.
//!
//! `set` and `toggle` disconnect immediately; `call` waits, because the whole point of a call is
//! its answer.
//!
//! Separate from `socket.rs`, the listener: this is the only external connector, running from a
//! compositor keybind's `spawn` with no runtime, config directory, or D-Bus.

use std::error::Error;

use std::time::Duration;

use shared::framing::{read_json_frame, write_json_frame};
use shared::{
    CONTROL_CLIENT_GENERATION, Call, CallOutcome, ConnectionHandshake, RendererFrame, SetState, SupervisorFrame,
};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// How long `obelisk call` waits for an answer.
///
/// Generous against the work a handler can actually do: config Lua runs under a 5ms CPU cap, so a
/// reply that has not arrived by now means the shell is wedged or the Renderer was replaced mid-call,
/// not that the handler is still thinking. Expiring says "outcome unknown", never "nothing
/// happened" -- the call may well have run.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Delivers `set` to the shell or reports connection failure. The Supervisor forwards it to the
/// onscreen generation, which applies or refuses it by name on its stderr. No reply returns here:
/// keybinds have nowhere to show one, and this process can only observe that the shell is absent.
pub fn send(set: SetState) -> Result<(), Box<dyn Error>> {
    let path = shared::control_socket_path()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut stream = UnixStream::connect(&path)
            .await
            .map_err(|err| format!("cannot reach the shell at {}: {err} (is obelisk running?)", path.display()))?;
        write_json_frame(&mut stream, &ConnectionHandshake { generation_id: CONTROL_CLIENT_GENERATION }).await?;
        write_json_frame(&mut stream, &RendererFrame::SetState(set)).await?;
        stream.shutdown().await?;
        Ok(())
    })
}

/// Sends one `obelisk call` and prints what the config returned.
///
/// The `id` sent is zero and is overwritten by the Supervisor, which owns the pending table; a
/// client-chosen id would let one peer collect another's answer.
pub fn call(name: String, arguments: Vec<serde_json::Value>) -> Result<(), Box<dyn Error>> {
    let path = shared::control_socket_path()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut stream = UnixStream::connect(&path)
            .await
            .map_err(|err| format!("cannot reach the shell at {}: {err} (is obelisk running?)", path.display()))?;
        write_json_frame(&mut stream, &ConnectionHandshake { generation_id: CONTROL_CLIENT_GENERATION }).await?;
        write_json_frame(&mut stream, &RendererFrame::Call(Call { id: 0, name: name.clone(), arguments })).await?;

        let answer = tokio::time::timeout(CALL_TIMEOUT, read_json_frame::<_, SupervisorFrame>(&mut stream))
            .await
            .map_err(|_| {
                format!(
                    "the shell did not answer `{name}` within {}s; the call may still have run",
                    CALL_TIMEOUT.as_secs()
                )
            })??;
        match answer {
            SupervisorFrame::CallResult(result) => match result.outcome {
                CallOutcome::Failed(why) => Err(format!("`{name}` failed: {why}").into()),
                CallOutcome::Returned(value) => {
                    match value {
                        // Nothing to say, so nothing is printed: an action run for its effect
                        // should not make a keybind's shell noisy.
                        serde_json::Value::Null => {}
                        // A bare string prints as itself. `rec.toggle` answering `recording` is for
                        // a human reading a terminal, and `"recording"` with quotes is for nobody.
                        serde_json::Value::String(text) => println!("{text}"),
                        other => println!("{other}"),
                    }
                    Ok(())
                }
            },
            other => Err(format!("the shell answered `{name}` with {other:?} instead of a result").into()),
        }
    })
}
