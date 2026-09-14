//! `obelisk.polkit`: polkitd's pending challenge and the dialog's one action (ADR-0114).
//!
//! `crate::polkit` owns D-Bus; this owns the config state and held reply. Like `lock`, `main.rs`
//! builds and pushes it because the bus callback, `secure_submit` frame, and PAM answer all land
//! in that loop.

use tokio::sync::oneshot;

use crate::polkit::{AgentError, BeginAuthenticationCall, first_unix_user_uid};

/// `obelisk.polkit`'s payload. All fields except `active` are empty while it is false.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct PolkitState {
    /// polkitd is waiting for the user; the remaining fields describe its request.
    pub active: bool,
    /// Translated text polkitd wants shown, such as "Authentication is required to ...".
    pub message: String,
    /// Action being authorized, e.g. `org.freedesktop.systemd1.manage-units`.
    pub action_id: String,
    /// Themed icon name, or empty when the caller set none.
    pub icon_name: String,
    /// A password is with PAM and unanswered. `pam_unix` takes about a second, so this drives a
    /// checking line; a second submit is refused while true.
    pub authenticating: bool,
    /// Drawable reason for the last failure, e.g. `"authentication failed"`. Empty until failure;
    /// the prompt stays open for another try and clears with it.
    pub error: String,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolkitAction {
    /// () Dismisses the prompt and tells polkitd's caller `Cancelled`.
    Cancel,
}

/// polkitd's pending `BeginAuthentication` call and its completing reply.
struct Pending {
    /// Only the cookie outlives `begin`. The display fields move into [`PolkitState`] and the
    /// identities are read for the uid and then dropped, so keeping the whole call would hold an
    /// authentication request's message and subject resident for the life of the challenge.
    cookie: String,
    uid: u32,
    reply: oneshot::Sender<Result<(), AgentError>>,
}

/// Effect of a PAM answer on the pending challenge.
pub enum Answer {
    /// Cookie no longer names the on-screen challenge: it was canceled or replaced during PAM.
    Stale,
    /// Recorded in `error`; the prompt stays open.
    Failed,
    /// The challenge is over; the helper told polkitd and `reply` ends the held call.
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

    /// polkitd's `BeginAuthentication`, admitting one at a time. A second request is answered
    /// `Cancelled`, not queued or shown, because one password field cannot serve two callers; the
    /// refused caller retries or reports the failure. A request without `unix-user` is also
    /// canceled; this agent takes the first identity
    /// (ADR-0028 left a picker for later). Returns whether the request became state.
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
            message: call.message,
            action_id: call.action_id,
            icon_name: call.icon_name,
            ..PolkitState::default()
        };
        self.pending = Some(Pending { cookie: call.cookie, uid, reply });
        true
    }

    /// Ends the challenge with `Cancelled`: dialog cancel (`None`) or polkitd's
    /// `CancelAuthentication` for the matching `Some(cookie)`. Returns whether one was open.
    pub fn cancel(&mut self, cookie: Option<&str>) -> bool {
        let Some(pending) = self.pending.take_if(|p| cookie.is_none_or(|c| c == p.cookie)) else {
            return false;
        };
        let _ = pending.reply.send(Err(AgentError::Cancelled));
        self.state = PolkitState::default();
        true
    }

    /// Admits and marks one PAM conversation, refusing when none is pending or one is already in
    /// flight; held Enter must not spawn a worker per repeat.
    pub fn try_begin_authentication(&mut self) -> Option<(u32, String)> {
        let pending = self.pending.as_ref().filter(|_| !self.state.authenticating)?;
        self.state.authenticating = true;
        Some((pending.uid, pending.cookie.clone()))
    }

    /// Applies the PAM answer for `cookie`. Success returns the reply that ends the held call; the
    /// controller has already forgotten the challenge.
    pub fn record_outcome(&mut self, cookie: &str, outcome: shared::PamOutcome) -> Answer {
        if !self.pending.as_ref().is_some_and(|p| p.cookie == cookie) {
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

/// `obelisk.polkit` action dispatch (ADR-0037). Returns whether state changed.
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
