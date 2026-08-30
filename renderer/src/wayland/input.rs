//! Pointer input (`on_click`, docs/adr/0050) and keyboard focus, including the `secure_submit`
//! accumulation that turns keystrokes into a `SecureSubmit` frame with no Lua value ever holding
//! the plaintext (ADR-0005/ADR-0027).
//!
//! `SeatHandler`, `PointerHandler` and `KeyboardHandler` all live here, next to the pure helpers
//! their callbacks call: which button armed a click, which `textfield` a press or a keyboard
//! `enter` focuses, and what one key event does to the field currently focused.

use super::*;
use crate::wayland::lock::UNLOCK_TARGET;

/// One press waiting for its release (docs/adr/0050 decision 2): a click is a press and a
/// release on the same node, so a user who presses a button, notices the mistake, and drags off
/// it releases harmlessly.
///
/// "Same node" is this pair, not a node identity: `ResolvedNode` has none (`NodeId` lives on
/// `RetainedNode`, dropped by `to_resolved`). The rect is the proxy, and the case it gets
/// "wrong" it gets right anyway: a re-resolve between press and release that moves the button
/// cancels the click, the same answer a real identity would give for a button moved out from
/// under the pointer.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ArmedClick {
    instance_id: String,
    rect: LogicalRect,
    /// The evdev code the press carried, so the release must be the same button, not merely
    /// a button (docs/adr/0050's second amendment).
    ///
    /// ponytail: one armed click, so chording drops both. `armed` is a single `Option`, and a
    /// second press overwrites the first, so pressing right then left on the same node and
    /// releasing either fires nothing: the release that arrives finds a different button armed, and
    /// the one after it finds nothing armed at all. It fails in the safe direction (a wrong handler
    /// never runs, a click is only lost) and it needs two buttons held at once, which nobody does
    /// to a shell. The upgrade is `armed` becoming keyed by button, an `ArrayVec` of three or a
    /// small map, with the same instance-id-and-rect comparison per entry; do it if a config ever
    /// wants a chord, or if a real mouse turns out to emit overlapping pairs on its own.
    button: u32,
}
/// The serial `xdg_popup.grab` needs, plus the surface the event carrying it was delivered to
/// (docs/adr/0049's amendment, docs/adr/0051 decision 1). Armed by [`PointerHandler::pointer_frame`]
/// and cleared by [`run`]'s poll loop at the end of the same turn.
///
/// docs/adr/0049 decision 2 claimed the re-resolve that creates a popup "is still running inside
/// input dispatch, so the engine has the serial of the event that caused it". It is not:
/// `re_resolve_if_dirty` runs in the poll loop, after `dispatch_pending` returns, so the dispatch
/// callback's stack -- and any serial on it -- is gone by then. Resolving inside dispatch would put
/// a full `Scene::apply`, arbitrary Lua and Wayland object creation inside a `Dispatch` callback,
/// reentering the queue being dispatched from. So the serial lives in a field for one turn instead
/// of on a stack; a re-resolve driven by anything other than input still finds nothing here.
///
/// Both a press and a release arm it, latest wins: a click fires on the release (docs/adr/0050
/// decision 2), so the release's serial is the one an `on_click` popup actually carries. `xdg_shell`
/// only asks that the serial come from "a real input event (button press, key press, touch down)"
/// and leaves recency to the compositor, which answers a refusal with an immediate `popup_done`
/// -- normal (docs/adr/0051 decision 3), not an error.
///
/// `instance_id`, not the tracked index: `self.surfaces` is a `Vec` an output change removes from
/// (`destroy_surface_by_id`), and an unplugged monitor between click and re-resolve would leave an
/// index naming a different surface. The id is stable and is what `is_instance_of` compares anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArmedSerial {
    pub(super) serial: u32,
    pub(super) instance_id: String,
}
/// The innermost `button` in a [`layout::hit::hit_path`] result carrying a callable `on_click`, as
/// that button's absolute rect and its function (docs/adr/0050 decision 1).
///
/// Scans from the deep end: the deepest node under the pointer is normally the `button`'s `text`
/// child, which has no `on_click`. A `button` without one is transparent to this scan, not a
/// barrier, so a plain `button` nested inside a handled one still lets the outer one fire.
///
/// `on_click` must be a `Value::Function`; anything else the config wrote there is not a click
/// handler. `layout::node` has no parser for the key (§ 5.2 leaves it opaque), so this predicate
/// is the only place its type is checked.
fn clickable_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(LogicalRect, &'a Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let Some(Value::Function(on_click)) = node.properties.get("on_click") else {
            return None;
        };
        Some((layout::hit::absolute_rect(&path[..=depth])?, on_click))
    })
}
/// Both answers one press wants out of decision 1's single traversal: the `button` that would fire,
/// and the destination the innermost `textfield` addresses the next secret to.
///
/// One struct, not two lookups: both come from one [`layout::hit::hit_path`] call. Walking twice
/// would be two answers to one event, and a re-resolve between the walks could make them disagree.
struct PointerHit {
    button: Option<(LogicalRect, Function)>,
    /// `Err` is a malformed `secure_submit` on the innermost `textfield` -- see [`focused_target`].
    focus: Result<Option<node::SecureSubmitTarget>, node::LayoutError>,
}
/// The `secure_submit` destination the innermost `textfield` in a hit path names (docs/adr/0050
/// decision 4, § 5.2 item 8).
///
/// `Ok(None)` collapses two cases nothing downstream tells apart: no `textfield` on the path, and
/// the innermost one declaring no `secure_submit`. Both mean the next completed submit has nowhere
/// to go.
///
/// ponytail: focus is therefore stored as its destination rather than as a node identity, so a
/// focused field with no destination is indistinguishable from no focus at all. Nothing reads focus
/// for any other purpose yet -- there is no caret, no selection, and no `on_key` (docs/adr/0050's
/// consequences). Upgrade path: carry the field's `NodeId` alongside the target once something has
/// to paint or address the *field* rather than its submit.
fn focused_target(path: &[&layout::ResolvedNode]) -> Result<Option<node::SecureSubmitTarget>, node::LayoutError> {
    let Some(field) = path.iter().rev().find(|node| node.kind == "textfield") else {
        return Ok(None);
    };
    node::parse_secure_submit(&field.properties)
}
/// The frame a completed `wp-text-input-v3` submit produces, or `None` when no focused `textfield`
/// named a destination for it (docs/adr/0050 decision 4).
///
/// `None` is the point: before it, the submit was addressed to `"unknown"/"unknown"`, which no
/// Supervisor capability routes -- a password put on the wire for nobody. Sending nothing is the
/// only safe answer to "whose password is this?".
///
/// The buffer is zeroized on both branches; on the branch that sends nothing it is the only thing
/// that happens, so a dropped submit never leaves the accumulated secret sitting in `App`.
fn submit_frame_for(
    generation_id: u32,
    target: Option<&node::SecureSubmitTarget>,
    buffer: &mut shared::SecureBuffer,
) -> Option<RendererFrame> {
    // An empty buffer is not a password, and sending one is not free. The Supervisor routes it
    // straight into PAM, which spends one of the user's counted attempts and one `pam_unix` failure
    // delay answering a keystroke that said nothing -- on the lock screen, where attempts are the
    // scarce resource. Enter on an empty field does nothing, the way it does in every other password
    // prompt. Checked before the destination, because it is true whatever the destination was.
    let Some(target) = target.filter(|_| !buffer.is_empty()) else {
        buffer.zeroize();
        return None;
    };
    Some(secure_submit_frame(generation_id, &target.capability, &target.action, buffer))
}
/// A focused `secure_submit` field, together with the surface whose tree declared it.
///
/// The surface id is what a past review's defects 2 and 3 were both missing: focus used to be
/// nothing but a destination, so nothing could tell "the field on the surface that currently has
/// the keyboard" from "the field on a surface this process destroyed ten seconds ago". Concretely:
/// type a password on the lock screen, let the compositor send `finished`, and the plaintext sits
/// in `App::secure_buffer` still addressed to `("lock", "authenticate")` with later bar keystrokes
/// appending to it -- `wl_keyboard.leave` was relied on to notice, but the protocol does not
/// require a compositor to send one for a surface the client itself destroyed.
///
/// So the field is bound to its surface, and [`focus_is_still_armed`] is the one question every
/// keystroke asks, rather than a clearing call bolted onto each of the five or six sites that can
/// take a surface away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FocusedField {
    /// The `"{id}@{output}"` instance id of the surface the field was declared on.
    surface_id: String,
    target: node::SecureSubmitTarget,
}
/// The one place `App::focused_secure_submit` is ever written, and the reason it is one place.
///
/// A `shared::SecureBuffer`'s lifetime belongs to the field the bytes were typed into, not to the
/// transport that carried them. Three sites cleared or reassigned the focus target and left the
/// plaintext behind -- `KeyboardHandler::leave`, `SeatHandler::remove_capability`'s keyboard arm,
/// and the retargeting press in `PointerHandler::pointer_frame`. Concretely: type a login password
/// into the lock screen's field and press nothing, let the compositor tear the lock surfaces down,
/// take keyboard focus on a bar whose sole `secure_submit` is `("network", "connect")`, type a
/// Wi-Fi PSK, press Enter, and [`submit_frame_for`] addresses `<login password><psk>` to the
/// network capability -- exactly the routing docs/adr/0005 exists to make impossible.
///
/// So the rule is enforced on the transition, not at each site that performs one: any change of
/// destination, including one field to another directly, scrubs. A fifth caller inherits it by
/// construction.
///
/// Re-arming the same destination deliberately does not scrub: a press decides focus
/// unconditionally (docs/adr/0050 decision 4), so clicking twice in the field being typed into
/// arrives here with the target unchanged.
///
/// A free function, not a `&mut self` method, so the property is testable without a live Wayland
/// connection -- the same reason [`secure_submit_frame`] is one.
fn retarget_secure_submit(focused: &mut Option<FocusedField>, buffer: &mut shared::SecureBuffer, next: Option<FocusedField>) {
    if *focused != next {
        buffer.zeroize();
    }
    *focused = next;
}
/// Whether this `secure_submit` destination is the one that can end a session lock.
///
/// Named rather than compared inline because two callers want it for opposite reasons:
/// `lock::lock_command` refuses a lock screen that has no such field, and nothing else in the file may
/// quietly grow a second opinion about which pair unlocks. See [`UNLOCK_TARGET`].
fn unlocks_the_session(target: &node::SecureSubmitTarget) -> bool {
    (target.capability.as_str(), target.action.as_str()) == UNLOCK_TARGET
}
/// Every `secure_submit` destination a resolved tree declares, in document order.
///
/// Whole-tree, unlike [`focused_target`]: a press names one node and walks a hit path for the
/// innermost, but these callers have no node to start from -- asking what a surface offers before
/// any event has arrived on it.
///
/// A malformed `secure_submit` contributes nothing rather than an error: the config bug is already
/// reported where it can name the surface (the press path logs it), and a field whose destination
/// cannot be parsed is a field nothing can address a secret to.
fn secure_submit_targets(tree: &layout::ResolvedNode) -> Vec<node::SecureSubmitTarget> {
    let mut found = Vec::new();
    let mut stack = vec![tree];
    while let Some(node) = stack.pop() {
        if node.kind == "textfield"
            && let Ok(Some(target)) = node::parse_secure_submit(&node.properties)
        {
            found.push(target);
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
/// "whose password is this?", the guess [`submit_frame_for`] already refuses to make (docs/adr/0050
/// decision 4). Zero is the same answer. Both cases leave focus alone for a press to decide, which
/// buys the single-field case: every lock screen and password prompt.
fn sole_secure_submit(tree: &layout::ResolvedNode) -> Option<node::SecureSubmitTarget> {
    let mut targets = secure_submit_targets(tree);
    (targets.len() == 1).then(|| targets.remove(0))
}
/// What `focused_secure_submit` becomes when keyboard focus arrives on `surface_id`, given that
/// surface's resolved tree and whatever is focused now.
///
/// A total function -- defect 2. [`KeyboardHandler::enter`] used to spell its "nothing to arm"
/// cases (an untracked surface, a tracked one with no sole `secure_submit`) as early returns that
/// moved `keyboard_focus` on and left `focused_secure_submit` unchanged. `apply_secure_key` gates
/// on focus alone, so keystrokes on a surface with no password field kept accumulating into the
/// previous surface's field and could still be submitted to its capability. Every case answers
/// here, pushed through [`App::focus_secure_submit`], so "nothing to arm" is the scrub it always
/// should have been.
///
/// What survives an `enter` is a field on the surface that is entering, and only that: a press on
/// a surface with several `secure_submit` fields picks one that [`sole_secure_submit`] refuses to
/// pick, and the compositor's `enter` commonly follows that press, so discarding it would make a
/// multi-field surface untypable by clicking. Requiring the tree to still declare that destination
/// keeps a reload from pointing at a field the config has since deleted.
fn focus_on_enter(surface_id: Option<&str>, tree: Option<&layout::ResolvedNode>, current: Option<&FocusedField>) -> Option<FocusedField> {
    let (id, tree) = (surface_id?, tree?);
    if let Some(current) = current.filter(|field| field.surface_id == id && secure_submit_targets(tree).contains(&field.target)) {
        return Some(current.clone());
    }
    Some(FocusedField { surface_id: id.to_string(), target: sole_secure_submit(tree)? })
}
/// Whether a focused field is still armed: its own surface both holds the keyboard and still exists
/// as a live `wl_surface` in this process.
///
/// Both clauses, neither redundant. The keyboard clause is defect 2: a pointer press arms focus on
/// whatever surface it landed on, so without it a field on a `keyboard_interactivity = none` panel
/// stays armed while another surface actually receives keys. The liveness clause is defect 3: a
/// `wl_surface` this process destroyed may never produce a `leave`, so the field on a torn-down
/// lock screen would otherwise stay armed with a login password in it.
///
/// Asked at the point of use rather than enforced at each of the five or six sites that can break
/// it, so a field is armed only while both facts are true, by construction.
fn focus_is_still_armed(field: &FocusedField, keyboard_focus: Option<&str>, its_surface_is_live: bool) -> bool {
    keyboard_focus == Some(field.surface_id.as_str()) && its_surface_is_live
}
/// Whether a `lock` surface's resolved tree can actually be authenticated out of -- the predicate
/// `lock::lock_command`'s `can_authenticate` reads, and it is deliberately built out of
/// [`sole_secure_submit`] rather than out of [`secure_submit_targets`].
///
/// The guard that grants the lock and the rule that arms the keyboard must be one predicate. They
/// were two: admission asked whether any field in the tree unlocks, focus armed only a sole field.
/// A lock screen with two `secure_submit` fields passed the guard, took the lock -- which the
/// compositor will not release when the client dies -- and then armed nothing when the compositor
/// handed the surface keyboard focus, leaving a VT switch as the only way back in.
///
/// Sole-and-unlocking is the right rule, not merely the stricter one: `any` is not implementable
/// as a focus rule at all, since with two destinations there is no non-arbitrary answer to "whose
/// password is this?" (the guess [`submit_frame_for`] already refuses to make, docs/adr/0050
/// decision 4). So the focus rule stays, and admission moves to meet it.
pub(crate) fn tree_can_authenticate(tree: &layout::ResolvedNode) -> bool {
    sole_secure_submit(tree).as_ref().is_some_and(unlocks_the_session)
}
/// What one key event does to a focused `secure_submit` field.
///
/// Borrowed rather than owned so the decision costs no allocation: the `String` only ever exists
/// because SCTK already built one on the `KeyEvent`.
#[derive(Debug, PartialEq, Eq)]
enum SecureKeyAction<'a> {
    Append(&'a str),
    Backspace,
    /// Escape: throw the whole entry away and stay in the field.
    Clear,
    Submit,
    Ignore,
}
/// One `wl_keyboard` key, as an edit to a focused `secure_submit` buffer.
///
/// Why the keyboard and not `zwp_text_input_v3`: text-input-v3 only produces a `commit_string`
/// when the compositor has an input method bound to the seat, so on an ordinary session with no
/// IME running, no byte reached `shared::SecureBuffer` and a granted lock could not be
/// authenticated out of at all. It is also the security-correct transport independently of that:
/// a password must not be routed through an input method, which is why swaylock and hyprlock read
/// xkb directly too.
///
/// The `zwp_text_input_v3` binding is gone entirely. Keeping it would have left two independent
/// writers on one `shared::SecureBuffer`, with an IME able to land a character through both, and a
/// live `ContentPurpose::Password` session open beside the keyboard reader -- exactly what
/// docs/adr/0027's amendment says must never see a password. Deleted rather than left dormant: a
/// dormant enabled text-input object is still an IME session the compositor may route keystrokes
/// into.
///
/// docs/adr/0027 still records the right design for the *other* field kind: an ordinary
/// Lua-readable `textfield` with `on_change`/`on_submit` (§ 5.2 item 8's unmasked half) needs IME
/// composition and must not be a raw keysym reader, binds without `ContentPurpose::Password`, and
/// shares nothing with this path but the node kind.
///
/// This adds no IDL surface: the bytes go into a native buffer and out to the Supervisor, the whole
/// definition of a `secure_submit` field (docs/adr/0005), and a key that does not land in one is
/// [`Ignore`d]. § 5.2 still declares no `on_key`.
///
/// [`Ignore`d]: SecureKeyAction::Ignore
///
/// Control characters are filtered by their text, not an allow-list of keysyms: `utf8` is `Some`
/// for Escape, Tab and Return alike (xkbcommon hands back the C0 control character), so an
/// unfiltered append would bury an ESC byte inside a secret and leave PAM rejecting it for no
/// visible reason.
///
/// `repeat` exists for one case: a held Enter must not submit twice. A submit zeroizes the buffer
/// as it reads it (see [`secure_submit_frame`]), so a repeat would send an empty password to PAM
/// and spend one of the user's attempts on it.
fn secure_key_action<'a>(event: &'a KeyEvent, repeat: bool) -> SecureKeyAction<'a> {
    match event.keysym {
        Keysym::Return | Keysym::KP_Enter => {
            if repeat {
                SecureKeyAction::Ignore
            } else {
                SecureKeyAction::Submit
            }
        }
        Keysym::BackSpace => SecureKeyAction::Backspace,
        // Escape used to fall through to the control-character filter and be ignored, leaving one
        // Backspace per character as the only way to abandon a mistyped password -- costly on a
        // surface where a wrong attempt is counted by PAM. Every other password prompt clears on
        // Escape; so does this one.
        Keysym::Escape => SecureKeyAction::Clear,
        _ => match event.utf8.as_deref() {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => SecureKeyAction::Append(text),
            _ => SecureKeyAction::Ignore,
        },
    }
}
/// The name `on_click`'s second argument carries for one evdev button code, or `None` for a button
/// this engine does not hand to Lua at all.
///
/// A string, not the raw `273` or a normalized `1`/`2`/`3`: every categorical value crossing this
/// boundary already is one (`fit`, `layer`, `anchor`, `align_h`). docs/adr/0050's second amendment
/// argues the rest and is the place to change if revisited.
///
/// `None` means the press never arms and the release never fires, matching an unhandled button.
/// The set that fires has to equal the set a config can name: handed `"other"` for `BTN_TASK`, a
/// config cannot tell it from `BTN_EXTRA` and would run whatever was written for the left button.
///
/// ponytail: back and forward do nothing, on a mouse that has them. Three of the eight `BTN_*`
/// codes `smithay_client_toolkit::seat::pointer` names are handled here and the other five are
/// dropped, which costs a five-button mouse its two thumb buttons. Adding them is not a rename:
/// real mice emit `BTN_SIDE` (0x113) and `BTN_EXTRA` (0x114) for back and forward, while
/// `BTN_BACK` (0x116) and `BTN_FORWARD` (0x115) carry the literal names and are rarer, so a correct
/// mapping is four codes onto two names and there is no caller to check it against yet. Map both
/// pairs with the first config that asks; this function is the only place that changes.
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some("left"),
        BTN_RIGHT => Some("right"),
        BTN_MIDDLE => Some("middle"),
        _ => None,
    }
}
/// Whether a release of `button` ends the press `armed` is holding, whether or not it completes it.
///
/// The narrower half of the pair with [`release_completes_click`]. Completing needs the same
/// surface, rect and button; ending needs only the same button, since dragging off the node and
/// releasing ends the press exactly as clicking does. A release of a different button must not end
/// it: pressing left, pressing right, then releasing left used to clear the slot and lose the
/// right click as well as the left one.
fn release_ends_press(armed: Option<&ArmedClick>, button: u32) -> bool {
    armed.is_some_and(|armed| armed.button == button)
}
/// Whether a release on `instance_id`, over the button at `released_on`, completes `armed`
/// (docs/adr/0050 decision 2).
///
/// Both halves must be a button hit, not merely the same coordinates: a release landing in the
/// armed rect but on something no longer a handled button (a re-resolve put a plain `rect` there)
/// is not the click the press started. `released_on` is [`clickable_button`]'s answer for the
/// release, not the raw pointer position.
fn release_completes_click(armed: Option<&ArmedClick>, instance_id: &str, released_on: Option<LogicalRect>, button: u32) -> bool {
    match (armed, released_on) {
        (Some(armed), Some(rect)) => armed.instance_id == instance_id && armed.rect == rect && armed.button == button,
        _ => false,
    }
}
/// `on_click`'s single argument: the button's rect as `{ x, y, width, height }` in its surface's
/// logical coordinates (docs/adr/0050 decision 3).
///
/// The rect travels to the callback, not back from it. § 6's `popup` entry says the anchor rect is
/// "passed straight from the rect `button`'s `on_click` hands back", which is the round trip
/// through the config: `on_click = function(rect) menu_anchor:set(rect) end`, with the `popup`
/// declaring `anchor_rect = menu_anchor`. `Err` names the step as well as the error: a rect table
/// this engine could not build is the engine's bug, a handler that raised is the config's, and
/// merging them would send a config author looking at their own Lua for a fault that isn't there.
fn call_on_click(lua: &Lua, on_click: &Function, rect: LogicalRect, button: &str) -> Result<(), (&'static str, mlua::Error)> {
    let argument = rect_table(lua, rect).map_err(|e| ("could not build on_click's rect argument", e))?;
    on_click.call::<()>((argument, button)).map_err(|e| ("on_click raised, ignoring it", e))
}
/// `on_click`'s first argument: the button's rect as `{ x, y, width, height }` in its surface's
/// logical coordinates (docs/adr/0050 decision 3).
pub(super) fn rect_table(lua: &Lua, rect: LogicalRect) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", rect.x)?;
    table.set("y", rect.y)?;
    table.set("width", rect.width)?;
    table.set("height", rect.height)?;
    Ok(table)
}
/// Builds one `RendererFrame::SecureSubmit` out of `buffer` (ADR-0005/ADR-0027).
///
/// The one sanctioned read (`expose_secret`) and the explicit `.zeroize()` of the source buffer
/// sit on adjacent lines here, so the accumulated secret stops existing the instant it has been
/// copied into the outgoing envelope -- not left to `Drop`, and not left live while the frame
/// travels to the socket thread. The frame's own plaintext copy is the socket thread's to scrub,
/// immediately after its wire write (`crate::socket`'s `pump`).
///
/// A free function, not a `&mut self` method, for [`retarget_secure_submit`]'s reason: it makes the
/// whole read/zeroize contract directly unit-testable, which nothing involving a live `wl_surface`
/// is.
fn secure_submit_frame(generation_id: u32, capability: &str, action: &str, buffer: &mut shared::SecureBuffer) -> RendererFrame {
    let frame = RendererFrame::SecureSubmit(SecureSubmit {
        generation_id,
        capability: capability.to_string(),
        action: action.to_string(),
        secret: buffer.expose_secret().to_vec(),
    });
    buffer.zeroize();
    frame
}

