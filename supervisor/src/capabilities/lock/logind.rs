//! The logind bridge for `oblisk.lock` (ADR-0138): `loginctl lock-session` in, `LockedHint` out.
//!
//! `loginctl lock-session` is the platform's way of asking whoever owns the screen to lock it. It
//! calls `org.freedesktop.login1.Manager.LockSession`, which makes logind emit a `Lock` signal on
//! this session's own object; a shell holding `ext_session_lock_v1` and ignoring that signal
//! breaks `systemd-lock-handler`, `xdg-desktop-portal`, `swayidle -l`, and every keybind anyone
//! has ever bound to `loginctl lock-session`. There is no `systemctl --user lock`: `systemctl`
//! manages units, and the lock request is a logind call.
//!
//! The reverse direction is `Session.SetLockedHint`, which is what `loginctl show-session` reports
//! as `LockedHint` and what a greeter or a session script reads to find out whether the screen is
//! locked. Only the session's own owner may set it, which this process is.
//!
//! `Unlock` is deliberately not honoured. ADR-0042 makes a successful PAM authentication the only
//! thing that may lift a lock, and `LockController::unlock` bypasses PAM entirely, so wiring the
//! signal to it would turn any caller who can reach the bus into an unlock. The signal is logged
//! and dropped; the way back in stays the password prompt or a VT switch.
//!
//! Both halves degrade to inert, logged once: a shell that cannot reach logind still locks from
//! its own bar.

use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub(crate) trait Login1SessionLookup {
    /// `"auto"` resolves the caller's own session, which is what every fallback below relies on.
    #[zbus(name = "GetSession")]
    fn get_session(&self, session_id: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.freedesktop.login1.Session", default_service = "org.freedesktop.login1")]
pub(crate) trait Login1Session {
    /// What `loginctl show-session` reports as `LockedHint`. Settable only by the session's owner.
    #[zbus(name = "SetLockedHint")]
    fn set_locked_hint(&self, locked: bool) -> zbus::Result<()>;

    #[zbus(signal, name = "Lock")]
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(signal, name = "Unlock")]
    fn unlock(&self) -> zbus::Result<()>;
}

/// The object path of this process's logind session. `$XDG_SESSION_ID` first because it names the
/// session exactly, then `"auto"`, which logind resolves against the caller: the two disagree only
/// on a machine where the variable was inherited from somewhere else, and there the explicit name
/// is the one to trust.
async fn resolve_session_path(connection: &zbus::Connection) -> Option<zbus::zvariant::OwnedObjectPath> {
    let manager = Login1SessionLookupProxy::new(connection).await.ok()?;
    let explicit = std::env::var("XDG_SESSION_ID").ok().filter(|id| !id.is_empty());
    if let Some(id) = explicit
        && let Ok(path) = manager.get_session(&id).await
    {
        return Some(path);
    }
    manager.get_session("auto").await.ok()
}

/// The logind session bridge. Holds only the hint sender; both directions run as spawned tasks.
pub struct SessionBridge {
    /// `None` when the session could not be resolved, which makes every hint a silent no-op.
    hints: Option<UnboundedSender<bool>>,
}

impl SessionBridge {
    /// Resolves the session, spawns the `Lock`/`Unlock` listener and the `SetLockedHint` writer,
    /// and returns. A `()` on `lock_requests` means logind asked for a lock and nothing more; the
    /// decision of what to do about it stays in `main.rs` beside every other lock decision.
    pub async fn new(connection: zbus::Connection, lock_requests: UnboundedSender<()>) -> Self {
        let Some(path) = resolve_session_path(&connection).await else {
            eprintln!(
                "lock: could not resolve this process's logind session; `loginctl lock-session` will not reach the \
                 shell and LockedHint will not be published"
            );
            return Self { hints: None };
        };
        let session = match Login1SessionProxy::builder(&connection).path(path.clone()) {
            Ok(builder) => match builder.build().await {
                Ok(session) => session,
                Err(err) => {
                    eprintln!(
                        "lock: failed to bind the logind session at {path}; lock-session will not reach us: {err}"
                    );
                    return Self { hints: None };
                }
            },
            Err(err) => {
                eprintln!("lock: logind handed back an unusable session path {path}: {err}");
                return Self { hints: None };
            }
        };

        tokio::spawn(forward_lock_signals(session.clone(), lock_requests));
        let (hints_tx, hints_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(publish_locked_hints(session, hints_rx));
        Self { hints: Some(hints_tx) }
    }

    /// Tells logind whether the screen is locked. Fire-and-forget: `LockedHint` is something other
    /// software reads, never something this shell's own behaviour depends on, so a failure to set
    /// it must not be in the path of taking the lock.
    pub fn publish_locked_hint(&self, locked: bool) {
        if let Some(hints) = &self.hints {
            let _ = hints.send(locked);
        }
    }
}

/// Forwards every `Lock` signal as one request. `Unlock` is logged and dropped; the module doc
/// says why.
async fn forward_lock_signals(session: Login1SessionProxy<'static>, lock_requests: UnboundedSender<()>) {
    let (locks, unlocks) = match (session.receive_lock().await, session.receive_unlock().await) {
        (Ok(locks), Ok(unlocks)) => (locks, unlocks),
        (locks, unlocks) => {
            let err = locks.err().or(unlocks.err());
            eprintln!(
                "lock: failed to subscribe to logind's Lock/Unlock signals; lock-session will not reach us: {err:?}"
            );
            return;
        }
    };
    let mut locks = locks.fuse();
    let mut unlocks = unlocks.fuse();
    loop {
        tokio::select! {
            signal = locks.next() => {
                if signal.is_none() {
                    break;
                }
                if lock_requests.send(()).is_err() {
                    break; // main is gone.
                }
            }
            signal = unlocks.next() => {
                if signal.is_none() {
                    break;
                }
                eprintln!(
                    "lock: logind asked for an unlock; refusing. Only a successful password authentication lifts a \
                     lock here (ADR-0042), so the way back in is the prompt or a VT switch"
                );
            }
        }
    }
}

/// Writes each hint through, newest wins. Serialized on one task rather than spawned per change:
/// two `SetLockedHint` calls in flight at once could land out of order and leave logind reporting
/// the opposite of what is on the glass.
async fn publish_locked_hints(session: Login1SessionProxy<'static>, mut hints: UnboundedReceiver<bool>) {
    while let Some(locked) = hints.recv().await {
        if let Err(err) = session.set_locked_hint(locked).await {
            eprintln!("lock: failed to publish LockedHint={locked} to logind: {err}");
        }
    }
}
