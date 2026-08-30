//! Hand-written `org.bluez.Agent1` (ADR-0030: Just-Works-only pairing, enforced by our own agent).
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::proxies::bind_agent_manager;
use super::AGENT_OBJECT_PATH;

/// `org.bluez.Error.Rejected` as a properly-named D-Bus error reply -- `zbus::fdo::Error::Failed`
/// would carry the wrong error name (`org.freedesktop.DBus.Error.Failed`).
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// `org.bluez.Agent1`, registered as the sole pairing agent for this session (ADR-0030).
/// `RequestPinCode`/`RequestPasskey`/`DisplayPinCode` reject (legacy PIN-only devices cannot
/// pair through this controller -- `bluetooth:pair(mac)` takes no PIN/passkey argument).
/// `RequestConfirmation`/`DisplayPasskey`/`AuthorizeService`/`RequestAuthorization`
/// auto-accept unconditionally: there is no UI to ask a human, and refusing would silently
/// break `connect()` for already-trusted or SSP-Just-Works devices. `Cancel`/`Release` are
/// no-ops.
struct BluetoothAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl BluetoothAgent {
    async fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Rejected("legacy PIN-only pairing is not supported".to_string()))
    }

    async fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, AgentError> {
        Err(AgentError::Rejected("legacy PIN-only pairing is not supported".to_string()))
    }

    async fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) -> Result<(), AgentError> {
        Err(AgentError::Rejected("legacy PIN-only pairing is not supported".to_string()))
    }

    async fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) -> Result<(), AgentError> {
        Ok(())
    }

    async fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    async fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        Ok(())
    }

    async fn request_authorization(&self, _device: OwnedObjectPath) -> Result<(), AgentError> {
        Ok(())
    }

    async fn cancel(&self) {}

    async fn release(&self) {}
}

/// Exports [`BluetoothAgent`] on `connection`'s object server, then registers it with
/// capability `"NoInputNoOutput"` (forces Just Works for any SSP-capable peer, ADR-0030) and
/// requests it as the system default -- so a pairing attempt triggered outside our own
/// `pair()` (e.g. `bluetoothctl`) hits this policy too. Every step is logged-and-continue,
/// not `?`-propagated: a machine with no `bluetoothd` running must not take the whole
/// Supervisor down over a pairing agent it doesn't need yet. Exports the agent object
/// *before* calling `RegisterAgent`, so a callback right after registration always finds a
/// live object to dispatch to.
pub(super) async fn register_agent_best_effort(connection: &zbus::Connection) {
    if let Err(err) = connection.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await {
        eprintln!("bluetooth: failed to export the Agent1 object at {AGENT_OBJECT_PATH}: {err}");
        return;
    }
    let agent_manager = match bind_agent_manager(connection).await {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("bluetooth: failed to bind org.bluez.AgentManager1 (bluetoothd not running?): {err}");
            return;
        }
    };
    let path = match ObjectPath::try_from(AGENT_OBJECT_PATH) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("bluetooth: {AGENT_OBJECT_PATH} is not a valid object path: {err}");
            return;
        }
    };
    if let Err(err) = agent_manager.register_agent(&path, "NoInputNoOutput").await {
        eprintln!("bluetooth: RegisterAgent failed: {err}");
        return;
    }
    if let Err(err) = agent_manager.request_default_agent(&path).await {
        eprintln!("bluetooth: RequestDefaultAgent failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    /// A connected pair of p2p zbus connections, no bus daemon involved. Both builders must
    /// be driven concurrently via `try_join!`.
    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder = zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    async fn agent1_proxy(caller_side: &zbus::Connection) -> zbus::Proxy<'_> {
        zbus::proxy::Builder::new(caller_side)
            .destination("org.oblisk.Supervisor")
            .expect("valid destination bus name")
            .path(AGENT_OBJECT_PATH)
            .expect("valid object path")
            .interface("org.bluez.Agent1")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p proxy to the agent")
    }

    fn dummy_device_path() -> zbus::zvariant::ObjectPath<'static> {
        zbus::zvariant::ObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").expect("valid object path")
    }

    #[tokio::test]
    async fn request_pin_code_returns_a_properly_named_rejected_error() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        let result: zbus::Result<String> = proxy.call("RequestPinCode", &(dummy_device_path(),)).await;

        match result.expect_err("RequestPinCode must be rejected") {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), "org.bluez.Error.Rejected"),
            other => panic!("expected a MethodError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_passkey_returns_a_properly_named_rejected_error() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        let result: zbus::Result<u32> = proxy.call("RequestPasskey", &(dummy_device_path(),)).await;

        match result.expect_err("RequestPasskey must be rejected") {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), "org.bluez.Error.Rejected"),
            other => panic!("expected a MethodError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_confirmation_auto_accepts() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("RequestConfirmation", &(dummy_device_path(), 123456u32)).await.expect("RequestConfirmation must auto-accept");
    }

    #[tokio::test]
    async fn request_authorization_auto_accepts() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("RequestAuthorization", &(dummy_device_path(),)).await.expect("RequestAuthorization must auto-accept");
    }

    #[tokio::test]
    async fn authorize_service_auto_accepts() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy
            .call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), "0000110b-0000-1000-8000-00805f9b34fb"))
            .await
            .expect("AuthorizeService must auto-accept");
    }

    #[tokio::test]
    async fn cancel_and_release_are_no_ops() {
        let (agent_side, caller_side) = p2p_pair().await;
        agent_side.object_server().at(AGENT_OBJECT_PATH, BluetoothAgent).await.expect("failed to export BluetoothAgent");

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("Cancel", &()).await.expect("Cancel must succeed");
        proxy.call::<_, _, ()>("Release", &()).await.expect("Release must succeed");
    }
}