impl App {
    /// Every write to `focused_secure_submit` in this file, funnelled so [`retarget_secure_submit`]
    /// gets to enforce the buffer's lifetime. See that function for the leak this closes; assigning
    /// the field directly anywhere else reopens it.
    fn focus_secure_submit(&mut self, next: Option<FocusedField>) {
        retarget_secure_submit(&mut self.focused_secure_submit, &mut self.secure_buffer, next);
    }

    /// Whether `instance_id` is still a surface this process has a live `wl_surface` for.
    ///
    /// `TrackedRole::wl_surface` is the whole test, and it is the right one because it answers
    /// `None` for both shapes a gone surface takes here: the entry removed outright
    /// ([`App::destroy_surface_by_id`]) and the entry kept with its role object dropped
    /// ([`App::hide_window`], [`App::teardown_lock_surfaces`], [`App::drop_popup_object`]).
    fn surface_is_live(&self, instance_id: &str) -> bool {
        self.surfaces.iter().any(|tracked| tracked.surface_id == instance_id && tracked.role.wl_surface().is_some())
    }

    /// Drops the focused field, and the half-typed secret with it, the moment [`focus_is_still_armed`]
    /// stops holding -- through [`App::focus_secure_submit`], so the scrub is the same one every
    /// other transition gets.
    ///
    /// Called before every keystroke, which is what makes the rule load-bearing rather than
    /// advisory: nothing can reach `secure_buffer` through a focus that has gone stale, whatever
    /// took the surface away and whether or not a `leave` ever followed.
    fn prune_secure_focus(&mut self) {
        let armed = self
            .focused_secure_submit
            .as_ref()
            .is_some_and(|field| focus_is_still_armed(field, self.keyboard_focus.as_deref(), self.surface_is_live(&field.surface_id)));
        if self.focused_secure_submit.is_some() && !armed {
            eprintln!("[oblisk-renderer] the focused secure_submit field is no longer the one receiving keys; dropping it and scrubbing its buffer");
            self.focus_secure_submit(None);
        }
    }

