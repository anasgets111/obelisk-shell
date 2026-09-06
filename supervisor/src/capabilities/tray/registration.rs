//! `RegisterStatusNotifierItem`'s `service`-argument classification and resolution (ADR-0031).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use zbus::names::{BusName, OwnedBusName, OwnedUniqueName, WellKnownName};
use zbus::zvariant::OwnedObjectPath;

use super::DEFAULT_ITEM_OBJECT_PATH;

/// Classifies `RegisterStatusNotifierItem`'s `service` before D-Bus I/O (ADR-0031); every `&str`
/// lands in one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistrationTarget {
    /// Object path (`service.starts_with('/')`); the bus name comes from the message sender.
    ObjectPathFromSender { object_path: String },
    /// Existing unique name (`:N.M`), used directly.
    UniqueName { unique_name: String },
    /// Well-known bus name; caller resolves it with `GetNameOwner`.
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
    /// Claimed unique name (`:N.M`) differs from the authenticated sender. A connection can only
    /// claim its own bus-daemon-filled, unspoofable unique name.
    UniqueNameMismatch {
        claimed: String,
        sender: String,
    },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSender => {
                write!(f, "an object-path service argument requires a message sender, but none was present")
            }
            Self::InvalidName(err) => write!(f, "invalid D-Bus name: {err}"),
            Self::Dbus(err) => write!(f, "{err}"),
            Self::UniqueNameMismatch { claimed, sender } => {
                write!(f, "claimed unique name {claimed:?} does not match the real sender {sender:?}")
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

/// Resolved registration identity and message address. They differ because Chromium answers only
/// one name (ADR-0072): `unique_name` is the identity reported by `NameOwnerChanged`; it keys the
/// registry, cleanup, and spool filename. `destination` addresses every `Get` and `GetLayout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedRegistration {
    pub(super) unique_name: OwnedUniqueName,
    pub(super) destination: OwnedBusName,
    pub(super) object_path: OwnedObjectPath,
}

/// Resolves `service` plus the message sender into [`ResolvedRegistration`] (ADR-0031). Only the
/// well-known branch performs `GetNameOwner` I/O.
///
/// Keep the well-known name as `destination`, not the looked-up owner. Slack's Chromium code
/// answers `Get` at `org.freedesktop.StatusNotifierItem-PID-1` but fails the same `Get` at its
/// owner unique name, making Slack invisible (ADR-0072). The lookup still supplies identity.
pub(super) async fn resolve_registration(
    connection: &zbus::Connection,
    service: &str,
    sender: Option<&str>,
) -> Result<ResolvedRegistration, RegistrationError> {
    match classify_service_arg(service) {
        RegistrationTarget::ObjectPathFromSender { object_path } => {
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            let unique_name =
                OwnedUniqueName::try_from(sender).map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let object_path = OwnedObjectPath::try_from(object_path)
                .map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let destination = OwnedBusName::from(BusName::Unique(unique_name.clone().into()));
            Ok(ResolvedRegistration { unique_name, destination, object_path })
        }
        RegistrationTarget::UniqueName { unique_name } => {
            // Reject made-up names: otherwise a peer could plant entries that `NameOwnerChanged`
            // can never clean up, since it only fires for names that were real.
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            if unique_name != sender {
                return Err(RegistrationError::UniqueNameMismatch { claimed: unique_name, sender: sender.to_string() });
            }
            let unique_name = OwnedUniqueName::try_from(unique_name)
                .map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let destination = OwnedBusName::from(BusName::Unique(unique_name.clone().into()));
            Ok(ResolvedRegistration { unique_name, destination, object_path: default_item_object_path() })
        }
        RegistrationTarget::WellKnownName { well_known_name } => {
            let dbus_proxy =
                zbus::fdo::DBusProxy::new(connection).await.map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            let well_known = WellKnownName::try_from(well_known_name)
                .map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let owner = dbus_proxy
                .get_name_owner(BusName::WellKnown(well_known.clone()))
                .await
                .map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            Ok(well_known_registration(well_known, owner))
        }
    }
}

/// Builds the well-known branch's answer separately from `GetNameOwner`, keeping Slack's case
/// testable without a bus. `destination` stays well-known because Chromium answers that name;
/// `owner` becomes `unique_name` for identity and cleanup (ADR-0072).
fn well_known_registration(well_known: WellKnownName<'_>, owner: OwnedUniqueName) -> ResolvedRegistration {
    let destination = OwnedBusName::from(BusName::WellKnown(well_known.to_owned()));
    ResolvedRegistration { unique_name: owner, destination, object_path: default_item_object_path() }
}

fn default_item_object_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from(DEFAULT_ITEM_OBJECT_PATH)
        .expect("DEFAULT_ITEM_OBJECT_PATH is a valid object path literal")
}

/// Strips the leading `:` from `:N.M` for Lua `id` and PNG filenames (ADR-0031). D-Bus guarantees
/// unique names contain only digits, colons, and dots.
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

    // ---- fabricated unique names must be rejected ----

    use crate::capabilities::test_support::p2p_pair;

    #[tokio::test]
    async fn resolve_registration_accepts_a_unique_name_matching_the_real_sender() {
        let (connection, _peer) = p2p_pair().await;
        match resolve_registration(&connection, ":1.5", Some(":1.5")).await {
            Ok(resolved) => {
                assert_eq!(resolved.unique_name.as_str(), ":1.5");
                assert_eq!(resolved.object_path.as_str(), DEFAULT_ITEM_OBJECT_PATH);
                assert_eq!(
                    resolved.destination.as_str(),
                    ":1.5",
                    "a sender that named itself has no other name to be addressed by"
                );
            }
            Err(err) => panic!("a claimed unique name equal to the real sender must be accepted: {err}"),
        }
    }

    #[test]
    fn a_well_known_registration_is_addressed_by_the_name_it_registered_not_its_owner() {
        // Slack's identity is `:1.659` for cleanup; Chromium receives every `Get` at the
        // well-known name.
        let well_known = WellKnownName::try_from("org.freedesktop.StatusNotifierItem-1240273-1").unwrap();
        let owner = OwnedUniqueName::try_from(":1.659").unwrap();
        let resolved = well_known_registration(well_known, owner);
        assert_eq!(resolved.unique_name.as_str(), ":1.659", "identity is the owner");
        assert_eq!(
            resolved.destination.as_str(),
            "org.freedesktop.StatusNotifierItem-1240273-1",
            "and the address is the name it registered under"
        );
        assert_eq!(resolved.object_path.as_str(), DEFAULT_ITEM_OBJECT_PATH);
    }

    #[tokio::test]
    async fn resolve_registration_rejects_a_fabricated_unique_name() {
        let (connection, _peer) = p2p_pair().await;
        // The real sender is :1.5; `service` claims never-connected :999.1.
        match resolve_registration(&connection, ":999.1", Some(":1.5")).await {
            Err(RegistrationError::UniqueNameMismatch { claimed, sender }) => {
                assert_eq!(claimed, ":999.1");
                assert_eq!(sender, ":1.5");
            }
            other => panic!(
                "a fabricated unique name not matching the real sender must be rejected as UniqueNameMismatch, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn resolve_registration_rejects_a_unique_name_with_no_sender_present() {
        let (connection, _peer) = p2p_pair().await;
        assert!(matches!(resolve_registration(&connection, ":1.5", None).await, Err(RegistrationError::NoSender)));
    }
}
