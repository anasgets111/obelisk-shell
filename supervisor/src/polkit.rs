//! Polkit authentication agent registration handshake.
//!
//! The real D-Bus method (verified against polkit's own source and introspection XML) is
//! `RegisterAuthenticationAgent(subject: (sa{sv}), locale: s, object_path: s) -> ()`, not
//! `RegisterAgent`. The call-out uses `zbus_polkit`'s `Authority`
//! proxy and `Subject` type directly rather than hand-deriving matching zvariant types
//! (ADR-0013).
//!
//! The agent side, `org.freedesktop.PolicyKit1.AuthenticationAgent`, has no maintained crate,
//! so `AuthenticationAgent` below is hand-written against the verified signature:
//! `BeginAuthentication(action_id: s, message: s, icon_name: s, details: a{ss}, cookie: s,
//! identities: a(sa{sv})) -> ()` and `CancelAuthentication(cookie: s) -> ()`.

use std::collections::HashMap;

use tokio::sync::mpsc::UnboundedSender;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};
pub use zbus_polkit::policykit1::{AuthorityProxy, Subject};

/// Object path this agent is exported at on our own unique connection name. Any path under
/// our control is valid -- the spec's `object_path` argument is caller-chosen, not fixed.
pub const AGENT_OBJECT_PATH: &str = "/org/oblisk/PolicyKit1/AuthenticationAgent";

/// Builds the `unix-session` `Subject` for the session this process is running in.
///
/// ponytail: resolves the session id from `$XDG_SESSION_ID` rather than the general-purpose
/// route (asking logind's `Manager.GetSessionByPID` for this process's own pid). systemd's
/// pam_systemd sets `$XDG_SESSION_ID` for every session it opens, which covers every real
/// graphical login this supervisor runs under; the logind round-trip is the upgrade path if
/// this ever needs to run somewhere pam_systemd doesn't apply, or once a logind client
/// exists in this crate for an unrelated reason (it doesn't yet -- ADR-0010 covers Wayland
/// idle/lock, not logind sessions).
pub fn current_session_subject() -> Result<Subject, std::env::VarError> {
    Ok(session_subject(std::env::var("XDG_SESSION_ID")?))
}

/// The `Subject` half of [`current_session_subject`], split off the `$XDG_SESSION_ID` read so a
/// test can check the shape without setting the variable. `set_var` races every other thread in
/// the test binary that reads the environment, which is why Rust 2024 made it `unsafe`.
fn session_subject(session_id: String) -> Subject {
    let mut subject_details = HashMap::new();
    subject_details.insert(
        "session-id".to_string(),
        OwnedValue::try_from(Value::from(session_id)).expect("String -> OwnedValue conversion is infallible"),
    );
    Subject { subject_kind: "unix-session".to_string(), subject_details }
}

/// One `BeginAuthentication` call as polkitd sent it, parsed off the wire.
///
/// `Eq` is deliberately not derived: `identities`' `OwnedValue` can hold a
/// `zvariant::Value::F64`, and `f64` only implements `PartialEq`, not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct BeginAuthenticationCall {
    pub action_id: String,
    pub message: String,
    pub icon_name: String,
    pub details: HashMap<String, String>,
    pub cookie: String,
    pub identities: Vec<(String, HashMap<String, OwnedValue>)>,
}

/// The uid to authenticate as and to report back via `AuthenticationAgentResponse2`, parsed
/// from `BeginAuthentication`'s `identities` list: a `unix-user` identity carries its uid
/// under the `"uid"` key, typed `uint32`.
///
/// ponytail: takes the *first* `unix-user` identity in the list, not all of them. polkitd can
/// list multiple identities that could satisfy an action (e.g. every member of `wheel`) --
/// picking one to authenticate as is normally a user-facing choice (an identity picker), which
/// doesn't exist here: this codebase's only `secure_submit` UI is a single password field with
/// no picker. First-match is the simplest correct behavior until a picker exists to make the
/// choice meaningful; see this ADR's upgrade path (ADR-0028) for where that UI would need
/// to attach.
pub fn first_unix_user_uid(identities: &[(String, HashMap<String, OwnedValue>)]) -> Option<u32> {
    identities
        .iter()
        .find(|(kind, _)| kind == "unix-user")
        .and_then(|(_, details)| details.get("uid"))
        .and_then(|v| u32::try_from(v.clone()).ok())
}

