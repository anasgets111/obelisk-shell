//! Hand-written `org.bluez.Agent1` (ADR-0030: our agent enforces Just-Works-only pairing).
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::AGENT_OBJECT_PATH;
use super::proxies::bind_agent_manager;

/// `org.bluez.Error.Rejected` as the D-Bus error reply; `zbus::fdo::Error::Failed` has the wrong
/// name (`org.freedesktop.DBus.Error.Failed`).
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// `org.bluez.Agent1`, the sole pairing agent for this session (ADR-0030). Rejects
/// `RequestPinCode`/`RequestPasskey`/`DisplayPinCode`: legacy PIN devices cannot use
/// `bluetooth:pair(mac)`, which has no PIN argument. Auto-accepts
/// `RequestConfirmation`/`DisplayPasskey`/`AuthorizeService`/`RequestAuthorization` because no UI
/// can ask a human and refusal breaks trusted or SSP-Just-Works `connect()`. `Cancel`/`Release`
/// are no-ops.
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

/// Exports [`BluetoothAgent`], registers capability `"NoInputNoOutput"` (forcing Just Works for
/// SSP peers), and requests it as the system default, so external pairing (e.g. `bluetoothctl`)
/// follows this policy too (ADR-0030). Logs and continues every step: absent `bluetoothd` must
/// not take down the Supervisor. The object is exported before `RegisterAgent`, so an immediate
/// callback finds a live object.
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
    use crate::capabilities::test_support::p2p_pair_serving;

    /// A p2p pair with the agent already exported, returned as `(caller, agent)`. Every test here
    /// really calls the agent, so it goes in through the builder rather than
    /// `object_server().at(..)` afterwards; see `test_support::p2p_pair` for why that ordering is
    /// the difference between a reply and a dropped call.
    async fn agent_pair() -> (zbus::Connection, zbus::Connection) {
        p2p_pair_serving(|peer| peer.serve_at(AGENT_OBJECT_PATH, BluetoothAgent)).await
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
        let (caller_side, _agent_side) = agent_pair().await;

        let proxy = agent1_proxy(&caller_side).await;
        let result: zbus::Result<String> = proxy.call("RequestPinCode", &(dummy_device_path(),)).await;

        match result.expect_err("RequestPinCode must be rejected") {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), "org.bluez.Error.Rejected"),
            other => panic!("expected a MethodError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_passkey_returns_a_properly_named_rejected_error() {
        let (caller_side, _agent_side) = agent_pair().await;

        let proxy = agent1_proxy(&caller_side).await;
        let result: zbus::Result<u32> = proxy.call("RequestPasskey", &(dummy_device_path(),)).await;

        match result.expect_err("RequestPasskey must be rejected") {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), "org.bluez.Error.Rejected"),
            other => panic!("expected a MethodError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_confirmation_auto_accepts() {
        let (caller_side, _agent_side) = agent_pair().await;

        let proxy = agent1_proxy(&caller_side).await;
        proxy
            .call::<_, _, ()>("RequestConfirmation", &(dummy_device_path(), 123456u32))
            .await
            .expect("RequestConfirmation must auto-accept");
    }

    #[tokio::test]
    async fn request_authorization_auto_accepts() {
        let (caller_side, _agent_side) = agent_pair().await;

        let proxy = agent1_proxy(&caller_side).await;
        proxy
            .call::<_, _, ()>("RequestAuthorization", &(dummy_device_path(),))
            .await
            .expect("RequestAuthorization must auto-accept");
    }

    #[tokio::test]
    async fn authorize_service_auto_accepts() {
        let (caller_side, _agent_side) = agent_pair().await;

        let proxy = agent1_proxy(&caller_side).await;
        proxy
            .call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), "0000110b-0000-1000-8000-00805f9b34fb"))
            .await
            .expect("AuthorizeService must auto-accept");
    }

    #[tokio::test]
    async fn cancel_and_release_are_no_ops() {
        let (caller_side, _agent_side) = agent_pair().await;

        let proxy = agent1_proxy(&caller_side).await;
        proxy.call::<_, _, ()>("Cancel", &()).await.expect("Cancel must succeed");
        proxy.call::<_, _, ()>("Release", &()).await.expect("Release must succeed");
    }
}
