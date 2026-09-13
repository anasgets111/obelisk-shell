//! Hand-written `org.bluez.Agent1`. Pairing waits for the user's answer through
//! `obelisk.bluetooth`'s `pairing_request`, where ADR-0030's agent accepted every request itself.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::proxies::bind_agent_manager;
use super::registry::DeviceRegistry;
use super::{AGENT_OBJECT_PATH, BluetoothSignal, PairingRequest};

/// `org.bluez.Error.Rejected` as the D-Bus error reply; `zbus::fdo::Error::Failed` has the wrong
/// name (`org.freedesktop.DBus.Error.Failed`).
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// The request on screen, and the reply BlueZ waits on unless the request only displays a code.
pub(super) struct PendingPrompt {
    pub(super) request: PairingRequest,
    reply: Option<oneshot::Sender<bool>>,
}

/// One prompt at a time. The agent fills it; [`answer`] empties it.
pub(super) type PromptSlot = Arc<Mutex<Option<PendingPrompt>>>;

/// Answers and takes down the prompt on screen. Returns whether one was showing.
pub(super) fn answer(prompts: &PromptSlot, accept: bool) -> bool {
    let Some(prompt) = prompts.lock().unwrap().take() else {
        return false;
    };
    if let Some(reply) = prompt.reply {
        let _ = reply.send(accept);
    }
    true
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` to `AA:BB:CC:DD:EE:FF`. BlueZ names every device object
/// this way, so the prompt is labelled without a call.
fn mac_from_path(path: &str) -> String {
    path.rsplit('/').next().and_then(|leaf| leaf.strip_prefix("dev_")).unwrap_or(path).replace('_', ":")
}

/// `org.bluez.Agent1`, the sole pairing agent for this session.
///
/// Numeric comparison, an incoming pairing, and a service request from an untrusted device each
/// wait for the user. A passkey or PIN to type on the device is shown with nothing to answer.
///
/// ponytail: `RequestPinCode` and `RequestPasskey` are rejected. Answering them needs a text field
/// this prompt does not have, so a legacy device that makes the user type a PIN on the host cannot
/// pair. Upgrade path: a `secure_submit` field like the network sheet's.
struct BluetoothAgent {
    prompts: PromptSlot,
    devices: DeviceRegistry,
    events: UnboundedSender<BluetoothSignal>,
}

impl BluetoothAgent {
    /// Shows `kind` for `device` and waits for the user.
    async fn ask(&self, kind: &str, device: &OwnedObjectPath, code: Option<String>) -> Result<(), AgentError> {
        let (reply, answered) = oneshot::channel();
        if !self.show(kind, device, code, Some(reply)).await {
            return Err(AgentError::Rejected("another pairing request is on screen".to_string()));
        }
        match answered.await {
            Ok(true) => Ok(()),
            _ => Err(AgentError::Rejected("the user declined, or BlueZ cancelled the request".to_string())),
        }
    }

    /// Puts the request on screen unless one already is, and returns whether it went up. A second
    /// request is refused, as polkit's agent refuses a second challenge: one dialog cannot answer
    /// two devices.
    async fn show(
        &self,
        kind: &str,
        device: &OwnedObjectPath,
        code: Option<String>,
        reply: Option<oneshot::Sender<bool>>,
    ) -> bool {
        let proxy = self.devices.lock().unwrap().get(device).map(|entry| entry.device.clone());
        let name = match proxy {
            Some(proxy) => proxy.name().await.unwrap_or_default(),
            None => String::new(),
        };
        let request = PairingRequest { kind: kind.to_string(), mac: mac_from_path(device.as_str()), name, code };
        {
            let mut slot = self.prompts.lock().unwrap();
            if slot.is_some() {
                return false;
            }
            *slot = Some(PendingPrompt { request, reply });
        }
        let _ = self.events.send(BluetoothSignal::PairingChanged);
        true
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl BluetoothAgent {
    async fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Rejected("typing a PIN on this host is not supported".to_string()))
    }

    async fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, AgentError> {
        Err(AgentError::Rejected("typing a passkey on this host is not supported".to_string()))
    }

    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) -> Result<(), AgentError> {
        self.show("display", &device, Some(pincode), None).await;
        Ok(())
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        self.ask("confirm", &device, Some(format!("{passkey:06}"))).await
    }

    /// BlueZ repeats this for every key typed on the device. The first call shows the code, and the
    /// rest find the slot taken by that same code.
    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        self.show("display", &device, Some(format!("{passkey:06}")), None).await;
    }

    async fn authorize_service(&self, device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        self.ask("service", &device, None).await
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.ask("authorize", &device, None).await
    }

    async fn cancel(&self) {
        if answer(&self.prompts, false) {
            let _ = self.events.send(BluetoothSignal::PairingChanged);
        }
    }

    async fn release(&self) {}
}

/// Exports [`BluetoothAgent`], registers it as `"DisplayYesNo"`, and requests it as the system
/// default, so pairing started elsewhere (e.g. `bluetoothctl`) asks here too. `"NoInputNoOutput"`
/// made BlueZ downgrade every pairing to Just Works; `"DisplayYesNo"` lets it run numeric
/// comparison.
///
/// Logs and continues every step: absent `bluetoothd` must not take down the Supervisor. The object
/// is exported before `RegisterAgent`, so an immediate callback finds a live object.
pub(super) async fn register_agent_best_effort(
    connection: &zbus::Connection,
    prompts: PromptSlot,
    devices: DeviceRegistry,
    events: UnboundedSender<BluetoothSignal>,
) {
    let agent = BluetoothAgent { prompts, devices, events };
    if let Err(err) = connection.object_server().at(AGENT_OBJECT_PATH, agent).await {
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
    if let Err(err) = agent_manager.register_agent(&path, "DisplayYesNo").await {
        eprintln!("bluetooth: RegisterAgent failed: {err}");
        return;
    }
    if let Err(err) = agent_manager.request_default_agent(&path).await {
        eprintln!("bluetooth: RequestDefaultAgent failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    /// A p2p pair with the agent already exported, returned as `(caller, agent, prompts, signals)`.
    /// Every test here really calls the agent, so it goes in through the builder rather than
    /// `object_server().at(..)` afterwards; see `test_support::p2p_pair` for why that ordering is
    /// the difference between a reply and a dropped call.
    async fn agent_pair() -> (zbus::Connection, zbus::Connection, PromptSlot, UnboundedReceiver<BluetoothSignal>) {
        let prompts = PromptSlot::default();
        let (events, signals) = unbounded_channel();
        let agent = BluetoothAgent { prompts: prompts.clone(), devices: Arc::default(), events };
        let (caller, agent_side) = p2p_pair_serving(|peer| peer.serve_at(AGENT_OBJECT_PATH, agent)).await;
        (caller, agent_side, prompts, signals)
    }

    async fn agent1_proxy(caller_side: &zbus::Connection) -> zbus::Proxy<'_> {
        zbus::proxy::Builder::new(caller_side)
            .destination("org.obelisk.Supervisor")
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

    fn rejected<T: std::fmt::Debug>(result: zbus::Result<T>) -> bool {
        matches!(result, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.bluez.Error.Rejected")
    }

    #[tokio::test]
    async fn request_pin_code_returns_a_properly_named_rejected_error() {
        let (caller, _agent, _prompts, _signals) = agent_pair().await;
        let proxy = agent1_proxy(&caller).await;
        assert!(rejected(proxy.call::<_, _, String>("RequestPinCode", &(dummy_device_path(),)).await));
    }

    #[tokio::test]
    async fn request_passkey_returns_a_properly_named_rejected_error() {
        let (caller, _agent, _prompts, _signals) = agent_pair().await;
        let proxy = agent1_proxy(&caller).await;
        assert!(rejected(proxy.call::<_, _, u32>("RequestPasskey", &(dummy_device_path(),)).await));
    }

    #[tokio::test]
    async fn a_confirmation_waits_for_the_user_and_returns_their_yes() {
        let (caller, _agent, prompts, mut signals) = agent_pair().await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 1234u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);

        tokio::select! {
            _ = &mut call => panic!("the call returned before the user answered"),
            signal = signals.recv() => assert_eq!(signal, Some(BluetoothSignal::PairingChanged)),
        }
        let request = prompts.lock().unwrap().as_ref().expect("the request is on screen").request.clone();
        assert_eq!(request.kind, "confirm");
        assert_eq!(request.code.as_deref(), Some("001234"), "a passkey keeps its leading zeros");
        assert_eq!(request.mac, "00:11:22:33:44:55");

        assert!(answer(&prompts, true));
        call.await.expect("an accepted confirmation returns Ok");
    }

    #[tokio::test]
    async fn a_declined_pairing_request_is_rejected() {
        let (caller, _agent, prompts, mut signals) = agent_pair().await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(),);
        let call = proxy.call::<_, _, ()>("RequestAuthorization", &args);
        tokio::pin!(call);

        tokio::select! {
            _ = &mut call => panic!("the call returned before the user answered"),
            _ = signals.recv() => {}
        }
        assert!(answer(&prompts, false));
        assert!(rejected(call.await));
    }

    #[tokio::test]
    async fn a_second_request_while_one_is_on_screen_is_refused_at_once() {
        let (caller, _agent, prompts, _signals) = agent_pair().await;
        *prompts.lock().unwrap() = Some(PendingPrompt { request: PairingRequest::default(), reply: None });

        let proxy = agent1_proxy(&caller).await;
        let result = proxy
            .call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), "0000110b-0000-1000-8000-00805f9b34fb"))
            .await;

        assert!(rejected(result));
        assert!(prompts.lock().unwrap().is_some(), "the prompt already showing stays");
    }

    #[tokio::test]
    async fn cancel_takes_the_prompt_down_and_answers_no() {
        let (caller, _agent, prompts, _signals) = agent_pair().await;
        let (reply, mut answered) = oneshot::channel();
        *prompts.lock().unwrap() = Some(PendingPrompt { request: PairingRequest::default(), reply: Some(reply) });

        let proxy = agent1_proxy(&caller).await;
        proxy.call::<_, _, ()>("Cancel", &()).await.expect("Cancel must succeed");
        assert!(prompts.lock().unwrap().is_none());
        assert_eq!(answered.try_recv(), Ok(false));
        proxy.call::<_, _, ()>("Release", &()).await.expect("Release must succeed");
    }

    #[test]
    fn mac_from_path_reads_the_address_out_of_the_object_path() {
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"), "AA:BB:CC:DD:EE:FF");
    }
}
