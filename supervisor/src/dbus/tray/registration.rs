//! `RegisterStatusNotifierItem`'s `service`-argument classification and resolution (ADR-0031).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use zbus::names::{BusName, OwnedUniqueName, WellKnownName};
use zbus::zvariant::OwnedObjectPath;

use super::DEFAULT_ITEM_OBJECT_PATH;

/// How `RegisterStatusNotifierItem`'s raw `service` argument classifies, before any D-Bus I/O
/// (ADR-0031). Pure and total: every `&str` lands in exactly one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistrationTarget {
    /// `service` was an object path (`service.starts_with('/')`) -- the bus name comes from the
    /// calling message's own sender, not from `service` itself.
    ObjectPathFromSender { object_path: String },
    /// `service` was already a unique name (`:N.M`) -- used directly.
    UniqueName { unique_name: String },
    /// `service` was a well-known bus name -- needs a `GetNameOwner` round trip (done by the
    /// caller) to resolve to a unique name.
    WellKnownName { well_known_name: String },
}

fn classify_service_arg(service: &str) -> RegistrationTarget {
    if service.starts_with('/') {
        RegistrationTarget::ObjectPathFromSender { object_path: service.to_string() }
    } else if service.starts_with(':') {
        RegistrationTarget::UniqueName { unique_name: service.to_string() }
    } else {
        RegistrationTarget::WellKnownName { well_known_name: service.to_string() }
    }
}

#[derive(Debug)]
pub(super) enum RegistrationError {
    NoSender,
    InvalidName(String),
    Dbus(String),
    /// `service` was already a unique name (`:N.M`) but didn't equal the real, authenticated
    /// sender of this call: a connection can only ever truthfully claim its own unique name
    /// (bus-daemon-filled, unspoofable), so any mismatch means the caller fabricated a name
    /// it doesn't own.
    UniqueNameMismatch { claimed: String, sender: String },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSender => write!(f, "an object-path service argument requires a message sender, but none was present"),
            Self::InvalidName(err) => write!(f, "invalid D-Bus name: {err}"),
            Self::Dbus(err) => write!(f, "{err}"),
            Self::UniqueNameMismatch { claimed, sender } => {
                write!(f, "claimed unique name {claimed:?} does not match the real sender {sender:?}")
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

/// Resolves `RegisterStatusNotifierItem`'s `service` argument plus the calling message's
/// sender into `(unique_name, object_path)` -- the registry key this controller actually uses
/// (ADR-0031). The well-known-name branch is the only one that performs I/O (`GetNameOwner`);
/// the other two are resolved synchronously.
pub(super) async fn resolve_registration(connection: &zbus::Connection, service: &str, sender: Option<&str>) -> Result<(OwnedUniqueName, OwnedObjectPath), RegistrationError> {
    match classify_service_arg(service) {
        RegistrationTarget::ObjectPathFromSender { object_path } => {
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            let unique_name = OwnedUniqueName::try_from(sender).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let object_path = OwnedObjectPath::try_from(object_path).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            Ok((unique_name, object_path))
        }
        RegistrationTarget::UniqueName { unique_name } => {
            // A connection can only ever truthfully claim its own real unique name: reject any
            // claimed unique name that doesn't equal the real, bus-daemon-authenticated
            // sender. Otherwise any session-bus peer could call
            // `RegisterStatusNotifierItem(":999.1")` repeatedly with made-up names, planting
            // registry entries `NameOwnerChanged` can never clean up (it only fires for a
            // name that was ever real and then genuinely disconnects).
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            if unique_name != sender {
                return Err(RegistrationError::UniqueNameMismatch { claimed: unique_name, sender: sender.to_string() });
            }
            let unique_name = OwnedUniqueName::try_from(unique_name).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            Ok((unique_name, default_item_object_path()))
        }
        RegistrationTarget::WellKnownName { well_known_name } => {
            let dbus_proxy = zbus::fdo::DBusProxy::new(connection).await.map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            let well_known = WellKnownName::try_from(well_known_name).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let owner = dbus_proxy.get_name_owner(BusName::WellKnown(well_known)).await.map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            Ok((owner, default_item_object_path()))
        }
    }
}

fn default_item_object_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from(DEFAULT_ITEM_OBJECT_PATH).expect("DEFAULT_ITEM_OBJECT_PATH is a valid object path literal")
}

/// The leading `:` stripped from a `:N.M` unique name -- both the internal Lua-facing `id` and
/// the PNG spool filename component (ADR-0031). D-Bus guarantees a unique name contains only
/// digits/colons/dots, so no further sanitization is needed.
pub(super) fn sanitize_unique_name(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').to_string()
}


#[cfg(test)]
mod tests {
    use super::*;

    // ---- classify_service_arg ----

    #[test]
    fn classify_service_arg_recognizes_an_object_path() {
        assert_eq!(
            classify_service_arg("/org/example/StatusNotifierItem"),
            RegistrationTarget::ObjectPathFromSender { object_path: "/org/example/StatusNotifierItem".to_string() }
        );
    }

    #[test]
    fn classify_service_arg_recognizes_a_unique_name() {
        assert_eq!(classify_service_arg(":1.42"), RegistrationTarget::UniqueName { unique_name: ":1.42".to_string() });
    }

    #[test]
    fn classify_service_arg_recognizes_a_well_known_name() {
        assert_eq!(
            classify_service_arg("org.example.NmApplet"),
            RegistrationTarget::WellKnownName { well_known_name: "org.example.NmApplet".to_string() }
        );
    }


    // ---- sanitize_unique_name ----

    #[test]
    fn sanitize_unique_name_strips_the_leading_colon() {
        assert_eq!(sanitize_unique_name(":1.234"), "1.234");
    }

    #[test]
    fn sanitize_unique_name_is_a_no_op_without_a_leading_colon() {
        assert_eq!(sanitize_unique_name("1.234"), "1.234");
    }


    // ---- resolve_registration / register_status_notifier_item: a fabricated unique name
    //      must be rejected, since a connection can only ever truthfully claim its own real
    //      unique name (the bus-daemon-authenticated sender) ----

    use super::super::test_support::p2p_pair;

    #[tokio::test]
    async fn resolve_registration_accepts_a_unique_name_matching_the_real_sender() {
        let (connection, _peer) = p2p_pair().await;
        match resolve_registration(&connection, ":1.5", Some(":1.5")).await {
            Ok((unique_name, object_path)) => {
                assert_eq!(unique_name.as_str(), ":1.5");
                assert_eq!(object_path.as_str(), DEFAULT_ITEM_OBJECT_PATH);
            }
            Err(err) => panic!("a claimed unique name equal to the real sender must be accepted: {err}"),
        }
    }

    #[tokio::test]
    async fn resolve_registration_rejects_a_fabricated_unique_name() {
        let (connection, _peer) = p2p_pair().await;
        // The real sender is :1.5, but `service` claims a completely different,
        // never-connected unique name.
        match resolve_registration(&connection, ":999.1", Some(":1.5")).await {
            Err(RegistrationError::UniqueNameMismatch { claimed, sender }) => {
                assert_eq!(claimed, ":999.1");
                assert_eq!(sender, ":1.5");
            }
            other => panic!("a fabricated unique name not matching the real sender must be rejected as UniqueNameMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_registration_rejects_a_unique_name_with_no_sender_present() {
        let (connection, _peer) = p2p_pair().await;
        assert!(matches!(resolve_registration(&connection, ":1.5", None).await, Err(RegistrationError::NoSender)));
    }
}
