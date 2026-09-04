//! `oblisk.polkit`: the challenge polkitd is asking the user to answer, and the one action a
//! dialog has against it (ADR-0114).
//!
//! The D-Bus half is `crate::polkit`; this owns what a config reads and the reply that half is
//! holding open. Built in `main.rs` and pushed from its loop rather than from `Capabilities`, like
//! `lock`: its inputs (a bus callback, a `secure_submit` frame, a PAM answer) all land in that loop.

use tokio::sync::oneshot;

use crate::polkit::{AgentError, BeginAuthenticationCall, first_unix_user_uid};

/// `oblisk.polkit`'s payload. Everything but `active` is empty while it is false.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct PolkitState {
    /// polkitd is waiting on the user for the request the fields below describe.
    pub active: bool,
    /// What polkitd wants shown, already translated: "Authentication is required to ...".
    pub message: String,
    /// The action being authorised, e.g. `org.freedesktop.systemd1.manage-units`.
    pub action_id: String,
    /// A themed icon name for the action, or empty when the caller set none.
    pub icon_name: String,
    /// A password is with PAM and no answer has come back. `pam_unix` takes about a second, so this
    /// is what a "checking" line reads. A second submit is refused while it is true.
    pub authenticating: bool,
    /// Why the last attempt failed, in words fit to draw, e.g. `"authentication failed"`. Empty
    /// until an attempt fails; the prompt stays open for another try, and this clears with it.
    pub error: String,
}

/// Every action `oblisk.polkit:invoke(...)` accepts (ADR-0037). `cancel` dismisses the prompt and
/// tells polkitd's caller `Cancelled`. No doc comment on the variant: schemars would render one as
/// `oneOf` rather than the bare `enum` the stub generator reads action names from.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolkitAction {
    Cancel,
}

/// The `BeginAuthentication` call polkitd is waiting on, with the reply that ends it.
struct Pending {
    call: BeginAuthenticationCall,
    uid: u32,
    reply: oneshot::Sender<Result<(), AgentError>>,
}

/// What a PAM answer did to the pending challenge.
pub enum Answer {
    /// The cookie is not the challenge on screen: cancelled or replaced while PAM was busy.
    Stale,
    /// Recorded in `error`; the prompt stays open.
    Failed,
    /// The challenge is over and polkitd has been told by the helper; `reply` ends the held call.
    Succeeded { reply: oneshot::Sender<Result<(), AgentError>> },
}

#[derive(Default)]
pub struct PolkitController {
    state: PolkitState,
    pending: Option<Pending>,
}

impl PolkitController {
    pub fn snapshot(&self) -> PolkitState {
        self.state.clone()
    }

    /// polkitd's `BeginAuthentication`. One at a time: a second request while one is open is
    /// answered `Cancelled` on the spot rather than queued or shown, since one password field
    /// cannot be typing for two callers, and the refused caller either retries or reports the
    /// failure itself. So is a request naming no `unix-user` identity, which this agent cannot
    /// authenticate (it takes the first such identity; ADR-0028 left a picker for later). Returns
    /// whether the request became the state.
    pub fn begin(&mut self, call: BeginAuthenticationCall, reply: oneshot::Sender<Result<(), AgentError>>) -> bool {
        let Some(uid) = first_unix_user_uid(&call.identities) else {
            eprintln!("polkit: challenge {:?} carried no unix-user identity; cancelling it", call.cookie);
            let _ = reply.send(Err(AgentError::Cancelled));
            return false;
        };
        if self.pending.is_some() {
            eprintln!("polkit: challenge {:?} arrived while another is on screen; cancelling it", call.cookie);
            let _ = reply.send(Err(AgentError::Cancelled));
            return false;
        }
        self.state = PolkitState {
            active: true,
            message: call.message.clone(),
            action_id: call.action_id.clone(),
            icon_name: call.icon_name.clone(),
            ..PolkitState::default()
        };
        self.pending = Some(Pending { call, uid, reply });
        true
    }

    /// Ends the pending challenge with `Cancelled`: the dialog's own cancel (`None`), or polkitd's
    /// `CancelAuthentication` for `Some(cookie)`, which only applies to the challenge it names.
    /// Returns whether anything was open to cancel.
    pub fn cancel(&mut self, cookie: Option<&str>) -> bool {
        let matches = self.pending.as_ref().is_some_and(|p| cookie.is_none_or(|c| c == p.call.cookie));
        if !matches {
            return false;
        }
        if let Some(pending) = self.pending.take() {
            let _ = pending.reply.send(Err(AgentError::Cancelled));
        }
        self.state = PolkitState::default();
        true
    }

