//! Inhibit half of `oblisk.idle` (ADR-0032): `org.freedesktop.login1.Manager.Inhibit` on the
//! existing system-bus connection -- per-generation refcount arithmetic, the hand-written
//! Login1Manager proxy, and the shared inhibit-fd/refcount state. Split from `dbus::idle` --
//! see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;

/// What one refcount transition means for the shared inhibit fd: open it (0->1) or close it
/// (->0), never both -- [`apply_inhibit`] only reports `should_open_fd`, the others only `should_close_fd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InhibitTransition {
    pub should_open_fd: bool,
    pub should_close_fd: bool,
}

fn total(counts: &HashMap<u32, u32>) -> u32 {
    counts.values().sum()
}

/// `idle:inhibit(reason)`'s refcount half: increments `generation_id`'s own count.
/// `should_open_fd` is true exactly when the *global* total was zero before this call
/// (ADR-0032's "opens the fd on 0->1").
pub fn apply_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    *counts.entry(generation_id).or_insert(0) += 1;
    InhibitTransition { should_open_fd: total_before == 0, should_close_fd: false }
}

/// `idle:release_inhibit()`'s refcount half: decrements `generation_id`'s own count.
/// `should_close_fd` is true exactly when the global total drops to zero because of this call.
/// Releasing an already-zero generation is a silent no-op -- `saturating_sub` never underflows.
pub fn apply_release_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    if let Some(count) = counts.get_mut(&generation_id) {
        *count = count.saturating_sub(1);
    }
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

