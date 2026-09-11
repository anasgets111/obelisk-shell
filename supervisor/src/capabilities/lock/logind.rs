//! logind bridge for `obelisk.lock` (ADR-0138): `loginctl lock-session` in, `LockedHint` out.
//!
//! `loginctl lock-session` calls `org.freedesktop.login1.Manager.LockSession`, making logind emit
//! `Lock` on this session's object. Ignoring it breaks `systemd-lock-handler`,
//! `xdg-desktop-portal`, `swayidle -l`, and keybinds using `loginctl lock-session`; `systemctl`
//! manages units and has no `systemctl --user lock`.
//!
//! The reverse direction is `Session.SetLockedHint`, reported by `loginctl show-session` as
//! `LockedHint` and read by greeters/session scripts. Only the session owner may set it; this
//! process is that owner.
//!
//! `Unlock` is logged and dropped. ADR-0042 permits only successful PAM authentication to lift a
//! lock; wiring it to `LockController::unlock` would let any bus caller bypass PAM. The way back
//! in remains the password prompt or a VT switch.
//!
//! Both halves degrade to inert, logged once; a shell unable to reach logind still locks from its
//! own bar.

use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub(crate) trait Login1SessionLookup {
    /// `"auto"` resolves the caller's own session, used by every fallback below.
    #[zbus(name = "GetSession")]
    fn get_session(&self, session_id: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.freedesktop.login1.Session", default_service = "org.freedesktop.login1")]
pub(crate) trait Login1Session {
    /// `loginctl show-session`'s `LockedHint`; settable only by the session owner.
    #[zbus(name = "SetLockedHint")]
    fn set_locked_hint(&self, locked: bool) -> zbus::Result<()>;

    #[zbus(signal, name = "Lock")]
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(signal, name = "Unlock")]
    fn unlock(&self) -> zbus::Result<()>;
}

/// This process's logind session path. Prefer `$XDG_SESSION_ID`, then `"auto"`; they differ only
/// when the variable was inherited from elsewhere, and there the explicit session name wins.
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

/// logind session bridge. Holds the hint sender; both directions run as spawned tasks.
pub struct SessionBridge {
    /// `None` when the session is unresolved; hints then no-op.
    hints: Option<UnboundedSender<bool>>,
}

impl SessionBridge {
    /// Resolves the session, spawns the `Lock`/`Unlock` listener and `SetLockedHint` writer, then
    /// returns. `()` on `lock_requests` means logind requested a lock; `main.rs` decides.
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

    /// Tells logind whether the screen is locked. Fire-and-forget: other software reads
    /// `LockedHint`, but this shell's lock path does not depend on setting it.
    pub fn publish_locked_hint(&self, locked: bool) {
        if let Some(hints) = &self.hints {
            let _ = hints.send(locked);
        }
    }
}

/// Forwards each `Lock` signal as one request. Logs and drops `Unlock` (see the module doc).
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

/// Writes every hint through in order, so the newest wins. One task serializes changes; per-change
/// tasks could race and leave logind reporting the opposite of what is on the glass.
async fn publish_locked_hints(session: Login1SessionProxy<'static>, mut hints: UnboundedReceiver<bool>) {
    while let Some(locked) = hints.recv().await {
        if let Err(err) = session.set_locked_hint(locked).await {
            eprintln!("lock: failed to publish LockedHint={locked} to logind: {err}");
        }
    }
}