/// `org.freedesktop.PolicyKit1.AuthenticationAgent`, the interface polkitd calls back into
/// once this process registers via [`register_agent`].
///
/// ponytail: `begin_authentication` only forwards the parsed challenge over a channel; it
/// does not drive a PAM conversation or push the challenge to the Renderer over IPC. Neither
/// exists yet in this dependency tree (no PAM crate) or codebase (no IPC socket server, no
/// `textfield` scene node) to hand the challenge to. See ADR-0015 for the real flow this
/// stands in for and what unblocks it.
pub struct AuthenticationAgent {
    challenges: UnboundedSender<BeginAuthenticationCall>,
}

impl AuthenticationAgent {
    pub fn new(challenges: UnboundedSender<BeginAuthenticationCall>) -> Self {
        Self { challenges }
    }
}

#[interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl AuthenticationAgent {
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) {
        // A dropped receiver just means nobody is listening (e.g. mid-shutdown); not a reason to fail the D-Bus call.
        let _ = self.challenges.send(BeginAuthenticationCall {
            action_id,
            message,
            icon_name,
            details,
            cookie,
            identities,
        });
    }

    async fn cancel_authentication(&self, _cookie: String) {
        // ponytail: nothing is tracking in-flight challenges yet to cancel -- see the struct
        // doc comment. A real implementation cancels the matching PAM conversation.
    }
}

/// The authentication agent, held unregistered until a config declares a `secure_submit` that
/// names polkit (ADR-0070 decisions 5 and 6).
///
/// A registration failure is logged, not propagated with `?`. "An authentication agent already
/// exists for the given subject" is the normal answer on a machine running any other desktop, and
/// it must not stop the shell from starting.
pub struct PolkitAgent {
    /// Taken by the first [`Self::register`] call, so a second is a no-op rather than a second
    /// `RegisterAuthenticationAgent` for the same subject.
    agent: Option<AuthenticationAgent>,
}

impl PolkitAgent {
    pub fn new(challenges: UnboundedSender<BeginAuthenticationCall>) -> Self {
        PolkitAgent { agent: Some(AuthenticationAgent::new(challenges)) }
    }

    /// Registers with polkitd, once. Every failure logs and leaves this process without an agent,
    /// which costs it the challenges it would have been asked to answer and nothing else.
    pub async fn register(&mut self, connection: &zbus::Connection) {
        match current_session_subject() {
            Ok(subject) => self.register_for(connection, &subject).await,
            Err(err) => {
                eprintln!(
                    "polkit: $XDG_SESSION_ID names no session to register an agent for; agent disabled for this run: {err}"
                );
                // Dropped rather than left for a later call to retry: `$XDG_SESSION_ID` will not
                // appear mid-run, so a second attempt would fail the same way.
                self.agent = None;
            }
        }
    }

    /// [`Self::register`] once the subject is known, holding the take-once rule.
    ///
    /// Split so the test can drive the real path twice without `set_var`: `register` resolves the
    /// subject from `$XDG_SESSION_ID`, and `setenv` rewrites the process-wide `environ` block,
    /// racing every concurrent `getenv` in the test binary whatever variable either one names.
    async fn register_for(&mut self, connection: &zbus::Connection, subject: &Subject) {
        let Some(agent) = self.agent.take() else {
            return;
        };
        match register_agent(connection, agent, subject, "en_US.UTF-8", AGENT_OBJECT_PATH).await {
            Ok(()) => eprintln!("polkit: registered as this session's authentication agent"),
            Err(err) => eprintln!(
                "polkit: RegisterAuthenticationAgent failed, so another agent answers this session; disabled for this run: {err}"
            ),
        }
    }
}

