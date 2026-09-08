//! The exported `org.kde.StatusNotifierWatcher` object itself (ADR-0031's "dual-role dance").
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use super::TraySignal;
use super::registration::resolve_registration;
use super::registry::{ItemRegistry, register_item};

pub(super) struct StatusNotifierWatcher {
    pub(super) connection: zbus::Connection,
    pub(super) registry: ItemRegistry,
    pub(super) host_registered: Arc<Mutex<bool>>,
    pub(super) events: UnboundedSender<TraySignal>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    #[zbus(name = "RegisterStatusNotifierItem")]
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string());
        // Log as well as return: the registering icon usually shows nothing, so a refused
        // registration is otherwise invisible to the shell. A Vesktop registration vanished this
        // way for a whole debugging session.
        let resolved = resolve_registration(&self.connection, &service, sender.as_deref()).await.map_err(|err| {
            eprintln!("tray: RegisterStatusNotifierItem({service:?}) could not be resolved: {err}");
            zbus::fdo::Error::Failed(format!("RegisterStatusNotifierItem({service:?}) could not be resolved: {err}"))
        })?;
        // KDE's watcher answers `RegisterStatusNotifierItem` by storing `service + path` and
        // publishing that, so a host never has to guess where an item sits. Ours resolves the same
        // pair and now says so (ADR-0172).
        let registered_id = format!("{}{}", resolved.unique_name.as_str(), resolved.object_path.as_str());

        register_item(&self.connection, &self.registry, &self.events, resolved).await.map_err(|err| {
            eprintln!("tray: RegisterStatusNotifierItem({service:?}) failed: {err}");
            zbus::fdo::Error::Failed(format!("RegisterStatusNotifierItem({service:?}) failed: {err}"))
        })?;

        let _ = emitter.status_notifier_item_registered(&registered_id).await;
        Ok(())
    }

    #[zbus(name = "RegisterStatusNotifierHost")]
    async fn register_status_notifier_host(
        &self,
        _service: String,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        // Accepted trivially (ADR-0031). Oblisk is the relevant host; this exists for spec
        // completeness and permits self-registration (see `TrayController::new`).
        let was_registered = {
            let mut guard = self.host_registered.lock().unwrap();
            let was = *guard;
            *guard = true;
            was
        };
        if !was_registered {
            let _ = emitter.status_notifier_host_registered().await;
        }
    }

    #[zbus(property, name = "RegisteredStatusNotifierItems")]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        // `":1.42/StatusNotifierItem"`, the shape KDE's watcher publishes and Plasma's own host
        // parses; a bare bus name is what that host rejects as an invalid id (ADR-0172).
        self.registry
            .lock()
            .unwrap()
            .keys()
            .map(|(unique_name, object_path)| format!("{unique_name}{object_path}"))
            .collect()
    }

    #[zbus(property, name = "IsStatusNotifierHostRegistered")]
    async fn is_status_notifier_host_registered(&self) -> bool {
        *self.host_registered.lock().unwrap()
    }

    #[zbus(property, name = "ProtocolVersion")]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_host_registered(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_host_unregistered(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::OwnedObjectPath;

    use super::super::{DEFAULT_ITEM_OBJECT_PATH, RawIconPixmap, RawToolTip, WATCHER_OBJECT_PATH};
    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    // ---- fabricated unique names must be rejected ----

    /// Builds a real method call with `sender` in its `SENDER` header, so `header.sender()` matches
    /// a real call and the test exercises the interface method.
    fn register_call_message(sender: &str) -> zbus::Message {
        zbus::Message::method_call(WATCHER_OBJECT_PATH, "RegisterStatusNotifierItem")
            .expect("valid method-call builder")
            .sender(sender)
            .expect("valid sender unique name")
            .build(&())
            .expect("well-formed method-call message")
    }

    fn test_watcher(connection: zbus::Connection, registry: ItemRegistry) -> StatusNotifierWatcher {
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        StatusNotifierWatcher { connection, registry, host_registered: Arc::new(Mutex::new(false)), events }
    }

    /// Stub for the properties `register_item` reads. A bare p2p peer never replies, so the real
    /// calls hang; without this, the accepts test hung until SIGKILL. `Menu` returns `/`, skipping
    /// DBusMenu/GetLayout.
    struct StubStatusNotifierItem;

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl StubStatusNotifierItem {
        #[zbus(property, name = "Id")]
        fn id(&self) -> String {
            "stub-item".to_string()
        }
        #[zbus(property, name = "Title")]
        fn title(&self) -> String {
            "Stub Item".to_string()
        }
        #[zbus(property, name = "IconName")]
        fn icon_name(&self) -> String {
            String::new()
        }
        #[zbus(property, name = "IconPixmap")]
        fn icon_pixmap(&self) -> Vec<RawIconPixmap> {
            Vec::new()
        }
        #[zbus(property, name = "Status")]
        fn status(&self) -> String {
            "Active".to_string()
        }
        #[zbus(property, name = "ItemIsMenu")]
        fn item_is_menu(&self) -> bool {
            false
        }
        #[zbus(property, name = "ToolTip")]
        fn tool_tip(&self) -> RawToolTip {
            (String::new(), Vec::new(), String::new(), String::new())
        }
        #[zbus(property, name = "Menu")]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/").expect("\"/\" is a valid object path")
        }
    }

    /// Stub for `NameHasOwner`, which the pre-insert liveness check calls on a p2p connection.
    struct StubDBusDaemon;

    #[zbus::interface(name = "org.freedesktop.DBus")]
    impl StubDBusDaemon {
        #[zbus(name = "NameHasOwner")]
        fn name_has_owner(&self, _name: String) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn register_status_notifier_item_accepts_a_unique_name_matching_the_real_sender() {
        let (connection, peer) = p2p_pair().await;
        peer.object_server()
            .at(DEFAULT_ITEM_OBJECT_PATH, StubStatusNotifierItem)
            .await
            .expect("failed to export the stub StatusNotifierItem");
        peer.object_server()
            .at("/org/freedesktop/DBus", StubDBusDaemon)
            .await
            .expect("failed to export the stub org.freedesktop.DBus");

        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let watcher = test_watcher(connection.clone(), registry.clone());
        let emitter =
            zbus::object_server::SignalEmitter::new(&connection, WATCHER_OBJECT_PATH).expect("valid signal emitter");

        let message = register_call_message(":1.5");
        let result = watcher.register_status_notifier_item(":1.5".to_string(), message.header(), emitter).await;

        assert!(result.is_ok(), "a claimed unique name equal to the real sender must be accepted: {result:?}");
        assert_eq!(registry.lock().unwrap().len(), 1, "a matching registration must create exactly one registry entry");
    }

    #[tokio::test]
    async fn register_status_notifier_item_rejects_a_fabricated_unique_name_and_creates_no_registry_entry() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let watcher = test_watcher(connection.clone(), registry.clone());
        let emitter =
            zbus::object_server::SignalEmitter::new(&connection, WATCHER_OBJECT_PATH).expect("valid signal emitter");

        // The real sender is :1.5; the call claims fabricated, never-connected :999.1.
        let message = register_call_message(":1.5");
        let result = watcher.register_status_notifier_item(":999.1".to_string(), message.header(), emitter).await;

        assert!(result.is_err(), "a fabricated unique name not equal to the real sender must be rejected");
        assert!(registry.lock().unwrap().is_empty(), "a rejected registration must not create a registry entry");
    }
}
