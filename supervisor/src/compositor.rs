//! Which compositor this session is running, and the probe that answers it.
//!
//! A top-level module because compositor identity is a session-level fact, not a capability's
//! property. It lived in `hardware/keyboard/layout.rs` (ADR-0034 built it there for
//! `keyboard.active_layout`) until `workspaces` became the second caller and had to reach
//! sideways into a sibling capability's module for it. `CONTEXT.md`'s **Compositor link** entry
//! already scoped that trait to "what keyboard layout needs today", so the probe was squatting
//! in a module that disclaimed owning it.
//!
//! Detection and Hyprland's socket paths, and no adaptor: ADR-0056 decision 1 settled that
//! `workspaces` gets no trait and `CompositorLink` does not grow one. What the two capabilities
//! genuinely share is this probe and, since `workspaces::hyprland` (ADR-0118), where Hyprland's
//! two sockets live; both moved here unchanged in behaviour.

use std::path::PathBuf;

/// A compositor this codebase has an implementor for, which is narrower than "a compositor that
/// exists": a session running anything else is [`detect_compositor`]'s `None`, and the
/// capabilities that need one degrade rather than guess (ADR-0056 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorKind {
    Hyprland,
    Niri,
}

impl CompositorKind {
    /// The lowercase name a payload carries (`oblisk.workspaces.compositor`, ADR-0119), so a
    /// config can pick display policy by compositor without a second detection of its own.
    pub fn name(self) -> &'static str {
        match self {
            CompositorKind::Hyprland => "hyprland",
            CompositorKind::Niri => "niri",
        }
    }
}

/// The env var each compositor sets for every process in its own session, in probe order.
///
/// A table rather than the `if`/`else` chain this replaces, so a third compositor is a line of
/// data and the precedence is something you read instead of infer. Order is the tie-break for a
/// session with two set, which is unlikely and harmless: real sessions run one compositor.
///
/// Every entry is a var its compositor sets *because it is running*, which is why
/// `$XDG_CURRENT_DESKTOP` is not in this table -- that one is a name, written by whatever
/// launched the session, and it is still set for a session whose compositor never came up. It is
/// good enough to say out loud ([`unsupported_session_report`]) and not good enough to dispatch
/// on.
const PROBES: &[(CompositorKind, &str)] =
    &[(CompositorKind::Hyprland, "HYPRLAND_INSTANCE_SIGNATURE"), (CompositorKind::Niri, "NIRI_SOCKET")];

/// The first [`PROBES`] entry whose var this session has set, or `None` for a compositor with no
/// implementor here.
pub fn detect_compositor() -> Option<CompositorKind> {
    PROBES.iter().find(|(_, var)| std::env::var_os(var).is_some()).map(|(kind, _)| *kind)
}

/// What a capability puts in its "disabled for this run" line when [`detect_compositor`] returned
/// `None`. Shared because `keyboard` and `workspaces` were each spelling their own version of it,
/// and the two disagreed about how much they told the user.
///
/// Names `$XDG_CURRENT_DESKTOP` when the session set one, because "this session is sway, which
/// has no implementor" is something a user can act on and "neither HYPRLAND_INSTANCE_SIGNATURE
/// nor NIRI_SOCKET is set" -- what `keyboard` used to print -- is not. Deliberately not a second
/// detection path: a name this does not recognise still yields no implementor, it just gets said.
pub fn unsupported_session_report() -> String {
    match session_desktop() {
        Some(desktop) => format!("this session is {desktop}, which has no implementor"),
        None => "no supported compositor was detected".to_string(),
    }
}

/// A socket in Hyprland's per-instance directory, `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`:
/// `"socket2.sock"` pushes newline-terminated `event>>payload` lines, `"socket.sock"` answers one
/// plain-text command per connection (`j/workspaces` for JSON, `dispatch ...` for a write). Here
/// rather than in either caller because `keyboard` and `workspaces` both open the same two files,
/// and a session-level path is the probe module's kind of fact.
pub fn hyprland_socket_path(signature: &str, name: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    hyprland_socket_path_in(&runtime_dir, signature, name)
}

/// The `join` half of [`hyprland_socket_path`], split off the `$XDG_RUNTIME_DIR` lookup so the
/// test does not have to `set_var`. `setenv` rewrites the process-wide `environ` block, so it
/// races every concurrent `getenv` in the test binary whatever variable either one names -- not
/// just another reader of this one.
fn hyprland_socket_path_in(runtime_dir: &str, signature: &str, name: &str) -> PathBuf {
    PathBuf::from(runtime_dir).join("hypr").join(signature).join(name)
}

fn session_desktop() -> Option<String> {
    let value = std::env::var("XDG_CURRENT_DESKTOP").ok()?;
    desktop_name(&value).map(str::to_string)
}

/// `$XDG_CURRENT_DESKTOP`'s first entry. The spec makes it a colon-separated list ordered most-
/// to least specific, so `"niri:wlroots"` names niri. Split out from [`session_desktop`] so the
/// parse is testable without writing a process-global env var from a test thread.
fn desktop_name(value: &str) -> Option<&str> {
    let first = value.split(':').next()?.trim();
    (!first.is_empty()).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compositor_kind_is_detectable_by_the_var_that_compositor_sets() {
        // Two halves. The `match` is exhaustive, so a new `CompositorKind` fails this build until
        // it names the env var it sets; the `find` and the length then fail until that pair is in
        // `PROBES` too. A variant with no probe entry is a compositor nothing can ever detect.
        for kind in [CompositorKind::Hyprland, CompositorKind::Niri] {
            let var = match kind {
                CompositorKind::Hyprland => "HYPRLAND_INSTANCE_SIGNATURE",
                CompositorKind::Niri => "NIRI_SOCKET",
            };
            assert_eq!(PROBES.iter().find(|(probe, _)| *probe == kind).map(|(_, v)| *v), Some(var), "{kind:?}");
        }
        assert_eq!(PROBES.len(), 2, "a PROBES entry for a kind the loop above does not list");
    }

    #[test]
    fn hyprland_socket_path_joins_runtime_dir_hypr_signature_and_name() {
        assert_eq!(
            hyprland_socket_path_in("/run/user/1000", "abc123", "socket2.sock"),
            PathBuf::from("/run/user/1000/hypr/abc123/socket2.sock")
        );
    }

    /// The fallback the previous version of this test could not reach: it had to set
    /// `$XDG_RUNTIME_DIR` to run at all, so the one branch that fires when the variable is
    /// missing went unasserted.
    #[test]
    fn a_missing_runtime_dir_falls_back_to_tmp() {
        assert_eq!(
            hyprland_socket_path_in("/tmp", "abc123", "socket2.sock"),
            PathBuf::from("/tmp/hypr/abc123/socket2.sock")
        );
    }

    #[test]
    fn desktop_name_takes_the_most_specific_entry_of_a_colon_separated_list() {
        assert_eq!(desktop_name("niri:wlroots"), Some("niri"));
        assert_eq!(desktop_name("Hyprland"), Some("Hyprland"));
        assert_eq!(desktop_name(" sway : wlroots "), Some("sway"));
    }

    #[test]
    fn desktop_name_is_none_when_the_variable_is_set_but_empty() {
        // A set-but-empty `XDG_CURRENT_DESKTOP` is a real thing a bare `weston`/`cage` session
        // leaves behind; reporting "this session is , which has no implementor" would be worse
        // than the generic line.
        assert_eq!(desktop_name(""), None);
        assert_eq!(desktop_name("   "), None);
        assert_eq!(desktop_name(":wlroots"), None);
    }
}