/// The inhibit half of `reset_registrations` (ADR-0006/ADR-0032): zeros `generation_id`'s
/// entire count in one step. `should_close_fd` follows the same global-total-reaches-zero rule
/// as [`apply_release_inhibit`], so a crashed generation holding the only inhibit still releases the fd.
pub fn cleanup_generation_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    counts.remove(&generation_id);
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub(crate) trait Login1Manager {
    #[zbus(name = "Inhibit")]
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

/// `what`/`who`/`mode` are fixed by ADR-0032: `mode = "block"` is the only mode that actually
/// blocks systemd's auto-suspend-on-idle rather than merely delaying it.
pub(crate) const INHIBIT_WHAT: &str = "idle";
pub(crate) const INHIBIT_WHO: &str = "oblisk";
pub(crate) const INHIBIT_MODE: &str = "block";

pub(crate) struct InhibitState {
    pub(crate) counts: HashMap<u32, u32>,
    pub(crate) fd: Option<zbus::zvariant::OwnedFd>,
}

pub(crate) struct LiveInhibit {
    /// The Supervisor's already-established system-bus connection. `Login1ManagerProxy` is built
    /// fresh per [`IdleController::inhibit`] call, not cached -- caching would freeze one startup failure into a permanent outage.
    pub(crate) system_bus: zbus::Connection,
    /// `tokio::sync::Mutex`, not `std::sync::Mutex`: the refcount decision, D-Bus call, and `fd`
    /// write run as one critical section with the lock held across an `.await`, which `std::sync::Mutex` can't do.
    pub(crate) state: tokio::sync::Mutex<InhibitState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- apply_inhibit / apply_release_inhibit (inhibit refcount arithmetic, TDD seam 2) ----

    #[test]
    fn apply_inhibit_opens_the_fd_on_the_global_zero_to_one_transition() {
        let mut counts = HashMap::new();
        let transition = apply_inhibit(&mut counts, 1);
        assert_eq!(transition, InhibitTransition { should_open_fd: true, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
    }

    #[test]
    fn apply_inhibit_does_not_reopen_the_fd_for_a_second_concurrent_generation() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        let transition = apply_inhibit(&mut counts, 2);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
        assert_eq!(counts.get(&2), Some(&1));
    }

    #[test]
    fn apply_release_inhibit_closes_the_fd_on_the_global_one_to_zero_transition() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        let transition = apply_release_inhibit(&mut counts, 1);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: true });
        assert_eq!(counts.get(&1), Some(&0));
    }

    #[test]
    fn apply_release_inhibit_from_one_generation_does_not_close_while_another_still_holds() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_inhibit(&mut counts, 2);

        let transition = apply_release_inhibit(&mut counts, 1);

        assert_eq!(
            transition,
            InhibitTransition { should_open_fd: false, should_close_fd: false },
            "generation 2 still holds an inhibit"
        );
        assert_eq!(counts.get(&1), Some(&0));
        assert_eq!(counts.get(&2), Some(&1));
    }

    #[test]
    fn apply_release_inhibit_on_an_already_zero_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_release_inhibit(&mut counts, 1);

        let transition = apply_release_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&0), "must not underflow below zero");
    }

    #[test]
    fn apply_release_inhibit_on_a_never_registered_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        let transition = apply_release_inhibit(&mut counts, 42);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert!(!counts.contains_key(&42));
    }

    // ---- cleanup_generation_inhibit (TDD seam 5, inhibit half) ----

    #[test]
    fn cleanup_generation_inhibit_zeros_only_the_named_generation_and_closes_if_it_was_the_last() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);

        let transition = cleanup_generation_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: true });
        assert!(!counts.contains_key(&1));
    }

    #[test]
    fn cleanup_generation_inhibit_does_not_close_while_another_generation_still_holds() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_inhibit(&mut counts, 2);

        let transition = cleanup_generation_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert!(!counts.contains_key(&1));
        assert_eq!(counts.get(&2), Some(&1), "generation 2's own count must be untouched");
    }

    #[test]
    fn cleanup_generation_inhibit_on_an_untracked_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);

        let transition = cleanup_generation_inhibit(&mut counts, 99);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
    }

    // ---- Login1ManagerProxy::inhibit (TDD seam 3: real D-Bus call, p2p pattern) ----

    use tokio::net::UnixStream;

    /// A connected pair of p2p zbus connections, no bus daemon involved.
    async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
        let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
        let guid = zbus::Guid::generate();
        let server_builder =
            zbus::connection::Builder::unix_stream(a).server(guid).expect("p2p server builder setup").p2p();
        let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
        tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
    }

    /// A stand-in for logind's own `org.freedesktop.login1.Manager`. Returns a real fd
    /// (`/dev/null`) so the proxy call under test gets back a real `OwnedFd` to wrap.
    struct StubLogin1Manager {
        calls: tokio::sync::mpsc::UnboundedSender<(String, String, String, String)>,
    }

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl StubLogin1Manager {
        #[zbus(name = "Inhibit")]
        fn inhibit(&self, what: String, who: String, why: String, mode: String) -> zbus::zvariant::OwnedFd {
            let _ = self.calls.send((what, who, why, mode));
            let file = std::fs::File::open("/dev/null").expect("open /dev/null for a test fd");
            let owned: std::os::fd::OwnedFd = file.into();
            zbus::zvariant::OwnedFd::from(owned)
        }
    }

    #[tokio::test]
    async fn login1_manager_inhibit_sends_the_expected_arguments_and_returns_a_fd() {
        let (manager_side, caller_side) = p2p_pair().await;
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        manager_side
            .object_server()
            .at("/org/freedesktop/login1", StubLogin1Manager { calls: calls_tx })
            .await
            .expect("failed to export the stub Login1Manager");

        let proxy: Login1ManagerProxy<'_> = zbus::proxy::Builder::new(&caller_side)
            .destination("org.oblisk.test")
            .expect("valid destination bus name")
            .path("/org/freedesktop/login1")
            .expect("valid object path")
            .interface("org.freedesktop.login1.Manager")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p Login1ManagerProxy");

        let fd =
            proxy.inhibit("idle", "oblisk", "playing a video", "block").await.expect("Inhibit call should succeed");
        // OwnedFd's own Drop closing it cleanly (no panic) is itself part of this assertion.
        drop(fd);

        let (what, who, why, mode) = calls_rx.recv().await.expect("stub Login1Manager never received Inhibit");
        assert_eq!(what, "idle");
        assert_eq!(who, "oblisk");
        assert_eq!(why, "playing a video");
        assert_eq!(mode, "block");
    }
}
