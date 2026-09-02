//! Which `secure_submit` destinations a resolved tree declares, and whether a `lock` surface built
//! from that tree can be authenticated out of (`CONTEXT.md`, **Secure submit**).
//!
//! Here, not in `crate::wayland::input` where these grew: everything here takes a
//! [`ResolvedNode`](crate::layout::ResolvedNode) or a
//! [`SecureSubmitTarget`](crate::layout::node::SecureSubmitTarget) and returns one, tree analysis
//! that input routing consumes rather than performs. The three callers sit in three modules
//! (`wayland::input` arms the keyboard, `wayland::lock` grants the lock, `crate::socket` vetoes a
//! reload); under `wayland`, the last of those needed a fully-qualified path back into
//! `crate::wayland` for a predicate the module re-exported upward for it alone.

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, SecureSubmitTarget};
use crate::layout::{ResolvedNode, Scene};

/// The `(capability, action)` pair that reaches PAM, and the only one that can ever end a session
/// lock. `supervisor/src/main.rs` routes `SecureSubmit { capability: "lock", action:
/// "authenticate" }` to the PAM worker and answers `PamOutcome::Success` with the one
/// `SetSessionLock { locked: false }` this process will ever see; every other pair cannot unlock
/// the session.
const UNLOCK_TARGET: (&str, &str) = ("lock", "authenticate");

/// Whether this destination is the one that ends a session lock.
///
/// Named rather than compared inline: `wayland::lock`'s `lock_command` refuses a lock screen with
/// no such field, and nothing else may quietly grow a second opinion about which pair unlocks.
/// See [`UNLOCK_TARGET`].
fn unlocks_the_session(target: &SecureSubmitTarget) -> bool {
    (target.capability.as_str(), target.action.as_str()) == UNLOCK_TARGET
}

/// Every `secure_submit` destination a resolved tree declares, in document order.
///
/// Whole-tree, unlike `wayland::input`'s `focused_target`, which walks a hit path from one node
/// for the innermost: these callers have no node to start from, asking what a surface offers
/// before any event has arrived.
///
/// Reads `node::paint_style` already parsed, so there is no malformed case left to skip: an
/// unparseable `secure_submit` fails `Scene::apply` first. `None` here is a `textfield` that
/// declared no destination.
///
/// **Not the admission rule.** Asking whether *any* target here unlocks is the bug
/// [`tree_can_authenticate`] exists to have fixed: it grants a lock the keyboard then arms nothing
/// on, since the compositor does not unlock when the client dies. Admission and focus both read
/// [`sole_secure_submit`] instead; this is `pub(crate)` only for `wayland::input`'s
/// `focus_on_enter`, asking whether a field it already holds is still declared.
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

/// The destination a surface takes on keyboard focus, when its tree declares exactly one.
///
/// Why keyboard focus focuses a field at all: without it, `focused_secure_submit` was set only by
/// a pointer press, requiring a mouse click before a keystroke could reach `shared::SecureBuffer`
/// on the one surface whose purpose is to accept a password. A lock surface must be typable the
/// moment the compositor hands it keyboard focus.
///
/// Exactly one, deliberately: with two `secure_submit` fields there is no non-arbitrary answer to
/// "whose password is this?", the guess `wayland::input`'s `submit_frame_for` already refuses to
/// make (ADR-0050 decision 4). Zero is the same answer. Both cases leave focus alone for a
/// press to decide, which buys the single-field case: every lock screen and password prompt.
pub(crate) fn sole_secure_submit(tree: &ResolvedNode) -> Option<SecureSubmitTarget> {
    let mut targets = secure_submit_targets(tree);
    (targets.len() == 1).then(|| targets.remove(0))
}

/// Whether a `lock` surface's resolved tree can actually be authenticated out of -- the predicate
/// `wayland::lock`'s `lock_command` reads as `can_authenticate`, and it is deliberately built out
/// of [`sole_secure_submit`] rather than out of [`secure_submit_targets`].
///
/// The guard that grants the lock and the rule that arms the keyboard must be one predicate. They
/// were two: admission asked whether any field in the tree unlocks, focus armed only a sole field.
/// A lock screen with two `secure_submit` fields passed the guard, took the lock -- which the
/// compositor will not release when the client dies -- and then armed nothing when the compositor
/// handed the surface keyboard focus, leaving a VT switch as the only way back in.
///
/// Sole-and-unlocking is the right rule, not merely the stricter one: `any` is not implementable
/// as a focus rule at all, since with two destinations there is no non-arbitrary answer to "whose
/// password is this?" (the guess `wayland::input`'s `submit_frame_for` already refuses to make,
/// ADR-0050 decision 4). So the focus rule stays, and admission moves to meet it.
pub(crate) fn tree_can_authenticate(tree: &ResolvedNode) -> bool {
    sole_secure_submit(tree).as_ref().is_some_and(unlocks_the_session)
}

/// The veto `Scene::apply` runs on the finished scene while this process holds a session lock:
/// the locked session must still be one the user can authenticate out of.
///
/// **Why this exists at all.** `layout::node::SurfaceFingerprint::Lock` carries only the `id`, so
/// editing a lock's `child` -- and so its password field -- diffs as `Unchanged` and reloads in
/// place, which the generation-swap gate does not police. Deleting the `textfield` while the lock
/// screen is up would therefore apply immediately, and the compositor does not unlock when a
/// lock client dies, so the way out would be a VT switch.
///
/// **It asks the apply what is on the glass, and holds no list of its own.** The arming side is
/// one `bool` (`crate::socket::RendererClient`'s `holds_session_lock`); the `lock` instances are
/// read out of the instance set this very apply is resolving. A remembered list could not survive
/// a hotplug: [`Scene`] keeps a retired instance's tree, so a snapshot taken at grant time would
/// vouch for a fossil nothing can paint while the live lock screen quietly lost its way out.
///
/// **`any`, not `all`, the same rule the grant used**, since a veto demanding all declared
/// instances be typable would refuse every reload for the rest of a lock already granted. An
/// empty set fails.
///
/// **Restyling a live lock screen must keep working**, which is why the veto asks the narrowest
/// possible question rather than freezing the tree (ADR-0052 decision 2).
///
/// The predicate is [`tree_can_authenticate`], not a copy of it: a second opinion about what makes
/// a lock screen usable is how a lock gets granted against a rule the keyboard does not follow.
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
            if tree_can_authenticate(&tree) {
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
