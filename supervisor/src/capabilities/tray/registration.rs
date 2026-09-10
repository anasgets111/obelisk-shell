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
    UniqueName { unique_name: String, object_path: Option<String> },
    /// Well-known bus name; caller resolves it with `GetNameOwner`.
    WellKnownName { well_known_name: String, object_path: Option<String> },
}

/// The spec describes `service` as either a bus name or an object path, but Chromium sends **both,
/// concatenated**: Slack registers
/// `"org.freedesktop.StatusNotifierItem-677302-1/StatusNotifierItem/1"`. Read whole, that is not a
/// valid bus name, so the registration was rejected and Slack had no tray icon at all while every
/// other item worked.
///
/// Split at the first `/`, as Quickshell's `status_notifier/item.cpp` does: what precedes it is the
/// connection, what follows is the object path, and a `service` with no `/` keeps
/// [`DEFAULT_ITEM_OBJECT_PATH`]. A leading `/` is still the whole string as a path, because there is
/// no name in front of it to take.
fn classify_service_arg(service: &str) -> RegistrationTarget {
    if service.starts_with('/') {
        return RegistrationTarget::ObjectPathFromSender { object_path: service.to_string() };
    }
    let (name, object_path) =
        service.split_once('/').map_or((service, None), |(name, path)| (name, Some(format!("/{path}"))));
    if name.starts_with(':') {
        RegistrationTarget::UniqueName { unique_name: name.to_string(), object_path }
    } else {
        RegistrationTarget::WellKnownName { well_known_name: name.to_string(), object_path }
    }
}

/// The object path a branch carried, or the spec's default when `service` named none.
fn object_path_or_default(object_path: Option<String>) -> Result<OwnedObjectPath, RegistrationError> {
    match object_path {
        Some(path) => OwnedObjectPath::try_from(path).map_err(|err| RegistrationError::InvalidName(err.to_string())),
        None => Ok(default_item_object_path()),
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
/// Keep the well-known name as `destination`, not the looked-up owner (ADR-0072). Not because the
/// owner rejects reads -- it answers them -- but because a Chromium process holds several
/// connections and only the one owning the registered name exports the object; the others answer
/// "Object does not exist" at every path. Addressing the registered name lets the bus pick, so we
/// never have to be right about which connection it is (ADR-0168). The lookup still supplies
/// identity, which keys the registry and cleanup.
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
        RegistrationTarget::UniqueName { unique_name, object_path } => {
            // Reject made-up names: otherwise a peer could plant entries that `NameOwnerChanged`
            // can never clean up, since it only fires for names that were real.
            let sender = sender.ok_or(RegistrationError::NoSender)?;
            if unique_name != sender {
                return Err(RegistrationError::UniqueNameMismatch { claimed: unique_name, sender: sender.to_string() });
            }
            let unique_name = OwnedUniqueName::try_from(unique_name)
                .map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let destination = OwnedBusName::from(BusName::Unique(unique_name.clone().into()));
            Ok(ResolvedRegistration { unique_name, destination, object_path: object_path_or_default(object_path)? })
        }
        RegistrationTarget::WellKnownName { well_known_name, object_path } => {
            let dbus_proxy =
                zbus::fdo::DBusProxy::new(connection).await.map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            let well_known = WellKnownName::try_from(well_known_name)
                .map_err(|err| RegistrationError::InvalidName(err.to_string()))?;
            let owner = dbus_proxy
                .get_name_owner(BusName::WellKnown(well_known.clone()))
                .await
                .map_err(|err| RegistrationError::Dbus(err.to_string()))?;
            Ok(well_known_registration(well_known, owner, object_path_or_default(object_path)?))
        }
    }
}

/// Builds the well-known branch's answer separately from `GetNameOwner`, keeping Slack's case
/// testable without a bus. `destination` stays well-known because Chromium answers that name;
/// `owner` becomes `unique_name` for identity and cleanup (ADR-0072).
fn well_known_registration(
    well_known: WellKnownName<'_>,
    owner: OwnedUniqueName,
    object_path: OwnedObjectPath,
) -> ResolvedRegistration {
    let destination = OwnedBusName::from(BusName::WellKnown(well_known.to_owned()));
    ResolvedRegistration { unique_name: owner, destination, object_path }
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

/// One item's identity: its connection and the object path it exports, as KDE's
/// `StatusNotifierWatcher` composes `service + path` (ADR-0172).
///
/// The unique name alone is not an identity. A connection may export several items -- Chromium
/// numbers them `/StatusNotifierItem/1`, `/StatusNotifierItem/2` -- and each is a separate icon
/// with separate properties, while [`super::registry::ItemKey`] has always keyed them apart. Only
/// the string handed to config collapsed them, and a `list` given two rows with one key refuses the
/// whole tray.
pub(super) fn item_id(unique_name: &str, object_path: &str) -> String {
    format!("{}{}", sanitize_unique_name(unique_name), object_path)
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
        assert_eq!(
            classify_service_arg(":1.42"),
            RegistrationTarget::UniqueName { unique_name: ":1.42".to_string(), object_path: None }
        );
    }

    #[test]
    fn classify_service_arg_recognizes_a_well_known_name() {
        assert_eq!(
            classify_service_arg("org.example.NmApplet"),
            RegistrationTarget::WellKnownName {
                well_known_name: "org.example.NmApplet".to_string(),
                object_path: None
            }
        );
    }

    // ---- sanitize_unique_name ----

    #[test]
    fn chromium_sends_the_bus_name_and_object_path_concatenated() {
        // Verbatim from the log: Slack registered this string and the whole of it was read as a
        // bus name, so the registration was rejected and Slack had no tray icon.
        assert_eq!(
            classify_service_arg("org.freedesktop.StatusNotifierItem-677302-1/StatusNotifierItem/1"),
            RegistrationTarget::WellKnownName {
                well_known_name: "org.freedesktop.StatusNotifierItem-677302-1".to_string(),
                object_path: Some("/StatusNotifierItem/1".to_string()),
            }
        );
    }

    #[test]
    fn a_unique_name_may_carry_a_path_too() {
        assert_eq!(
            classify_service_arg(":1.42/StatusNotifierItem"),
            RegistrationTarget::UniqueName {
                unique_name: ":1.42".to_string(),
                object_path: Some("/StatusNotifierItem".to_string()),
            }
        );
    }

    #[test]
    fn a_leading_slash_is_still_a_bare_object_path() {
        // Nothing precedes the slash to take as a name, so the sender supplies the connection.
        assert_eq!(
            classify_service_arg("/StatusNotifierItem"),
            RegistrationTarget::ObjectPathFromSender { object_path: "/StatusNotifierItem".to_string() }
        );
    }

    #[test]
    fn sanitize_unique_name_strips_the_leading_colon() {
        assert_eq!(sanitize_unique_name(":1.234"), "1.234");
    }

    #[test]
    fn sanitize_unique_name_is_a_no_op_without_a_leading_colon() {
        assert_eq!(sanitize_unique_name("1.234"), "1.234");
    }

    /// A connection may export several items, and the unique name alone names all of them the
    /// same -- which is a duplicate key to a `list`, and a refused tray (ADR-0172).
    #[test]
    fn two_items_on_one_connection_get_two_ids() {
        assert_eq!(item_id(":1.42", "/StatusNotifierItem"), "1.42/StatusNotifierItem");
        assert_ne!(item_id(":1.42", "/StatusNotifierItem/1"), item_id(":1.42", "/StatusNotifierItem/2"));
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
        let resolved = well_known_registration(well_known, owner, default_item_object_path());
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