    /// The half of [`App::prune_secure_focus`] that does not wait for a keystroke: a field whose
    /// surface this process destroyed is dropped, and its buffer scrubbed, on the next poll turn.
    ///
    /// Only the liveness clause, deliberately: `prune_secure_focus`'s other clause is about routing --
    /// which surface is receiving keys -- and it is only ever wrong at the moment a key arrives, which
    /// is where it is asked. Applying it once a turn would also disarm the field a press on a
    /// multi-field surface just chose, in the window before the compositor's matching `enter` lands,
    /// and `sole_secure_submit` cannot re-choose it (see [`focus_on_enter`]).
    ///
    /// What this buys is the residency ceiling defect 3 named: type a password on the lock screen, let
    /// the compositor send `finished`, and `teardown_lock_surfaces` destroys the `wl_surface` with no
    /// `leave` required to follow. Without this the plaintext would sit in `secure_buffer`, still
    /// addressed to `("lock", "authenticate")`, until some later keystroke happened to notice -- which
    /// on a session where the user walks away is never.
    pub(super) fn drop_secure_focus_if_its_surface_is_gone(&mut self) {
        let gone = self.focused_secure_submit.as_ref().is_some_and(|field| !self.surface_is_live(&field.surface_id));
        if gone {
            eprintln!("[oblisk-renderer] the surface holding the focused secure_submit field is gone; dropping it and scrubbing its buffer");
            self.focus_secure_submit(None);
        }
    }

