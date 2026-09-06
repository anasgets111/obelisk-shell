//! `secure_submit` tree analysis (`CONTEXT.md`, Secure submit). Kept here rather than
//! `crate::wayland::input`, where these predicates grew; the last helper needed a fully-qualified
//! path back into `crate::wayland`. Input routing consumes these predicates; `wayland::input` arms
//! the keyboard, `wayland::lock` grants the lock, and `crate::socket` vetoes unsafe reloads.

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, SecureSubmitTarget};
use crate::layout::{ResolvedNode, Scene};

/// The only pair reaching PAM and ending a session lock. `supervisor/src/main.rs` routes
/// `("lock", "authenticate")` to PAM and only its success emits `SetSessionLock { locked: false }`.
const UNLOCK_TARGET: (&str, &str) = ("lock", "authenticate");

/// Whether this destination ends a session lock. `wayland::lock::lock_command` uses the same
/// predicate when admitting a lock screen.
fn unlocks_the_session(target: &SecureSubmitTarget) -> bool {
    (target.capability.as_str(), target.action.as_str()) == UNLOCK_TARGET
}

/// Every declared destination in document order. It walks the whole tree because callers ask what
/// a surface offers before an event supplies a hit node. Malformed targets already fail
/// `Scene::apply`; `None` means a `textfield` declared no destination. This is not admission:
/// `tree_can_authenticate` uses [`sole_secure_submit`] so focus and lock admission cannot diverge.
pub(crate) fn secure_submit_targets(tree: &ResolvedNode) -> Vec<SecureSubmitTarget> {
    let mut found = Vec::new();
    let mut stack = vec![tree];
    while let Some(node) = stack.pop() {
        if let Some(node::PaintStyle::TextField { target: Some(target), .. }) = &node.paint {
            found.push(target.clone());
        }
        stack.extend(node.children.iter().rev());
    }
    found
}

/// The sole destination in the keyboard-focus scope, if exactly one is declared, with its surface
/// id. Focus moved here from pointer-only `focused_secure_submit` so a lock field is typable as
/// soon as the compositor grants keyboard focus. Two destinations have no non-arbitrary password
/// owner (`submit_frame_for`, ADR-0050 decision 4), so zero or many leave focus for a press.
/// Scope it over the focused surface and its shown popups: niri gives a grabbing popup the keyboard
/// only when its parent already held it. Surface-only scope made the network password prompt
/// untypable while its popup was open, then typable after reopening the panel.
pub(crate) fn sole_secure_submit_in_scope<'a>(
    scope: &[(&'a str, &ResolvedNode)],
) -> Option<(&'a str, SecureSubmitTarget)> {
    let mut sole = None;
    for (surface_id, tree) in scope {
        for target in secure_submit_targets(tree) {
            if sole.is_some() {
                return None;
            }
            sole = Some((*surface_id, target));
        }
    }
    sole
}

/// [`sole_secure_submit_in_scope`] for a standalone lock surface, which cannot parent a popup
/// (`SurfaceInstance::as_popup_parent` returns `None`).
pub(crate) fn sole_secure_submit(tree: &ResolvedNode) -> Option<SecureSubmitTarget> {
    sole_secure_submit_in_scope(&[("", tree)]).map(|(_, target)| target)
}

/// Whether a lock can authenticate out of its tree. Admission and keyboard arming must share this
/// sole-and-unlocking rule: the old admission-`any`/focus-`sole` split let a two-field lock take
/// the lock, arm nothing, and leave only a VT switch because the compositor does not unlock on
/// client death (ADR-0050 decision 4).
pub(crate) fn tree_can_authenticate(tree: &ResolvedNode) -> bool {
    sole_secure_submit(tree).as_ref().is_some_and(unlocks_the_session)
}

/// Vetoes a finished `Scene::apply` while the session is locked. Lock fingerprints contain only
/// `id`, so changing `child` would reload in place and could delete the password field; the
/// compositor would not unlock on client death. It reads the instances from this apply rather than
/// a remembered list, which survives hotplug and avoids retired-tree fossils. `any` lock instance
/// may authenticate, an empty set fails; restyling remains allowed (ADR-0052 decision 2).
pub(crate) fn lock_stays_authenticatable(
    scene: &Scene,
    instances: &[SurfaceInstance],
    holds_session_lock: bool,
) -> Result<(), node::LayoutError> {
    if !holds_session_lock {
        return Ok(());
    }
    let mut locks = Vec::new();
    for instance in instances {
        let Some(tree) = scene.surface(&instance.instance_id) else {
            continue;
        };
        if tree.kind == "lock" {
            if tree_can_authenticate(tree) {
                return Ok(());
            }
            locks.push(instance.instance_id.as_str());
        }
    }
    Err(node::invalid(
        "child",
        format!(
            "this evaluation leaves the locked session's `lock` surfaces {locks:?} with no single `textfield` carrying \
             `secure_submit = {{ capability = \"lock\", action = \"authenticate\" }}`, so the locked session would have no way back in \
             but a VT switch; the reload was refused and the lock screen that is on screen still stands (§ 6.4, ADR-0052 decision 3)"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(capability: &str, action: &str) -> SecureSubmitTarget {
        SecureSubmitTarget { capability: capability.to_string(), action: action.to_string() }
    }

    #[test]
    fn only_the_lock_authenticate_pair_can_unlock_the_session() {
        // supervisor/src/main.rs routes `("lock", "authenticate")` to the PAM worker and nothing
        // else to it, so a lock screen whose field submits anywhere else can never unlock.
        assert!(unlocks_the_session(&target("lock", "authenticate")));
        assert!(!unlocks_the_session(&target("polkit", "authenticate")));
        assert!(!unlocks_the_session(&target("lock", "cancel")));
    }
}
