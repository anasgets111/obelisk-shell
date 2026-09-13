//! Hand-written `org.bluez.Agent1`; pairing waits for the user's answer in `pairing_request`.
//! Split from `dbus::bluetooth` -- see `dbus/bluetooth/mod.rs` for the module-level doc.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::proxies::bind_agent_manager;
use super::registry::DeviceRegistry;
use super::{AGENT_OBJECT_PATH, BluetoothSignal, PairingKind, PairingRequest};

/// How long a newly shown request ignores a yes, so a click meant for a request that was just
/// replaced cannot accept the one that replaced it.
pub(super) const ACCEPT_GRACE: Duration = Duration::from_millis(750);

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
    shown_at: Instant,
}

#[cfg(test)]
impl PendingPrompt {
    pub(super) fn display(mac: &str) -> Self {
        let request =
            PairingRequest { kind: PairingKind::Display, mac: mac.to_string(), name: String::new(), code: None };
        Self { request, reply: None, shown_at: Instant::now() }
    }
}

/// One prompt at a time. The agent fills it; [`answer`] and [`clear_display`] empty it.
pub(super) type PromptSlot = Arc<Mutex<Option<PendingPrompt>>>;

/// Whether the device with this MAC may raise a prompt now; `BluetoothController::invited` answers.
pub(super) type Invited = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Answers and takes down the prompt on screen. `mac` must name the device it is for; `None`, from
/// BlueZ's `Cancel`, answers whatever is showing. A yes before [`ACCEPT_GRACE`] has passed since
/// the prompt went up is ignored and leaves it up. Returns whether a prompt came down.
pub(super) fn answer(prompts: &PromptSlot, mac: Option<&str>, accept: bool, now: Instant) -> bool {
    let prompt = {
        let mut slot = prompts.lock().unwrap();
        let Some(current) = slot.as_ref() else {
            return false;
        };
        if mac.is_some_and(|mac| mac != current.request.mac) {
            return false;
        }
        if accept && now.duration_since(current.shown_at) < ACCEPT_GRACE {
            return false;
        }
        slot.take()
    };
    if let Some(reply) = prompt.and_then(|prompt| prompt.reply) {
        let _ = reply.send(accept);
    }
    true
}