    /// One key event applied to the focused `secure_submit` field, or nothing at all when no field
    /// is focused (docs/adr/0005).
    ///
    /// The focus check is the gate: `focused_secure_submit` is `Some` only when some field named a
    /// destination for the next secret, so a keystroke that reaches the buffer already has somewhere
    /// to be sent. A `textfield` with no `secure_submit` leaves it `None` (see [`focused_target`]),
    /// and a key arriving then is dropped rather than accumulated -- buffering a password for a field
    /// that can never submit it is a secret held for no reason.
    ///
    /// Nothing here touches Lua: the bytes go from the `KeyEvent` into a native `shared::SecureBuffer`
    /// and out to the Supervisor, and no Lua value is ever built from them.
    fn apply_secure_key(&mut self, event: &KeyEvent, repeat: bool) {
        // Before the gate, not after it: the gate reads `focused_secure_submit` alone, and a focus
        // whose surface is gone or is no longer the one receiving keys is exactly the state this key
        // must not be appended to (see [`focus_is_still_armed`]).
        self.prune_secure_focus();
        if self.focused_secure_submit.is_none() {
            return;
        }
        match secure_key_action(event, repeat) {
            SecureKeyAction::Append(text) => self.secure_buffer.push_str(text),
            // `pop_char` zeroizes the bytes it drops rather than only shortening the buffer, which
            // is what keeps a corrected character from staying readable in this process's heap for
            // the rest of the entry.
            SecureKeyAction::Backspace => {
                self.secure_buffer.pop_char();
            }
            // Through the seam in both directions, not the buffer directly: the scrub Escape wants
            // is the one `retarget_secure_submit` performs on a transition, and re-arming the
            // identical field immediately after leaves the user still in it, free to retype. A
            // fifth writer of `secure_buffer` with its own idea of clearing is what this file has
            // spent two reviews avoiding.
            SecureKeyAction::Clear => {
                let field = self.focused_secure_submit.clone();
                self.focus_secure_submit(None);
                self.focus_secure_submit(field);
            }
            SecureKeyAction::Submit => self.finish_secure_submit(),
            SecureKeyAction::Ignore => {}
        }
    }

    /// A completed `secure_submit`: builds the outgoing frame out of the accumulated buffer and
    /// queues it for the socket thread. [`submit_frame_for`] performs the one sanctioned read and
    /// leaves `self.secure_buffer` scrubbed and empty on either branch.
    ///
    /// [`submit_frame_for`] refuses on two counts and both end here: no focused destination
    /// (docs/adr/0050 decision 4) and an empty buffer. Logged rather than silent: a user who pressed
    /// enter deserves an explanation, and it is almost always a `textfield` missing its
    /// `secure_submit` table -- the empty case explains itself on the glass, since there is nothing
    /// in the field.
    fn finish_secure_submit(&mut self) {
        let target = self.focused_secure_submit.as_ref().map(|field| &field.target);
        let Some(frame) = submit_frame_for(self.generation_id, target, &mut self.secure_buffer) else {
            eprintln!(
                "[oblisk-renderer] secure_submit dropped: nothing had been typed, or no focused textfield named a capability/action to address it to; the buffer was zeroized and nothing was sent"
            );
            return;
        };
        if let Err(e) = self.outbound_tx.send(frame) {
            eprintln!("[oblisk-renderer] failed to queue SecureSubmit for the socket thread: {e}");
        }
    }

