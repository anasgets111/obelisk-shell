//! `RegisterStatusNotifierItem`'s `service`-argument classification and resolution (ADR-0031).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use zbus::names::{BusName, OwnedBusName, OwnedUniqueName, WellKnownName};
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

/// One resolved `RegisterStatusNotifierItem` call: who the registry files the item under, and
/// where its messages are actually addressed.
///
/// Two names rather than one because they answer different questions and a Chromium tray item
/// answers only one of them (docs/adr/0072). `unique_name` is the identity: it is what
/// `NameOwnerChanged` reports on, what the registry keys by, and what the spool filename is built
/// from. `destination` is the address every `Get` and `GetLayout` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedRegistration {
    pub(super) unique_name: OwnedUniqueName,
    pub(super) destination: OwnedBusName,
    pub(super) object_path: OwnedObjectPath,
}

/// Resolves `RegisterStatusNotifierItem`'s `service` argument plus the calling message's sender
/// into a [`ResolvedRegistration`] (ADR-0031). The well-known-name branch is the only one that
/// performs I/O (`GetNameOwner`); the other two are resolved synchronously.
///
/// That branch keeps the well-known name as the `destination` rather than substituting the owner
/// it just looked up. Resolving to the owner is the textbook thing to do and it is what made Slack
/// invisible: its Chromium D-Bus code answers a property `Get` addressed to
/// `org.freedesktop.StatusNotifierItem-PID-1` and fails the identical `Get` addressed to the
/// unique name that owns it. The lookup still happens, because the identity half of the answer
/// needs it (docs/adr/0072).
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

/// The well-known branch's answer, split from the `GetNameOwner` that produces `owner` so the
/// part that broke Slack is testable without a bus.
///
/// `destination` is the well-known name and not `owner`. Substituting the owner is the textbook
/// resolution and it is what made Slack's item unreadable (docs/adr/0072): its Chromium D-Bus code
/// dispatches property reads on the message's destination field, answering
/// `org.freedesktop.StatusNotifierItem-PID-1` and failing the same read sent to `:1.N`, which owns
/// it. `owner` is still what comes back as `unique_name`, because identity is the other half of
/// the answer and only the owner can carry it.
fn well_known_registration(well_known: WellKnownName<'_>, owner: OwnedUniqueName) -> ResolvedRegistration {
    let destination = OwnedBusName::from(BusName::WellKnown(well_known.to_owned()));
    ResolvedRegistration { unique_name: owner, destination, object_path: default_item_object_path() }
}

fn default_item_object_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from(DEFAULT_ITEM_OBJECT_PATH)
        .expect("DEFAULT_ITEM_OBJECT_PATH is a valid object path literal")
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
        // The Slack case. Both halves matter and they are different names: the registry files the
        // item under `:1.659` so `NameOwnerChanged` can clean it up, and every `Get` goes to the
        // well-known name because that is the only one Chromium answers.
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
        // The real sender is :1.5, but `service` claims a completely different,
        // never-connected unique name.
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