/// Registers `agent` as the polkit authentication agent for `subject`/`locale`. Exports
/// `agent` on `connection`'s object server *before* calling `RegisterAuthenticationAgent`, so
/// a callback arriving right after registration succeeds always finds a live object.
pub async fn register_agent(
    connection: &zbus::Connection,
    agent: AuthenticationAgent,
    subject: &Subject,
    locale: &str,
    object_path: &str,
) -> zbus::Result<()> {
    connection.object_server().at(object_path, agent).await?;
    let authority = AuthorityProxy::new(connection).await?;
    authority.register_authentication_agent(subject, locale, object_path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    /// A stand-in for polkitd's own `org.freedesktop.PolicyKit1.Authority` object, exported
    /// on the peer end of a p2p connection so `register_agent`'s real wire call can be
    /// exercised without a live system bus.
    struct MockAuthority {
        calls: mpsc::UnboundedSender<(Subject, String, String)>,
    }

    #[interface(name = "org.freedesktop.PolicyKit1.Authority")]
    impl MockAuthority {
        async fn register_authentication_agent(&self, subject: Subject, locale: String, object_path: String) {
            let _ = self.calls.send((subject, locale, object_path));
        }
    }

    /// A connected pair of p2p zbus connections, no bus daemon involved. Mirrors zbus's own
    /// `tests/e2e.rs` (`iface_and_proxy_unix_p2p`) -- crucially, building both ends concurrently
    /// via `try_join!`: the SASL handshake needs both peers reading and writing at once.
    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder =
            zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    fn test_subject() -> Subject {
        let mut subject_details = HashMap::new();
        subject_details.insert("session-id".to_string(), OwnedValue::try_from(Value::from("c1")).unwrap());
        Subject { subject_kind: "unix-session".to_string(), subject_details }
    }

    #[tokio::test]
    async fn register_agent_sends_register_authentication_agent_with_the_right_args() {
        let (authority_side, agent_side) = p2p_pair().await;
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        authority_side
            .object_server()
            .at("/org/freedesktop/PolicyKit1/Authority", MockAuthority { calls: calls_tx })
            .await
            .expect("failed to export the mock Authority");

        let (challenges_tx, _challenges_rx) = mpsc::unbounded_channel();
        let agent = AuthenticationAgent::new(challenges_tx);
        let subject = test_subject();

        register_agent(&agent_side, agent, &subject, "en_US.UTF-8", AGENT_OBJECT_PATH)
            .await
            .expect("registration against the mock Authority should succeed");

        let (received_subject, locale, object_path) =
            calls_rx.recv().await.expect("mock Authority never received RegisterAuthenticationAgent");
        assert_eq!(received_subject.subject_kind, "unix-session");
        assert_eq!(
            received_subject.subject_details.get("session-id").cloned().and_then(|v| String::try_from(v).ok()),
            Some("c1".to_string())
        );
        assert_eq!(locale, "en_US.UTF-8");
        assert_eq!(object_path, AGENT_OBJECT_PATH);
    }

    /// Registering twice would ask polkitd for a second agent on one subject. The second call is
    /// reachable because every generation sends its own starts (ADR-0070 decision 3).
    #[tokio::test]
    async fn registering_twice_makes_only_one_wire_call() {
        let (authority_side, agent_side) = p2p_pair().await;
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        authority_side
            .object_server()
            .at("/org/freedesktop/PolicyKit1/Authority", MockAuthority { calls: calls_tx })
            .await
            .expect("failed to export the mock Authority");
        let (challenges_tx, _challenges_rx) = mpsc::unbounded_channel();
        let mut agent = PolkitAgent::new(challenges_tx);

        let subject = test_subject();
        agent.register_for(&agent_side, &subject).await;
        agent.register_for(&agent_side, &subject).await;

        calls_rx.recv().await.expect("the first register must reach the Authority");
        assert!(calls_rx.try_recv().is_err(), "the second register must be a no-op");
    }

    /// Was `current_session_subject_reads_xdg_session_id`, which set `$XDG_SESSION_ID` and read it
    /// back through [`current_session_subject`]. Its safety comment claimed no other test in the
    /// binary touched that variable; `registering_twice_makes_only_one_wire_call` sets it to `c1`,
    /// and the harness runs both on parallel threads, so the read-back saw `c1` whenever it lost.
    /// Testing [`session_subject`] instead keeps the assertion and needs no environment at all.
    #[test]
    fn a_session_id_becomes_a_unix_session_subject() {
        let subject = session_subject("test-session-42".to_string());
        assert_eq!(subject.subject_kind, "unix-session");
        assert_eq!(
            subject.subject_details.get("session-id").cloned().and_then(|v| String::try_from(v).ok()),
            Some("test-session-42".to_string())
        );
    }

    #[tokio::test]
    async fn begin_authentication_forwards_the_parsed_challenge() {
        let (agent_side, caller_side) = p2p_pair().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        agent_side
            .object_server()
            .at(AGENT_OBJECT_PATH, AuthenticationAgent::new(tx))
            .await
            .expect("failed to export AuthenticationAgent");

        let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&caller_side)
            .destination("org.oblisk.Supervisor")
            .expect("valid destination bus name")
            .path(AGENT_OBJECT_PATH)
            .expect("valid object path")
            .interface("org.freedesktop.PolicyKit1.AuthenticationAgent")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p proxy to the agent");

        let details: HashMap<String, String> =
            HashMap::from([("polkit.gettext_domain".to_string(), "polkit".to_string())]);
        let identity_details: HashMap<String, OwnedValue> =
            HashMap::from([("uid".to_string(), OwnedValue::try_from(Value::from(1000u32)).unwrap())]);
        let identities: Vec<(String, HashMap<String, OwnedValue>)> = vec![("unix-user".to_string(), identity_details)];
        proxy
            .call_method(
                "BeginAuthentication",
                &(
                    "org.oblisk.test.action",
                    "Authenticate to do the thing",
                    "dialog-password",
                    details,
                    "cookie-123",
                    identities,
                ),
            )
            .await
            .expect("BeginAuthentication call should succeed");

        let received = rx.recv().await.expect("BeginAuthentication was never forwarded over the channel");
        assert_eq!(received.action_id, "org.oblisk.test.action");
        assert_eq!(received.message, "Authenticate to do the thing");
        assert_eq!(received.icon_name, "dialog-password");
        assert_eq!(received.cookie, "cookie-123");
        assert_eq!(received.details.get("polkit.gettext_domain").map(String::as_str), Some("polkit"));
        assert_eq!(
            first_unix_user_uid(&received.identities),
            Some(1000),
            "identities must be forwarded, not discarded"
        );
    }

    #[tokio::test]
    async fn cancel_authentication_dispatches_without_error() {
        let (agent_side, caller_side) = p2p_pair().await;
        let (tx, _rx) = mpsc::unbounded_channel();
        agent_side
            .object_server()
            .at(AGENT_OBJECT_PATH, AuthenticationAgent::new(tx))
            .await
            .expect("failed to export AuthenticationAgent");

        let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&caller_side)
            .destination("org.oblisk.Supervisor")
            .expect("valid destination bus name")
            .path(AGENT_OBJECT_PATH)
            .expect("valid object path")
            .interface("org.freedesktop.PolicyKit1.AuthenticationAgent")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p proxy to the agent");

        proxy
            .call_method("CancelAuthentication", &("cookie-123",))
            .await
            .expect("CancelAuthentication call should succeed");
    }

    fn unix_user_identity(uid: u32) -> (String, HashMap<String, OwnedValue>) {
        ("unix-user".to_string(), HashMap::from([("uid".to_string(), OwnedValue::try_from(Value::from(uid)).unwrap())]))
    }

    #[test]
    fn first_unix_user_uid_is_none_for_an_empty_list() {
        assert_eq!(first_unix_user_uid(&[]), None);
    }

    #[test]
    fn first_unix_user_uid_is_none_when_only_a_unix_group_is_present() {
        let group = (
            "unix-group".to_string(),
            HashMap::from([("gid".to_string(), OwnedValue::try_from(Value::from(100u32)).unwrap())]),
        );
        assert_eq!(first_unix_user_uid(&[group]), None);
    }

    #[test]
    fn first_unix_user_uid_returns_the_uid_of_a_unix_user_identity() {
        assert_eq!(first_unix_user_uid(&[unix_user_identity(1000)]), Some(1000));
    }

    #[test]
    fn first_unix_user_uid_is_none_when_the_unix_user_entry_is_missing_the_uid_key() {
        let malformed = ("unix-user".to_string(), HashMap::new());
        assert_eq!(first_unix_user_uid(&[malformed]), None);
    }

    #[test]
    fn first_unix_user_uid_returns_the_first_unix_user_entrys_uid_when_multiple_are_present() {
        let identities = [unix_user_identity(1000), unix_user_identity(2000)];
        assert_eq!(first_unix_user_uid(&identities), Some(1000), "must take the first identity, not just any of them");
    }
}