    /// One hit-test of surface `index` at `position`, answering both of [`PointerHit`]'s questions
    /// (docs/adr/0050 decisions 1 and 4).
    ///
    /// `position` is surface-local and logical, the space `layout::hit` walks `ResolvedNode::rect` in,
    /// so there is no conversion here. That holds only while `paint_surface` paints at scale `1.0` and
    /// nothing calls `wl_surface::set_buffer_scale`; docs/adr/0050's consequences name this as a
    /// caller that a future HiDPI change has to move together with `paint_surface` and
    /// `apply_input_region`.
    ///
    /// A surface with no resolved tree answers the same as a point that missed everything: no button,
    /// no focused destination.
    ///
    /// Owned on the way out, every part: `Scene::surface` clones into a `ResolvedNode` (the same
    /// property `paint_surface` relies on), so the borrow of `self.client` ends on that line, and both
    /// the `Function` and the target are cloned out of the local tree before it is dropped.
    fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit { button: None, focus: Ok(None) };
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(&tree, point);
        PointerHit {
            button: clickable_button(&path).map(|(rect, on_click)| (rect, on_click.clone())),
            focus: focused_target(&path),
        }
    }

    /// Calls one `button`'s `on_click` with its rect (docs/adr/0050 decision 3).
    ///
    /// A raise is logged against the surface it happened on and swallowed. A broken `on_click` is
    /// a config bug, and a config bug must not take a shell that is otherwise painting down with
    /// it; docs/adr/0046's rescue path is for an evaluation that failed, not for one misbehaving
    /// handler, so this deliberately does not set `self.exit` and deliberately does not enter
    /// rescue.
    fn fire_on_click(&mut self, instance_id: &str, rect: LogicalRect, button: &str, on_click: &Function) {
        // Nothing marks the scene dirty here. A handler that changes what is painted does it by
        // writing a `state(name, initial)` signal, and `signal:set()` marks the flag itself
        // (ADR-0044 decision 5); a handler that writes nothing correctly causes no re-resolve.
        if let Err((what, e)) = call_on_click(self.client.lua(), on_click, rect, button) {
            eprintln!("[oblisk-renderer] {instance_id}: {what}: {e}");
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    /// Pointer and keyboard, and no touch object at all -- § 5.2 has no touch-specific property for
    /// one to serve.
    ///
    /// Idempotent by the `is_none` guards, not by trusting the compositor: `wl_seat::capabilities`
    /// is a full re-statement of the current set on every change, so a seat that gains a keyboard
    /// re-announces its pointer, and SCTK turns each announcement into this call. A second
    /// `wl_pointer` would deliver duplicate events into one `armed` slot, and a second
    /// `wl_keyboard` two `enter`/`leave` streams into one `keyboard_focus`.
    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                // Not fatal: a shell with no pointer still paints, still reloads, and still takes
                // `wp-text-input-v3` input. Only `on_click` stops working, which is what this says.
                Err(e) => eprintln!("[oblisk-renderer] wl_seat::get_pointer failed; no button's on_click will ever fire: {e}"),
            },
            // `None` rmlvo: take the compositor's own keymap. This shell never interprets a keysym
            // (there is no `on_key` in § 5.2), so imposing a layout of its own would be policy
            // serving nothing.
            Capability::Keyboard if self.keyboard.is_none() => match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                // Also not fatal, and narrower than it looks: losing this loses the `enter`/`leave`
                // that clear a focused `textfield`, so a stale focus can outlive the user moving on.
                Err(e) => eprintln!("[oblisk-renderer] wl_seat::get_keyboard failed; keyboard focus will never be tracked: {e}"),
            },
            _ => {}
        }
    }

    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat, capability: Capability) {
        match capability {
            Capability::Pointer => {
                // A pointer that is gone will never send the `release` this press was waiting for,
                // which is the same reason `leave` clears it (docs/adr/0050 decision 2).
                self.armed = None;
                if let Some(pointer) = self.pointer.take() {
                    // `wl_pointer::release` is `since="3"`; below that the destructor does not exist
                    // and dropping the proxy is the whole cleanup. Same guard SCTK's own
                    // `ThemedPointer::drop` applies (src/seat/pointer/mod.rs:572).
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
            }
            Capability::Keyboard => {
                // No keyboard means nothing will ever report the user leaving, so the focus this
                // was holding is stale from here on (docs/adr/0050 decision 4), and whatever was
                // half-typed into it goes with it ([`App::focus_secure_submit`]).
                self.keyboard_focus = None;
                self.focus_secure_submit(None);
                if let Some(keyboard) = self.keyboard.take() {
                    // `wl_keyboard::release` is `since="3"` too (wayland.xml); same reasoning as
                    // the pointer's guard directly above.
                    if keyboard.version() >= 3 {
                        keyboard.release();
                    }
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}
}

/// Pointer input to `on_click` (docs/adr/0050). See `delegate_dispatch2!(App)` at the bottom of
/// this file for why no `delegate_pointer!` call accompanies this.
impl PointerHandler for App {
    fn pointer_frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            // A surface this process does not own: a `wl_pointer` is per seat, not per surface,
            // and nothing stops the compositor from having delivered an event for a surface that
            // has since been destroyed by a `visible` flip or an output change.
            let Some(index) = self.index_of_surface(&event.surface) else {
                continue;
            };
            match event.kind {
                // Left, right and middle (docs/adr/0050's second amendment). Any other code is not
                // a button a config can name, so it arms nothing and fires nothing, which is what
                // decision 2's original `BTN_LEFT`-only match did for every code but one.
                PointerEventKind::Press { button, serial, .. } => {
                    if pointer_button_name(button).is_none() {
                        continue;
                    }
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // docs/adr/0049's amendment: armed here, read by this turn's re-resolve if one
                    // creates a popup, cleared by `run`'s poll loop at the end of the turn either
                    // way. See [`ArmedSerial`] for why both edges of a click arm it.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    // docs/adr/0051's first amendment, counted on exactly the events that arm the
                    // serial: the same "the user asked again" fact, kept past the end-of-turn disarm.
                    self.pointer_input_count += 1;
                    let hit = self.hit_under(index, event.position);
                    // The press decides focus, not the release: decision 4 says a press whose path
                    // holds a `textfield` focuses it, and a press that lands anywhere else clears
                    // it. A malformed `secure_submit` is the config's bug, not this shell's, so it
                    // is logged against the surface and treated as no destination -- refusing to
                    // guess a capability is the same call `focused_target` documents.
                    let focus = match hit.focus {
                        Ok(target) => target,
                        Err(e) => {
                            eprintln!("[oblisk-renderer] {instance_id}: textfield has a malformed secure_submit, so it takes focus with no destination: {e}");
                            None
                        }
                    }
                    // Bound to the surface the press landed on, per [`FocusedField`]: a field armed
                    // here stays armed only while that surface is both alive and the one the
                    // compositor is sending keys to.
                    .map(|target| FocusedField { surface_id: instance_id.clone(), target });
                    // Through the seam, because this is the site that *reassigns* rather than
                    // clears: a press moving from one `textfield` to another is the direct A-to-B
                    // transition [`retarget_secure_submit`] exists for.
                    self.focus_secure_submit(focus);
                    self.armed = hit.button.map(|(rect, _)| ArmedClick { instance_id, rect, button });
                }
                PointerEventKind::Release { button, serial, .. } => {
                    let Some(name) = pointer_button_name(button) else {
                        continue;
                    };
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // Overwrites the press's, and that is the point: a click fires on the release
                    // (docs/adr/0050 decision 2), so a popup opened by `on_click` is opened by
                    // *this* event and carries this serial.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    self.pointer_input_count += 1;
                    // Focus is untouched here. The press already decided it, and a release that
                    // drags off a `textfield` must not un-focus the field the user is typing into.
                    let hit = self.hit_under(index, event.position).button;
                    let fires = release_completes_click(self.armed.as_ref(), &instance_id, hit.as_ref().map(|(rect, _)| *rect), button);
                    // Before the call, so a handler that re-enters here cannot find its own press
                    // still armed. See [`release_ends_press`] for why this is not unconditional.
                    if release_ends_press(self.armed.as_ref(), button) {
                        self.armed = None;
                    }
                    if let Some((rect, on_click)) = hit.filter(|_| fires) {
                        self.fire_on_click(&instance_id, rect, name, &on_click);
                    }
                }
                // The pointer left the surface, so the release (if it ever comes) lands somewhere
                // else. This is the drag-off-and-cancel decision 2 is built around.
                PointerEventKind::Leave { .. } => self.armed = None,
                // `Enter`/`Motion`/`Axis`: nothing in § 5.2 reads hover or scroll yet
                // (build-steps.md section 6 ranks both), and a motion that leaves the armed rect
                // deliberately does *not* disarm -- dragging back onto the button and releasing
                // still clicks it, which is what every toolkit does.
                _ => {}
            }
        }
    }
}

/// Which surface the compositor gave keyboard focus to. `keyboard_interactivity` is the client's
/// half of this: it tells the compositor whether a surface may be focused at all. `wl_keyboard`'s
/// `enter`/`leave` is the only way the client learns what the compositor decided. See
/// `delegate_dispatch2!(App)` at the bottom of this file for why no `delegate_keyboard!` call
/// accompanies this.
impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        // `raw`/`keysyms` are the keys already held down when focus arrived. Nothing reads a key
        // here, so they are dropped along with every other key event below.
        //
        // A `wl_keyboard` is per seat, not per surface, so an `enter` can name a surface this
        // process destroyed since the compositor sent it (a `visible` flip, an output change); that
        // is the `None` below and it is not an error.
        self.keyboard_focus = self.surface_id_for(surface).map(str::to_string);
        // `Scene::surface` hands back an owned tree, so the borrow of `self.client` ends on this
        // line and the write below is free to take `&mut self`.
        let tree = self.keyboard_focus.as_ref().and_then(|id| self.client.scene().surface(id));
        // The rule that makes a lock screen typable with no click: keyboard focus on a surface
        // declaring exactly one `secure_submit` field focuses that field (see [`sole_secure_submit`]
        // for why exactly one, and why this is needed at all).
        let next = focus_on_enter(self.keyboard_focus.as_deref(), tree.as_ref(), self.focused_secure_submit.as_ref());
        match (&self.keyboard_focus, &next) {
            (None, _) => eprintln!("[oblisk-renderer] keyboard focus entered an untracked surface; not tracking it"),
            (Some(id), Some(field)) => eprintln!(
                "[oblisk-renderer] {id}: keyboard focus takes its `secure_submit` field ({}/{})",
                field.target.capability, field.target.action
            ),
            (Some(id), None) => eprintln!("[oblisk-renderer] keyboard focus entered {id}, which declares no sole `secure_submit` field"),
        }
        // Unconditional, and that is defect 2. Both "nothing to arm" cases used to be early returns
        // that moved `keyboard_focus` on and left the previous surface's field armed with its
        // half-typed secret, which `apply_secure_key` would then go on appending to and submitting
        // to that surface's capability. A focus that arms nothing has to *disarm*.
        self.focus_secure_submit(next);
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        // Unconditional, ignoring which surface is named: the protocol orders `leave` on the old
        // surface before `enter` on the new one, so there is no interleaving where clearing here
        // would drop a focus that had already moved on.
        let left = self.keyboard_focus.take().unwrap_or_else(|| "an untracked surface".to_string());
        // docs/adr/0050 decision 4's third clearing source. The user is demonstrably somewhere
        // else, so the `textfield` stops owning the next secret and the armed press will never see
        // its release -- the same answer `PointerEventKind::Leave` gives for the same reason. The
        // scrub that used to live on `zwp_text_input_v3`'s `leave` is now this call's, and it is the
        // load-bearing half: no submit is coming for those bytes.
        self.focus_secure_submit(None);
        self.armed = None;
        eprintln!("[oblisk-renderer] keyboard focus left {left}");
    }

    // A key reaches exactly one place and it is not a config: § 5.2 declares no key-handler
    // property on any node, and docs/adr/0050's consequences say this ADR does not invent one. What
    // it does have is the `secure_submit` field docs/adr/0005 defines: [`App::apply_secure_key`]
    // pushes bytes into a native `shared::SecureBuffer` and out to the Supervisor without a Lua
    // value ever existing, adding no IDL surface. See [`secure_key_action`] for why this is the
    // keyboard and not text-input.
    fn press_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, event: KeyEvent) {
        self.apply_secure_key(&event, false);
    }

    fn repeat_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, event: KeyEvent) {
        self.apply_secure_key(&event, true);
    }

    // Genuinely empty, and the two below with it: a release carries no `utf8` at all (SCTK's own
    // `KeyEvent` doc says so), and neither a modifier latch nor a layout change edits a buffer.
    // They exist because `KeyboardHandler` has no default bodies for them.
    fn release_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, _event: KeyEvent) {}

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_submit_frame_carries_the_accumulated_secret_and_zeroizes_the_buffer_it_read() {
        // build-steps.md Phase 15 item 2 / ADR-0005/ADR-0027: the frame carries the exact secret
        // this thread accumulated, tagged with this process's own generation_id, and the source
        // buffer is scrubbed in the same breath as the read rather than left live.
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        let frame = secure_submit_frame(4, "polkit", "authenticate", &mut buffer);

        assert_eq!(
            frame,
            RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            })
        );
        assert!(buffer.is_empty(), "the source SecureBuffer must be zeroized as soon as it has been read");
    }

    fn hit_node(lua: &Lua, kind: &str, (x, y, width, height): (f32, f32, f32, f32), on_click: bool) -> layout::ResolvedNode {
        let mut properties = HashMap::new();
        if on_click {
            properties.insert("on_click".to_string(), Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        }
        layout::ResolvedNode {
            kind: kind.to_string(),
            rect: LogicalRect { x, y, width, height },
            visible: true,
            properties,
            children: Vec::new(),
        }
    }

    #[test]
    fn the_innermost_handled_button_under_the_pointer_is_the_one_that_would_fire() {
        // The shape decision 1 exists for: the deepest node is the `text`, and the outer `row`
        // is not a button, so only the middle node answers.
        let lua = Lua::new();
        let mut button = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), true);
        button.children.push(hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), false));
        let mut row = hit_node(&lua, "row", (0.0, 0.0, 100.0, 32.0), false);
        row.children.push(button);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(row);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let (rect, _) = clickable_button(&path).expect("the button carries an on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    #[test]
    fn a_button_with_no_on_click_is_transparent_rather_than_a_barrier() {
        // An unhandled `button` nested inside a handled one must not swallow the click: the scan
        // keeps walking outwards past it.
        let lua = Lua::new();
        let inner = hit_node(&lua, "button", (5.0, 2.0, 20.0, 20.0), false);
        let mut outer = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), true);
        outer.children.push(inner);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(outer);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert_eq!(path.len(), 3, "the inner button is still on the path");
        let (rect, _) = clickable_button(&path).expect("the outer button carries the on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    #[test]
    fn an_on_click_that_is_not_a_function_is_not_a_click_handler() {
        // Nothing in `layout::node` parses this key (§ 5.2 leaves it opaque), so a config writing
        // `on_click = "quit"` reaches here as a string and must simply not fire.
        let lua = Lua::new();
        let mut button = hit_node(&lua, "button", (0.0, 0.0, 40.0, 24.0), false);
        button.properties.insert("on_click".to_string(), Value::String(lua.create_string("quit").unwrap()));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(button);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert!(clickable_button(&path).is_none());
    }

    #[test]
    fn a_release_fires_only_over_the_same_surface_and_the_same_rect_the_press_armed() {
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let moved = LogicalRect { x: 11.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, button: BTN_LEFT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_LEFT));
        // Dragged off the button, then released: the release hits no button at all.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", None, BTN_LEFT));
        // Dragged onto a different button on the same surface.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(moved), BTN_LEFT));
        // Same button geometry, different surface -- two panels can resolve identical rects.
        assert!(!release_completes_click(Some(&armed), "notification_area@eDP-1", Some(rect), BTN_LEFT));
        // A release with nothing armed (a press that hit no button, or a `leave` in between).
        assert!(!release_completes_click(None, "bar@eDP-1", Some(rect), BTN_LEFT));
    }

    #[test]
    fn only_the_three_buttons_a_config_can_name_are_handled_at_all() {
        assert_eq!(pointer_button_name(BTN_LEFT), Some("left"));
        assert_eq!(pointer_button_name(BTN_RIGHT), Some("right"));
        assert_eq!(pointer_button_name(BTN_MIDDLE), Some("middle"));
        // A side button, a browser-back button, and a code off the end of the mouse range: each
        // answers `None`, since a config cannot tell them apart.
        assert_eq!(pointer_button_name(0x113), None);
        assert_eq!(pointer_button_name(0x116), None);
        assert_eq!(pointer_button_name(0), None);
    }

    #[test]
    fn a_release_ends_only_its_own_buttons_press() {
        // The bug this exists to stop: press left, press right, release left, release right, all
        // on one node. If the left release clears the slot, the right press is thrown away with
        // it and the right click never fires despite being a complete pair.
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, button: BTN_RIGHT };

        assert!(release_ends_press(Some(&armed), BTN_RIGHT));
        assert!(!release_ends_press(Some(&armed), BTN_LEFT));
        // Ending it does not depend on it having fired: dragging off the node and releasing the
        // same button is over too, and the slot has to go.
        assert!(!release_ends_press(None, BTN_RIGHT));
    }

    #[test]
    fn a_release_fires_only_for_the_button_the_press_armed() {
        // Press right, release left, on the same node: two different clicks interleaved, and
        // neither completed. A mouse can hold more than one button down at a time, so this is a
        // real sequence rather than a hypothetical one.
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, button: BTN_RIGHT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_RIGHT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_MIDDLE));
    }

    /// One `secure_submit` destination, as the parsers hand it back.
    fn target(capability: &str, action: &str) -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: capability.to_string(), action: action.to_string() }
    }

    /// A focused field as [`App::focus_secure_submit`] stores one: a destination *and* the instance
    /// id of the surface it was declared on.
    fn field(surface_id: &str, capability: &str, action: &str) -> FocusedField {
        FocusedField { surface_id: surface_id.to_string(), target: target(capability, action) }
    }

    /// A `textfield` node carrying whatever the config wrote under `secure_submit`; `None` writes
    /// nothing, which is § 5.2 item 8's "optional even on a masked field".
    fn textfield(lua: &Lua, secure_submit: Option<Value>) -> layout::ResolvedNode {
        let mut node = hit_node(lua, "textfield", (0.0, 0.0, 40.0, 24.0), false);
        if let Some(value) = secure_submit {
            node.properties.insert("secure_submit".to_string(), value);
        }
        node
    }

    fn secure_submit_table(lua: &Lua, capability: &str, action: &str) -> Value {
        let table = lua.create_table().unwrap();
        table.set("capability", capability).unwrap();
        table.set("action", action).unwrap();
        Value::Table(table)
    }

    #[test]
    fn a_press_landing_on_no_textfield_leaves_no_destination_focused() {
        let lua = Lua::new();
        let button = hit_node(&lua, "button", (0.0, 0.0, 40.0, 24.0), true);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert_eq!(focused_target(&[&root, &button]).unwrap(), None);
    }

    #[test]
    fn the_innermost_textfield_on_the_path_is_the_one_that_owns_the_next_secret() {
        // Same deep-end scan `clickable_button` makes, and for the same reason (docs/adr/0050
        // decision 1): one traversal, two questions.
        let lua = Lua::new();
        let outer = textfield(&lua, Some(secure_submit_table(&lua, "outer", "ignored")));
        let inner = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);

        assert_eq!(
            focused_target(&[&root, &outer, &inner]).unwrap(),
            Some(node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() })
        );
    }

    #[test]
    fn a_textfield_with_no_secure_submit_focuses_with_no_destination() {
        let lua = Lua::new();
        let field = textfield(&lua, None);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert_eq!(focused_target(&[&root, &field]).unwrap(), None);
    }

    #[test]
    fn moving_focus_between_two_secure_submit_fields_zeroizes_what_the_first_accumulated() {
        // The credential leak this seam closes: the lock screen's `("lock", "authenticate")` field
        // accumulates a login password, focus moves to the bar's `("network", "connect")` field
        // without an Enter in between, and the next submit used to carry `<login password><psk>`
        // to the network capability.
        let mut focused = Some(field("screen@DP-1", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, Some(field("bar@DP-1", "network", "connect")));

        assert_eq!(focused, Some(field("bar@DP-1", "network", "connect")));
        assert!(buffer.is_empty(), "a password typed for one destination must not reach the next one's capability");
    }

    #[test]
    fn the_same_destination_on_a_different_surface_is_a_different_field() {
        // The surface half of the identity is load-bearing: two surfaces may both declare
        // `("lock", "authenticate")` -- a lock screen on each of two monitors does. With the
        // destination alone as the identity, focus moving between them compared equal and the
        // scrub was skipped.
        let mut focused = Some(field("screen@eDP-1", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, Some(field("screen@DP-1", "lock", "authenticate")));

        assert!(buffer.is_empty(), "a field is its surface as well as its destination");
    }

    #[test]
    fn clearing_focus_zeroizes_the_buffer_and_re_focusing_the_same_field_does_not() {
        // Two halves of the same rule. Clearing is `leave`/`capability_lost`, where no submit is
        // ever coming for the bytes. Re-arming the same destination is a press landing in the field
        // already being typed into (docs/adr/0050 decision 4), and wiping there would delete half
        // a password mid-entry.
        let mut focused = Some(field("screen@TEST", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, None);
        assert_eq!(focused, None);
        assert!(buffer.is_empty(), "focus leaving with no submit must scrub what it accumulated");

        focused = Some(field("screen@TEST", "lock", "authenticate"));
        buffer.push_str("hunter2");
        retarget_secure_submit(&mut focused, &mut buffer, Some(field("screen@TEST", "lock", "authenticate")));
        assert_eq!(buffer.expose_secret(), b"hunter2", "re-focusing the same field must not eat the entry in progress");
    }

    #[test]
    fn a_malformed_secure_submit_is_an_error_rather_than_a_guessed_destination() {
        let lua = Lua::new();
        let field = textfield(&lua, Some(Value::String(lua.create_string("polkit").unwrap())));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(focused_target(&[&root, &field]).is_err(), "a non-table secure_submit names no capability");
    }

    #[test]
    fn a_submit_with_a_focused_target_is_addressed_to_that_capability_and_action() {
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");
        let target = node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() };

        let frame = submit_frame_for(4, Some(&target), &mut buffer);

        assert_eq!(
            frame,
            Some(RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            }))
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_submit_with_no_focused_target_sends_nothing_and_still_zeroizes_the_buffer() {
        // docs/adr/0050 decision 4: addressing this to `"unknown"/"unknown"` would put a password
        // on the wire for no one. The scrub is the half that is not optional.
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        assert_eq!(submit_frame_for(4, None, &mut buffer), None);
        assert!(buffer.is_empty(), "a dropped submit must still leave the accumulated secret scrubbed");
    }

    #[test]
    fn an_enter_on_an_empty_field_sends_nothing() {
        // Not free: the Supervisor routes a `("lock", "authenticate")` submit straight into PAM, so
        // an Enter that said nothing spends one of the user's counted attempts.
        let mut buffer = shared::SecureBuffer::new();
        assert_eq!(submit_frame_for(4, Some(&target("lock", "authenticate")), &mut buffer), None);
    }

    #[test]
    fn keyboard_focus_arriving_on_nothing_typable_disarms_whatever_was_armed() {
        // Both of `KeyboardHandler::enter`'s "nothing to arm" cases, once early returns that left
        // the previous surface's field armed while keystrokes kept accumulating into it. `enter`
        // now pushes this answer through `App::focus_secure_submit` whatever it is.
        let lua = Lua::new();
        let untypable = tree_with(&lua, vec![textfield(&lua, None)]);
        let armed = field("screen@TEST", "lock", "authenticate");
        assert_eq!(focus_on_enter(None, None, Some(&armed)), None, "an `enter` on a surface this process already destroyed");
        assert_eq!(focus_on_enter(Some("bar@TEST"), Some(&untypable), Some(&armed)), None, "a surface whose tree names no destination");

        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert_eq!(focus_on_enter(Some("screen@TEST"), Some(&typable), None), Some(armed.clone()));

        // What an `enter` must *not* undo: a press on a surface declaring two fields picked one the
        // sole-field rule refuses to pick, and the compositor's `enter` for that surface commonly
        // follows the press that caused it.
        let two_fields = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        let pressed = field("screen@TEST", "polkit", "authenticate");
        assert_eq!(focus_on_enter(Some("screen@TEST"), Some(&two_fields), Some(&pressed)), Some(pressed));
        assert_eq!(
            focus_on_enter(Some("screen@TEST"), Some(&two_fields), Some(&field("bar@TEST", "network", "connect"))),
            None,
            "a field belonging to another surface is not this surface's to keep"
        );
    }

    #[test]
    fn a_field_is_armed_only_while_its_own_surface_holds_the_keyboard_and_still_exists() {
        // The one question every keystroke asks, in place of a clearing call bolted onto each of the
        // five or six sites that can take a surface away. The liveness half is the traced leak: type
        // a login password on the lock screen, the compositor sends `finished`,
        // `teardown_lock_surfaces` destroys the `wl_surface` with no `leave` required to follow, so
        // the plaintext used to stay live in `App::secure_buffer`.
        let armed = field("screen@TEST", "lock", "authenticate");
        assert!(focus_is_still_armed(&armed, Some("screen@TEST"), true));
        assert!(!focus_is_still_armed(&armed, Some("screen@TEST"), false), "its `wl_surface` is gone, whether or not a `leave` ever came");
        assert!(!focus_is_still_armed(&armed, Some("bar@TEST"), true), "another surface is the one receiving keys");
        assert!(!focus_is_still_armed(&armed, None, true), "the keyboard is on a surface this process does not own");
    }

    /// A `lock` tree as the scene hands one back: a root with the password field somewhere under it.
    fn tree_with(lua: &Lua, fields: Vec<layout::ResolvedNode>) -> layout::ResolvedNode {
        let mut root = hit_node(lua, "column", (0.0, 0.0, 1920.0, 1080.0), false);
        let mut inner = hit_node(lua, "column", (0.0, 0.0, 360.0, 200.0), false);
        inner.children = fields;
        root.children = vec![hit_node(lua, "label", (0.0, 0.0, 100.0, 20.0), false), inner];
        root
    }

    #[test]
    fn keyboard_focus_takes_the_one_secure_submit_field_a_surface_declares() {
        // The rule that makes a lock screen typable without a click: `focused_secure_submit` used
        // to be set only by a pointer press.
        let lua = Lua::new();
        let tree = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);

        assert_eq!(
            sole_secure_submit(&tree),
            Some(node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() })
        );
    }

    #[test]
    fn two_secure_submit_fields_on_one_surface_focus_neither() {
        // Deliberately not "the first one": with two destinations there is no non-arbitrary answer
        // to "whose password is this?", which is the same question `submit_frame_for` refuses to
        // guess at (docs/adr/0050 decision 4). A press still picks one, because a press names a node.
        let lua = Lua::new();
        let tree = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        assert_eq!(sole_secure_submit(&tree), None);

        // A field with no destination is not a candidate either -- it names nowhere to send to.
        let bare = tree_with(&lua, vec![textfield(&lua, None)]);
        assert_eq!(sole_secure_submit(&bare), None);
    }

    #[test]
    fn the_lock_admission_guard_and_the_keyboard_focus_rule_are_one_predicate() {
        // Defect D: `lock_command`'s `can_authenticate` asked whether any field unlocks, while
        // keyboard focus arms only a surface's sole field. A lock tree with two `secure_submit`
        // fields passed the guard, took the lock, and armed nothing on `enter` -- a keyboard-only
        // machine could then only leave the session by a VT switch.
        let lua = Lua::new();
        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert!(tree_can_authenticate(&typable));
        assert_eq!(sole_secure_submit(&typable), Some(target("lock", "authenticate")));

        let two_fields = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        assert!(!tree_can_authenticate(&two_fields), "a lock the keyboard cannot arm must not be granted the lock");
        assert_eq!(sole_secure_submit(&two_fields), None);

        // One field, but pointed somewhere the Supervisor does not route an unlock through.
        let wrong_destination = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")))]);
        assert!(!tree_can_authenticate(&wrong_destination));
    }

    #[test]
    fn only_the_lock_authenticate_pair_can_unlock_the_session() {
        // supervisor/src/main.rs routes `("lock", "authenticate")` to the PAM worker and nothing
        // else to it, so a lock screen whose field submits anywhere else can never unlock.
        assert!(unlocks_the_session(&target("lock", "authenticate")));
        assert!(!unlocks_the_session(&target("polkit", "authenticate")));
        assert!(!unlocks_the_session(&target("lock", "cancel")));
    }

    fn key(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent { time: 0, raw_code: 0, keysym, utf8: utf8.map(str::to_string) }
    }

    #[test]
    fn a_focused_secure_field_reads_the_keyboard_directly() {
        // `zwp_text_input_v3` alone did not deliver this: it only produces a `commit_string` when
        // the compositor has an input method bound, so on a session with no IME not one byte
        // reached `SecureBuffer`.
        assert_eq!(secure_key_action(&key(Keysym::a, Some("a")), false), SecureKeyAction::Append("a"));
        assert_eq!(secure_key_action(&key(Keysym::Return, Some("\r")), false), SecureKeyAction::Submit);
        assert_eq!(secure_key_action(&key(Keysym::KP_Enter, Some("\r")), false), SecureKeyAction::Submit);
        assert_eq!(secure_key_action(&key(Keysym::BackSpace, Some("\u{8}")), false), SecureKeyAction::Backspace);
    }

    #[test]
    fn a_control_key_never_becomes_a_character_of_the_password() {
        // `utf8` is not empty for Escape, Tab or Return -- xkbcommon hands back the C0 control
        // character for each -- so an unfiltered append would silently put an ESC byte in the
        // middle of a secret that PAM then rejects with no visible reason.
        assert_eq!(secure_key_action(&key(Keysym::Tab, Some("\t")), false), SecureKeyAction::Ignore);
        assert_eq!(secure_key_action(&key(Keysym::Shift_L, None), false), SecureKeyAction::Ignore);
    }

    #[test]
    fn escape_throws_the_entry_away_instead_of_being_ignored() {
        // Escape used to reach the control-character filter above and be dropped, which left one
        // Backspace per character as the only way to abandon a mistyped password -- on the surface
        // where a wrong guess costs a counted PAM attempt and a `pam_unix` failure delay.
        assert_eq!(secure_key_action(&key(Keysym::Escape, Some("\u{1b}")), false), SecureKeyAction::Clear);
    }

    #[test]
    fn holding_enter_down_does_not_resubmit_an_already_scrubbed_buffer() {
        // A submit zeroizes the buffer as it reads it, so the second submit of a key repeat would
        // send an *empty* password to PAM and burn one of the user's attempts. Backspace and
        // ordinary characters repeat normally, which is what every text field does.
        assert_eq!(secure_key_action(&key(Keysym::Return, Some("\r")), true), SecureKeyAction::Ignore);
        assert_eq!(secure_key_action(&key(Keysym::BackSpace, Some("\u{8}")), true), SecureKeyAction::Backspace);
        assert_eq!(secure_key_action(&key(Keysym::a, Some("a")), true), SecureKeyAction::Append("a"));
    }

    #[test]
    fn on_clicks_argument_is_the_buttons_rect_as_four_named_fields() {
        let lua = Lua::new();
        let table = rect_table(&lua, LogicalRect { x: 10.5, y: 4.0, width: 40.0, height: 24.0 }).unwrap();
        assert_eq!(table.get::<f32>("x").unwrap(), 10.5);
        assert_eq!(table.get::<f32>("y").unwrap(), 4.0);
        assert_eq!(table.get::<f32>("width").unwrap(), 40.0);
        assert_eq!(table.get::<f32>("height").unwrap(), 24.0);
    }

    #[test]
    fn on_click_takes_the_button_name_as_a_second_argument_beside_the_rect() {
        // Second argument, not a fifth field on the rect table: every handler written against the
        // one-argument form keeps working, since Lua drops arguments a function does not declare.
        let lua = Lua::new();
        let seen: Function = lua
            .load(r#"seen = {} return function(rect, button) seen.x, seen.w, seen.button = rect.x, rect.width, button end"#)
            .eval()
            .unwrap();
        call_on_click(&lua, &seen, LogicalRect { x: 12.0, y: 4.0, width: 40.0, height: 24.0 }, "right").unwrap();

        let recorded: Table = lua.globals().get("seen").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 12.0);
        assert_eq!(recorded.get::<f32>("w").unwrap(), 40.0);
        assert_eq!(recorded.get::<String>("button").unwrap(), "right");
    }

    #[test]
    fn a_one_argument_on_click_still_runs_unchanged() {
        // docs/adr/0050 decision 3's exact worked example, which every config in the tree uses.
        let lua = Lua::new();
        let anchor: Function = lua.load(r#"anchor = nil return function(rect) anchor = rect end"#).eval().unwrap();
        call_on_click(&lua, &anchor, LogicalRect { x: 40.0, y: 0.0, width: 86.0, height: 24.0 }, "left").unwrap();

        let recorded: Table = lua.globals().get("anchor").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 40.0);
        assert_eq!(recorded.get::<f32>("height").unwrap(), 24.0);
    }
}