/// Takes down a code display for `mac`, if that is what is showing. Returns whether it did.
pub(super) fn clear_display(prompts: &PromptSlot, mac: &str) -> bool {
    let mut slot = prompts.lock().unwrap();
    let showing = slot.as_ref().is_some_and(|prompt| prompt.reply.is_none() && prompt.request.mac == mac);
    if showing {
        slot.take();
    }
    showing
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` to `AA:BB:CC:DD:EE:FF`. BlueZ names every device object
/// this way, so the prompt is labelled without a call.
fn mac_from_path(path: &str) -> String {
    path.rsplit('/').next().and_then(|leaf| leaf.strip_prefix("dev_")).unwrap_or(path).replace('_', ":")
}

/// `org.bluez.Agent1`, this session's only pairing agent. Only an invited device (the adapter is
/// visible, or this Supervisor is pairing it) raises a prompt; a `"service"` request asks instead
/// when a tracked device reads `Paired`.
///
/// ponytail: `RequestPinCode` and `RequestPasskey` are rejected, since the card has no text field.
/// Upgrade path: a `secure_submit` field. ponytail: pairing by another client with no agent of its
/// own is refused while the adapter is hidden. Upgrade path: invite a device whose `Device1.Pairing`
/// is already true.
struct BluetoothAgent {
    prompts: PromptSlot,
    devices: DeviceRegistry,
    invited: Invited,
    events: UnboundedSender<BluetoothSignal>,
}

impl BluetoothAgent {
    /// Shows `kind` for an invited `device` and waits for the user.
    async fn ask(&self, kind: PairingKind, device: &OwnedObjectPath, code: Option<String>) -> Result<(), AgentError> {
        let (reply, answered) = oneshot::channel();
        if !self.show(kind, device, code, Some(reply)).await {
            return Err(AgentError::Rejected("the device is not invited, or another request is on screen".to_string()));
        }
        match answered.await {
            Ok(true) => Ok(()),
            _ => Err(AgentError::Rejected("the user declined, or BlueZ cancelled the request".to_string())),
        }
    }

    /// Puts the request on screen and returns whether it went up. An uninvited device's request
    /// stays down, and so does a `"service"` request from a device that is not tracked and paired:
    /// BlueZ asks for any untrusted device, and the card says it is paired. Nothing replaces a
    /// prompt already showing, except that a request BlueZ waits on replaces a code display, which
    /// has no answer to lose.
    async fn show(
        &self,
        kind: PairingKind,
        device: &OwnedObjectPath,
        code: Option<String>,
        reply: Option<oneshot::Sender<bool>>,
    ) -> bool {
        let mac = mac_from_path(device.as_str());
        let proxy = self.devices.lock().unwrap().get(device).map(|entry| entry.device.clone());
        let allowed = match (kind, &proxy) {
            (PairingKind::Service, Some(proxy)) => proxy.paired().await.unwrap_or(false),
            (PairingKind::Service, None) => false,
            _ => (self.invited)(&mac),
        };
        if !allowed {
            eprintln!(
                "bluetooth: refused a {kind:?} request from {mac}: not invited, or a service request from an unpaired device"
            );
            return false;
        }
        let name = match proxy {
            Some(proxy) => proxy.name().await.unwrap_or_default(),
            None => String::new(),
        };
        let request = PairingRequest { kind, mac, name, code };
        {
            let mut slot = self.prompts.lock().unwrap();
            let free = slot.as_ref().is_none_or(|current| current.reply.is_none() && reply.is_some());
            if !free {
                return false;
            }
            *slot = Some(PendingPrompt { request, reply, shown_at: Instant::now() });
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

    /// An error here cancels the pairing, which is right when nobody was shown the PIN.
    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) -> Result<(), AgentError> {
        if self.show(PairingKind::Display, &device, Some(pincode), None).await {
            Ok(())
        } else {
            Err(AgentError::Rejected("the PIN could not be shown".to_string()))
        }
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        self.ask(PairingKind::Confirm, &device, Some(format!("{passkey:06}"))).await
    }

    /// BlueZ repeats this for every key typed on the device. The first call shows the code, and the
    /// rest find the slot taken by that same code.
    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        self.show(PairingKind::Display, &device, Some(format!("{passkey:06}")), None).await;
    }

    async fn authorize_service(&self, device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        self.ask(PairingKind::Service, &device, None).await
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.ask(PairingKind::Authorize, &device, None).await
    }

    async fn cancel(&self) {
        if answer(&self.prompts, None, false, Instant::now()) {
            let _ = self.events.send(BluetoothSignal::PairingChanged);
        }
    }

    async fn release(&self) {}
}

/// Exports [`BluetoothAgent`] and registers it as the system default `"DisplayYesNo"` agent, so
/// pairing started elsewhere asks here too and BlueZ runs numeric comparison, not Just Works. Each
/// step logs and continues, since a missing `bluetoothd` must not take down the Supervisor.
pub(super) async fn register_agent_best_effort(
    connection: &zbus::Connection,
    prompts: PromptSlot,
    devices: DeviceRegistry,
    invited: Invited,
    events: UnboundedSender<BluetoothSignal>,
) {
    let agent = BluetoothAgent { prompts, devices, invited, events };
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

    use super::super::proxies::Device1Proxy;
    use super::super::registry::DeviceEntry;
    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    const MAC: &str = "00:11:22:33:44:55";

    /// A p2p pair with the agent already exported, returned as `(caller, agent, prompts, signals)`.
    /// `invited` answers every device the same way. Every test here really calls the agent, so it
    /// goes in through the builder rather than `object_server().at(..)` afterwards; see
    /// `test_support::p2p_pair` for why that ordering is the difference between a reply and a
    /// dropped call.
    async fn agent_pair(
        invited: bool,
        devices: DeviceRegistry,
    ) -> (zbus::Connection, zbus::Connection, PromptSlot, UnboundedReceiver<BluetoothSignal>) {
        let prompts = PromptSlot::default();
        let (events, signals) = unbounded_channel();
        let agent = BluetoothAgent { prompts: prompts.clone(), devices, invited: Arc::new(move |_| invited), events };
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

    /// A yes as the user can first give it, once the grace has passed.
    fn after_grace() -> Instant {
        Instant::now() + ACCEPT_GRACE
    }

    #[tokio::test]
    async fn typing_a_pin_or_passkey_on_this_host_is_rejected_by_name() {
        let (caller, _agent, _prompts, _signals) = agent_pair(true, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;
        assert!(rejected(proxy.call::<_, _, String>("RequestPinCode", &(dummy_device_path(),)).await));
        assert!(rejected(proxy.call::<_, _, u32>("RequestPasskey", &(dummy_device_path(),)).await));
    }

    #[tokio::test]
    async fn a_confirmation_waits_for_the_user_and_returns_their_yes() {
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 1234u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);

        tokio::select! {
            _ = &mut call => panic!("the call returned before the user answered"),
            signal = signals.recv() => assert_eq!(signal, Some(BluetoothSignal::PairingChanged)),
        }
        let request = prompts.lock().unwrap().as_ref().expect("the request is on screen").request.clone();
        assert_eq!(request.kind, PairingKind::Confirm);
        assert_eq!(request.code.as_deref(), Some("001234"), "a passkey keeps its leading zeros");
        assert_eq!(request.mac, MAC);

        assert!(answer(&prompts, Some(MAC), true, after_grace()));
        call.await.expect("an accepted confirmation returns Ok");
    }

    #[tokio::test]
    async fn an_uninvited_device_raises_nothing() {
        let (caller, _agent, prompts, _signals) = agent_pair(false, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;

        assert!(rejected(proxy.call::<_, _, ()>("RequestAuthorization", &(dummy_device_path(),)).await));
        assert!(rejected(proxy.call::<_, _, ()>("RequestConfirmation", &(dummy_device_path(), 1u32)).await));
        assert!(
            rejected(proxy.call::<_, _, ()>("DisplayPinCode", &(dummy_device_path(), "0000")).await),
            "a PIN nobody was shown cancels the pairing"
        );
        assert!(prompts.lock().unwrap().is_none());
    }

    /// A `Device1` that answers only `Paired`.
    struct Bonded(bool);

    #[zbus::interface(name = "org.bluez.Device1")]
    impl Bonded {
        #[zbus(property)]
        async fn paired(&self) -> bool {
            self.0
        }
    }

    /// A registry tracking one device at `dummy_device_path()` whose `Paired` reads `paired`, with
    /// the connections that serve it.
    async fn tracked(paired: bool) -> (DeviceRegistry, zbus::Connection, zbus::Connection) {
        let (caller, served) = p2p_pair_serving(|peer| peer.serve_at(dummy_device_path(), Bonded(paired))).await;
        let device = Device1Proxy::builder(&caller).path(dummy_device_path()).unwrap().build().await.unwrap();
        let entry = DeviceEntry { mac: MAC.to_string(), device, battery: None, forwarder: tokio::spawn(async {}) };
        let devices = DeviceRegistry::default();
        devices.lock().unwrap().insert(dummy_device_path().into(), entry);
        (devices, caller, served)
    }

    #[tokio::test]
    async fn a_service_request_while_hidden_needs_a_tracked_paired_device() {
        let hid = "00001124-0000-1000-8000-00805f9b34fb";
        let (unpaired, _unpaired_caller, _unpaired_device) = tracked(false).await;
        for devices in [DeviceRegistry::default(), unpaired] {
            let (caller, _agent, prompts, _signals) = agent_pair(false, devices).await;
            let proxy = agent1_proxy(&caller).await;
            assert!(rejected(proxy.call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), hid)).await));
            assert!(prompts.lock().unwrap().is_none(), "an unknown or unpaired device raises nothing");
        }

        let (paired, _paired_caller, _paired_device) = tracked(true).await;
        let (caller, _agent, prompts, mut signals) = agent_pair(false, paired).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), hid);
        let call = proxy.call::<_, _, ()>("AuthorizeService", &args);
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("a paired device's request waits for the user"),
            _ = signals.recv() => {}
        }
        assert_eq!(prompts.lock().unwrap().as_ref().map(|prompt| prompt.request.kind), Some(PairingKind::Service));
        assert!(answer(&prompts, Some(MAC), false, Instant::now()));
        assert!(rejected(call.await));
    }

    #[tokio::test]
    async fn cancel_rejects_a_confirmation_that_is_still_waiting() {
        // zbus serves `Cancel` while `RequestConfirmation` is parked on the user; this pins that.
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 1234u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("the call returned before anyone answered"),
            _ = signals.recv() => {}
        }

        let (waiting, cancelled) = tokio::join!(call, proxy.call::<_, _, ()>("Cancel", &()));

        cancelled.expect("Cancel must succeed");
        assert!(rejected(waiting));
        assert!(prompts.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_second_request_while_one_waits_is_refused_at_once() {
        let (caller, _agent, prompts, _signals) = agent_pair(true, DeviceRegistry::default()).await;
        let (reply, _answered) = oneshot::channel();
        *prompts.lock().unwrap() = Some(PendingPrompt { reply: Some(reply), ..PendingPrompt::display(MAC) });

        let proxy = agent1_proxy(&caller).await;
        let result = proxy
            .call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), "0000110b-0000-1000-8000-00805f9b34fb"))
            .await;

        assert!(rejected(result));
        assert!(prompts.lock().unwrap().is_some(), "the prompt already waiting stays");
    }

    #[tokio::test]
    async fn a_confirmation_replaces_a_code_display() {
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        *prompts.lock().unwrap() = Some(PendingPrompt::display("AA:AA:AA:AA:AA:AA"));
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 7u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("the call returned before anyone answered"),
            _ = signals.recv() => {}
        }

        assert_eq!(prompts.lock().unwrap().as_ref().map(|prompt| prompt.request.kind), Some(PairingKind::Confirm));
        assert!(answer(&prompts, Some(MAC), false, Instant::now()));
        assert!(rejected(call.await));
    }

    #[test]
    fn a_yes_is_ignored_too_soon_or_for_another_device() {
        let prompts = PromptSlot::default();
        let (reply, mut answered) = oneshot::channel();
        *prompts.lock().unwrap() = Some(PendingPrompt { reply: Some(reply), ..PendingPrompt::display(MAC) });

        assert!(!answer(&prompts, Some(MAC), true, Instant::now()), "a yes inside the grace is ignored");
        assert!(
            !answer(&prompts, Some("AA:AA:AA:AA:AA:AA"), true, after_grace()),
            "an answer for another device is ignored"
        );
        assert!(prompts.lock().unwrap().is_some());

        assert!(answer(&prompts, Some(MAC), true, after_grace()));
        assert_eq!(answered.try_recv(), Ok(true));
    }
}
