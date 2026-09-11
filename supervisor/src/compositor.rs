//! Which compositor this session is running, and the probe that answers it.
//!
//! Top-level because compositor identity belongs to the session, not a capability. It lived in
//! `hardware/keyboard/layout.rs` (ADR-0034 put it there for `keyboard.active_layout`) until
//! `workspaces` became the second caller and had to reach sideways into a sibling capability's
//! module. `CONTEXT.md`'s **Compositor link** already scoped that trait to "what keyboard layout
//! needs today", so the probe belonged elsewhere.
//!
//! This owns detection and Hyprland's socket paths, with no adaptor. ADR-0056 decision 1 says
//! `workspaces` gets no trait and `CompositorLink` does not grow one. The two capabilities share
//! only this probe and, since `workspaces::hyprland` (ADR-0118), the two socket locations; both
//! moved here unchanged in behaviour.

use std::path::PathBuf;

/// A compositor implemented here, narrower than "a compositor that exists". Other sessions yield
/// [`detect_compositor`]'s `None`; dependent capabilities degrade rather than guess (ADR-0056
/// decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorKind {
    Hyprland,
    Niri,
}

impl CompositorKind {
    /// Lowercase payload name (`obelisk.workspaces.compositor`, ADR-0119), so config can choose
    /// display policy without detecting the compositor again.
    pub fn name(self) -> &'static str {
        match self {
            CompositorKind::Hyprland => "hyprland",
            CompositorKind::Niri => "niri",
        }
    }
}

/// Env vars each compositor sets for every process in its session, in probe order.
///
/// A table replaces the old `if`/`else` chain: a third compositor is one data line and precedence
/// is explicit. Order breaks ties if two vars are set; that case is unlikely and harmless because
/// real sessions run one compositor.
///
/// Entries are vars set *because the compositor is running*. `$XDG_CURRENT_DESKTOP` is only a name
/// written by the launcher and remains set if the compositor never starts. It is useful to report
/// via [`unsupported_session_report`], not to dispatch on.
const PROBES: &[(CompositorKind, &str)] =
    &[(CompositorKind::Hyprland, "HYPRLAND_INSTANCE_SIGNATURE"), (CompositorKind::Niri, "NIRI_SOCKET")];

/// The first [`PROBES`] entry whose var this session has set, or `None` for a compositor with no
/// implementor here.
pub fn detect_compositor() -> Option<CompositorKind> {
    PROBES.iter().find(|(_, var)| std::env::var_os(var).is_some()).map(|(kind, _)| *kind)
}

/// The shared "disabled for this run" line when [`detect_compositor`] returns `None`. `keyboard`
/// and `workspaces` had diverged in how much their local versions told the user.
///
/// If set, names `$XDG_CURRENT_DESKTOP`: "this session is sway, which has no implementor" is
/// actionable, unlike `keyboard`'s old "neither HYPRLAND_INSTANCE_SIGNATURE nor NIRI_SOCKET is
/// set". This is not a second detection path; an unrecognised name still yields no implementor.
pub fn unsupported_session_report() -> String {
    match session_desktop() {
        Some(desktop) => format!("this session is {desktop}, which has no implementor"),
        None => "no supported compositor was detected".to_string(),
    }
}

/// A socket in Hyprland's per-instance directory,
/// `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`. `"socket2.sock"` pushes
/// newline-terminated `event>>payload` lines; `"socket.sock"` answers one plain-text command per
/// connection (`j/workspaces` for JSON, `dispatch ...` for a write). Shared because `keyboard` and
/// `workspaces` both open these files, and their path is a session-level fact.
pub fn hyprland_socket_path(signature: &str, name: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    hyprland_socket_path_in(&runtime_dir, signature, name)
}

/// The `join` half of [`hyprland_socket_path`], split from `$XDG_RUNTIME_DIR` lookup so tests avoid
/// `set_var`. `setenv` rewrites process-wide `environ` and races every concurrent `getenv` in the
/// test binary, even for unrelated variables.
fn hyprland_socket_path_in(runtime_dir: &str, signature: &str, name: &str) -> PathBuf {
    PathBuf::from(runtime_dir).join("hypr").join(signature).join(name)
}

fn session_desktop() -> Option<String> {
    let value = std::env::var("XDG_CURRENT_DESKTOP").ok()?;
    desktop_name(&value).map(str::to_string)
}

/// `$XDG_CURRENT_DESKTOP`'s first entry. The spec orders its colon-separated list most to least
/// specific, so `"niri:wlroots"` names niri. Separate from [`session_desktop`] to test parsing
/// without writing a process-global env var from a test thread.
fn desktop_name(value: &str) -> Option<&str> {
    let first = value.split(':').next()?.trim();
    (!first.is_empty()).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compositor_kind_is_detectable_by_the_var_that_compositor_sets() {
        // The exhaustive `match` forces each new kind to name its env var. `find` and the length
        // then force that pair into `PROBES`; a missing entry is undetectable.
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

    /// The old test had to set `$XDG_RUNTIME_DIR` to run, leaving the missing-variable fallback
    /// unasserted.
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
        // Bare `weston`/`cage` can leave `XDG_CURRENT_DESKTOP` set but empty; reporting
        // "this session is , which has no implementor" is worse than the generic line.
        assert_eq!(desktop_name(""), None);
        assert_eq!(desktop_name("   "), None);
        assert_eq!(desktop_name(":wlroots"), None);
    }
}
