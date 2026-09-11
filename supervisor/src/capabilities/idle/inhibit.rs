//! Inhibit half of `obelisk.idle` (ADR-0032): login1 `Inhibit` on the existing system bus, with
//! per-generation refcounts, a hand-written proxy, and shared fd state. Split from `dbus::idle`;
//! see `hardware/idle/mod.rs`.

use std::collections::HashMap;

/// One refcount transition's fd action: open at 0->1 or close at ->0, never both.
/// [`apply_inhibit`] reports only `should_open_fd`; the others only `should_close_fd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InhibitTransition {
    pub should_open_fd: bool,
    pub should_close_fd: bool,
}

fn total(counts: &HashMap<u32, u32>) -> u32 {
    counts.values().sum()
}

/// `idle:inhibit(reason)`'s refcount half: increments `generation_id`; `should_open_fd` is true
/// only when the global total was zero before this call (ADR-0032's 0->1 rule).
pub fn apply_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    *counts.entry(generation_id).or_insert(0) += 1;
    InhibitTransition { should_open_fd: total_before == 0, should_close_fd: false }
}

/// `idle:release_inhibit()` decrements `generation_id`; `should_close_fd` is true only when the
/// global total reaches zero. Releasing an empty generation is a silent `saturating_sub` no-op.
pub fn apply_release_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    if let Some(count) = counts.get_mut(&generation_id) {
        *count = count.saturating_sub(1);
    }
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

/// Inhibit half of `reset_registrations` (ADR-0006/ADR-0032): zero `generation_id`'s count;
/// close the fd when the global total reaches zero, so a crashed last holder releases it.
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

    /// Current `block` inhibitors, colon-separated, e.g. `"idle:handle-power-key"`. A real,
    /// change-notified property lets one watch cover every holder, including this shell (ADR-0139).
    #[zbus(property, name = "BlockInhibited")]
    fn block_inhibited(&self) -> zbus::Result<String>;

    /// Every held inhibitor's `what`, `who`, `why`, `mode`, `uid`, and `pid`. Called once per
    /// `BlockInhibited` change, never on a timer (ADR-0139/ADR-0141).
    #[zbus(name = "ListInhibitors")]
    fn list_inhibitors(&self) -> zbus::Result<Vec<super::state::InhibitorRow>>;
}

/// ADR-0032 fixes `what`/`who`/`mode`; only `mode = "block"` stops systemd's auto-suspend-on-idle,
/// while `delay` merely postpones it.
pub(crate) const INHIBIT_WHAT: &str = "idle";
pub(crate) const INHIBIT_WHO: &str = "obelisk";
pub(crate) const INHIBIT_MODE: &str = "block";

pub(crate) struct InhibitState {
    pub(crate) counts: HashMap<u32, u32>,
    pub(crate) fd: Option<zbus::zvariant::OwnedFd>,
}

pub(crate) struct LiveInhibit {
    /// Existing Supervisor system-bus connection. `Login1ManagerProxy` is built fresh per
    /// [`IdleController::inhibit`] call; caching would make one startup failure permanent.
    pub(crate) system_bus: zbus::Connection,
    /// `tokio::sync::Mutex`: refcount decision, D-Bus call, and `fd` write are one critical section
    /// held across `.await`, which `std::sync::Mutex` cannot do.
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

    use crate::capabilities::test_support::p2p_pair_serving;

    /// Stand-in for logind's `org.freedesktop.login1.Manager`; returns `/dev/null` so the proxy
    /// receives a real `OwnedFd`.
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
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let (caller_side, _manager_side) =
            p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", StubLogin1Manager { calls: calls_tx }))
                .await;

        let proxy: Login1ManagerProxy<'_> = zbus::proxy::Builder::new(&caller_side)
            .destination("org.obelisk.test")
            .expect("valid destination bus name")
            .path("/org/freedesktop/login1")
            .expect("valid object path")
            .interface("org.freedesktop.login1.Manager")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p Login1ManagerProxy");

        let fd =
            proxy.inhibit("idle", "obelisk", "playing a video", "block").await.expect("Inhibit call should succeed");
        // OwnedFd::drop must close it cleanly without panicking.
        drop(fd);

        let (what, who, why, mode) = calls_rx.recv().await.expect("stub Login1Manager never received Inhibit");
        assert_eq!(what, "idle");
        assert_eq!(who, "obelisk");
        assert_eq!(why, "playing a video");
        assert_eq!(mode, "block");
    }
}