    /// Admits one PAM conversation for the pending challenge and marks it started, or refuses:
    /// nothing pending, or one already in flight (a held Enter must not spawn a worker per repeat).
    pub fn try_begin_authentication(&mut self) -> Option<(u32, String)> {
        let pending = self.pending.as_ref().filter(|_| !self.state.authenticating)?;
        self.state.authenticating = true;
        Some((pending.uid, pending.call.cookie.clone()))
    }

    /// Applies the PAM answer for `cookie`. Success hands back the reply to end the held call with;
    /// the controller has already forgotten the challenge.
    pub fn record_outcome(&mut self, cookie: &str, outcome: shared::PamOutcome) -> Answer {
        if !self.pending.as_ref().is_some_and(|p| p.call.cookie == cookie) {
            return Answer::Stale;
        }
        self.state.authenticating = false;
        if outcome != shared::PamOutcome::Success {
            self.state.error = super::lock::error_for_outcome(&outcome);
            return Answer::Failed;
        }
        let pending = self.pending.take().expect("checked above");
        self.state = PolkitState::default();
        Answer::Succeeded { reply: pending.reply }
    }
}

/// `oblisk.polkit`'s action dispatch (ADR-0037). Returns whether the state changed.
pub fn dispatch(controller: &mut PolkitController, envelope: &shared::CommandEnvelope) -> bool {
    let Some(action) = crate::parse_action::<PolkitAction>(&envelope.params) else { return false };
    match action {
        PolkitAction::Cancel => controller.cancel(None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Value};

    use super::*;

    fn challenge(cookie: &str) -> BeginAuthenticationCall {
        let uid = HashMap::from([("uid".to_string(), OwnedValue::try_from(Value::from(1000u32)).unwrap())]);
        BeginAuthenticationCall {
            action_id: "org.test.act".into(),
            message: "Authenticate to test".into(),
            icon_name: "dialog-password".into(),
            details: HashMap::new(),
            cookie: cookie.into(),
            identities: vec![("unix-user".to_string(), uid)],
        }
    }

    fn begun(controller: &mut PolkitController, cookie: &str) -> oneshot::Receiver<Result<(), AgentError>> {
        let (reply, answer) = oneshot::channel();
        controller.begin(challenge(cookie), reply);
        answer
    }

    #[test]
    fn a_challenge_becomes_the_state_and_a_second_one_is_refused() {
        let mut c = PolkitController::default();
        let mut first = begun(&mut c, "a");
        assert!(c.snapshot().active);
        assert_eq!(c.snapshot().message, "Authenticate to test");
        let mut second = begun(&mut c, "b");
        assert!(matches!(second.try_recv(), Ok(Err(AgentError::Cancelled))));
        assert!(first.try_recv().is_err(), "the first is still waiting on the user");
    }

    #[test]
    fn a_failed_password_keeps_the_prompt_open_with_the_reason() {
        let mut c = PolkitController::default();
        let mut answer = begun(&mut c, "a");
        let (uid, cookie) = c.try_begin_authentication().expect("one attempt is admitted");
        assert_eq!(uid, 1000);
        assert!(c.try_begin_authentication().is_none(), "not two at once");
        assert!(matches!(c.record_outcome(&cookie, shared::PamOutcome::AuthFailed), Answer::Failed));
        let state = c.snapshot();
        assert!(state.active && !state.authenticating);
        assert_eq!(state.error, "authentication failed");
        assert!(answer.try_recv().is_err(), "polkitd is still waiting");
    }

    #[test]
    fn success_hands_the_challenge_back_and_clears_the_state() {
        let mut c = PolkitController::default();
        let _answer = begun(&mut c, "a");
        let (_, cookie) = c.try_begin_authentication().unwrap();
        assert!(matches!(c.record_outcome(&cookie, shared::PamOutcome::Success), Answer::Succeeded { .. }));
        assert_eq!(c.snapshot(), PolkitState::default());
    }

    #[test]
    fn cancel_answers_polkitd_and_a_late_pam_answer_is_stale() {
        let mut c = PolkitController::default();
        let mut answer = begun(&mut c, "a");
        let (_, cookie) = c.try_begin_authentication().unwrap();
        assert!(!c.cancel(Some("other")), "polkitd cancelling a different cookie changes nothing");
        assert!(c.cancel(None));
        assert!(matches!(answer.try_recv(), Ok(Err(AgentError::Cancelled))));
        assert!(!c.snapshot().active);
        assert!(matches!(c.record_outcome(&cookie, shared::PamOutcome::Success), Answer::Stale));
        assert!(!c.cancel(None), "nothing left to cancel");
    }
}
