//! Pointer input (`on_click`, ADR-0050) and keyboard focus, including `secure_submit` accumulation:
//! keystrokes become a `SecureSubmit` frame with no Lua value ever holding the plaintext
//! (ADR-0005/ADR-0027). `SeatHandler`, `PointerHandler` and `KeyboardHandler` live here, beside the
//! pure helpers they call: which button armed a click, which `textfield` a press or `enter`
//! focuses, and what one key event does to the focused field.

use super::*;
use crate::layout::secure_submit::{secure_submit_targets, sole_secure_submit_in_scope};

/// What one notch of a mouse wheel scrolls, in logical pixels, when the compositor sends a step
/// count instead of a distance (ADR-0069 decision 6). A flat 39, the only chosen number in this
/// file: nothing here reads a font size, and a per-container step would need one the container does
/// not carry. It approximates three lines of the shipped config's 13px text. A touchpad never
/// reaches it, since it reports real pixels.
const WHEEL_STEP_PIXELS: f32 = 39.0;

/// How far one wheel event scrolls, in logical pixels (ADR-0069 decision 6). `pixels` is what a
/// touchpad sends, used as sent. `steps` is `value120` (120 per logical notch), all a mouse wheel
/// sends; consulted only when there is no distance, a compositor sending both being the same motion
/// twice. Its own function so the arithmetic is testable without a live `wl_pointer` and a
/// compositor to deliver a real notch.
fn wheel_delta(pixels: f64, steps: i32) -> f32 {
    if pixels != 0.0 {
        return pixels as f32;
    }
    steps as f32 / 120.0 * WHEEL_STEP_PIXELS
}

/// One press waiting for its release (ADR-0050 decision 2): a click is a press and a release on the
/// same node, so pressing, noticing the mistake, and dragging off releases harmlessly. "Same node"
/// is this pair, not a node identity: `ResolvedNode` has none (`NodeId` lives on `RetainedNode`,
/// dropped by `to_resolved`). The rect stands in for identity and gets the one "wrong" case right
/// anyway: a re-resolve that moves the button between press and release cancels the click, what a
/// real identity would also give for a button moved from under the pointer.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ArmedClick {
    instance_id: String,
    rect: LogicalRect,
    /// The evdev code the press carried, so the release must be the same button, not merely a
    /// button (ADR-0050's second amendment). ponytail: one armed click, so chording drops both; a
    /// second press overwrites `armed`. Upgrade: key `armed` by button (an `ArrayVec` of three, or
    /// a small map) when a config wants a chord.
    button: u32,
}
/// The serial `xdg_popup.grab` needs, plus the surface that carried it (ADR-0049's amendment,
/// ADR-0051 decision 1). Armed by [`PointerHandler::pointer_frame`], cleared by [`run`]'s poll loop
/// at the end of the same turn. Set in a field, not read off the dispatch stack, since a popup's
/// re-resolve runs in the poll loop after `dispatch_pending` returns, by which point that stack is
/// gone; resolving inside dispatch would nest `Scene::apply` and Lua/Wayland object creation inside
/// a `Dispatch` callback, reentering the queue being dispatched from. A press and a release both
/// arm it, latest wins, since a click fires on the release (ADR-0050 decision 2) and its serial is
/// what an `on_click` popup carries; `xdg_shell` only requires a serial from "a real input event,"
/// so a stale one still gets a normal `popup_done` refusal (ADR-0051 decision 3), not an error.
/// `instance_id`, not the tracked index, since an output change can rename indices between click
/// and re-resolve; the id is stable, matching what `is_instance_of` compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArmedSerial {
    pub(super) serial: u32,
    pub(super) instance_id: String,
}
/// The innermost `button` in a [`layout::hit::hit_path`] result carrying a callable `on_click`, as
/// that button's absolute rect and its function (ADR-0050 decision 1). Scans from the deep end: the
/// deepest node under the pointer is normally the `button`'s `text` child, which has no `on_click`.
/// A `button` without one is transparent, not a barrier, so a plain `button` nested inside a
/// handled one still lets the outer one fire. `on_click` must be a `Value::Function`; anything else
/// is not a click handler, and this is the only place that checks, since `layout::node` has no
/// parser for the key (§ 5.2 leaves it opaque).
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
/// and the destination the innermost `textfield` addresses the next secret to. One struct, not two
/// lookups: both come from one [`layout::hit::hit_path`] call, since walking twice risks a
/// re-resolve between them giving two answers to one event.
struct PointerHit {
    button: Option<(LogicalRect, Function)>,
    field: Option<FieldTarget>,
}
/// What the innermost `textfield` under a press turns out to be (ADR-0092). The two kinds share
/// the node kind and nothing else: one addresses a capability action and never lets its bytes near
/// Lua (ADR-0005), the other hands every edit straight to a Lua callback.
enum FieldTarget {
    Masked(node::SecureSubmitTarget),
    Plain {
        /// The field's own node, which is how paint finds it again across passes that move it
        /// (ADR-0099).
        id: layout::scene::NodeId,
        on_change: Option<Function>,
        on_submit: Option<Function>,
        on_cancel: Option<Function>,
    },
}
/// What the innermost `textfield` in a hit path is, if the press landed on one at all (ADR-0050
/// decision 4, § 5.2 item 8, ADR-0092).
///
/// A `secure_submit` table makes it masked and the target is the whole identity, since that is
/// where its bytes go. Without one it is a plain field, keyed by its box and carrying whichever of
/// `on_change`/`on_submit` the config declared. There is no third, malformed case: `paint_style`
/// parses the destination while `Scene::apply` resolves the node, so a `secure_submit` that fails
/// to parse fails the whole pass.
///
/// `None` for a `textfield` that is neither: masked with no destination has nowhere to send a
/// submit, and plain with no callback has nobody to tell. Focusing either would take the keyboard
/// away from a field that can use it, to buffer keystrokes nothing will ever read.
///
/// The plain half is identified by the node's `NodeId`, which `ResolvedNode` now carries
/// (ADR-0099) -- stable across a pass that moves the field, which its rect was not. [`ArmedClick`]
/// still uses a rect for the same job; a press and its release are one gesture and the tree rarely
/// moves between them, so that stand-in has not cost anything yet.
fn focused_field(path: &[&layout::ResolvedNode]) -> Option<FieldTarget> {
    let field = path.iter().rev().find(|node| node.kind == "textfield")?;
    let node::PaintStyle::TextField { target, .. } = field.paint.as_ref()? else {
        return None;
    };
    if let Some(target) = target {
        return Some(FieldTarget::Masked(target.clone()));
    }
    let function = |key: &str| match field.properties.get(key) {
        Some(Value::Function(f)) => Some(f.clone()),
        _ => None,
    };
    let (on_change, on_submit) = (function("on_change"), function("on_submit"));
    if on_change.is_none() && on_submit.is_none() {
        return None;
    }
    Some(FieldTarget::Plain { id: field.id, on_change, on_submit, on_cancel: function("on_cancel") })
}
/// The focused plain `textfield`: where it lives, what has been typed into it, and who to tell
/// (ADR-0092). The buffer is an ordinary `String` and deliberately so -- this is the half of § 5.2
/// item 8 whose whole purpose is that a config can read the text, the opposite of
/// [`FocusedField`].
#[derive(Debug, Clone)]
pub(super) struct FocusedTextField {
    surface_id: String,
    id: layout::scene::NodeId,
    buffer: String,
    on_change: Option<Function>,
    on_submit: Option<Function>,
    on_cancel: Option<Function>,
}

/// What one key did to a plain field's buffer, before any callback runs (ADR-0092, ADR-0102).
/// Split from [`App::apply_plain_key`] so the Escape rule can be tested without a Wayland seat.
#[derive(Debug, PartialEq, Eq)]
struct PlainEdit {
    /// The buffer's text is different from before, so `on_change` has something to say.
    changed: bool,
    /// Enter: `on_submit` fires with the text and the buffer is emptied.
    submitted: bool,
    /// Escape on a field that declared `on_cancel`: focus is dropped and `on_cancel` fires.
    cancelled: bool,
}

/// Applies `action` to `buffer`. Escape clears the buffer either way; whether it also gives the
/// field up depends on `cancels` -- whether the field declared `on_cancel`. Without one, clearing
/// and staying is the only honest answer, since the config could not be told the field stopped
/// taking keys (ADR-0092 decision 6). With one, it can, so Escape means what it means everywhere
/// else: leave.
fn edit_plain_buffer(buffer: &mut String, action: KeyAction<'_>, cancels: bool) -> PlainEdit {
    match action {
        KeyAction::Append(text) => {
            buffer.push_str(text);
            PlainEdit { changed: true, submitted: false, cancelled: false }
        }
        KeyAction::Backspace => PlainEdit { changed: buffer.pop().is_some(), submitted: false, cancelled: false },
        KeyAction::Clear => {
            let had = !buffer.is_empty();
            buffer.clear();
            PlainEdit { changed: had, submitted: false, cancelled: cancels }
        }
        KeyAction::Submit => PlainEdit { changed: true, submitted: true, cancelled: false },
        KeyAction::Ignore => PlainEdit { changed: false, submitted: false, cancelled: false },
    }
}
/// The frame a completed `wp-text-input-v3` submit produces, or `None` when no focused `textfield`
/// named a destination for it (ADR-0050 decision 4). `None` is the point: without it the submit
/// would address `"unknown"/"unknown"`, a password put on the wire for nobody, and sending nothing
/// is the only safe answer to "whose password is this?" The buffer is zeroized on both branches, so
/// a dropped submit never leaves the secret in `App`.
fn submit_frame_for(
    generation_id: u32,
    target: Option<&node::SecureSubmitTarget>,
    buffer: &mut shared::SecureBuffer,
) -> Option<RendererFrame> {
    // An empty buffer is not a password, and sending one is not free: the Supervisor routes it into
    // PAM, spending a counted attempt and a `pam_unix` failure delay on a keystroke that said
    // nothing. Checked before the destination, since it holds regardless of the destination.
    let Some(target) = target.filter(|_| !buffer.is_empty()) else {
        buffer.zeroize();
        return None;
    };
    Some(secure_submit_frame(generation_id, &target.capability, &target.action, buffer))
}
/// A focused `secure_submit` field, together with the surface whose tree declared it. The surface
/// id lets [`focus_is_still_armed`] tell a field on the surface holding the keyboard from one on a
/// surface this process destroyed: the protocol does not require a `wl_keyboard.leave` for a
/// client-destroyed surface, so the destination alone could not tell them apart (a lock-screen
/// password would otherwise sit in `App::secure_buffer`, addressed to `("lock", "authenticate")`,
/// with later bar keystrokes appending to it). Binding the field to its surface makes
/// [`focus_is_still_armed`] the one question every keystroke asks, rather than a clearing call
/// bolted onto each of the five or six sites that can take a surface away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FocusedField {
    /// The `"{id}@{output}"` instance id of the surface the field was declared on.
    surface_id: String,
    target: node::SecureSubmitTarget,
}
/// The one place `App::focused_secure_submit` is ever written: a `shared::SecureBuffer`'s lifetime
/// belongs to the field the bytes were typed into, not the transport that carried them. Three sites
/// reassign or clear focus (`KeyboardHandler::leave`, `SeatHandler::remove_capability`'s keyboard
/// arm, and the retargeting press in `PointerHandler::pointer_frame`); skipping the scrub at any
/// one lets a password reach [`submit_frame_for`] addressed to the next field's capability, exactly
/// the routing ADR-0005 exists to prevent. Enforced on the transition, so a fifth caller inherits
/// it by construction: any change of destination scrubs, including one field to another directly.
/// Re-arming the same destination deliberately does not, since a press decides focus
/// unconditionally (ADR-0050 decision 4) and clicking twice in the field being typed into must
/// leave it unchanged. A free function, not a `&mut self` method, so the read/zeroize contract is
/// unit-testable without a live Wayland connection (same reason as [`secure_submit_frame`]).
fn retarget_secure_submit(
    focused: &mut Option<FocusedField>,
    buffer: &mut shared::SecureBuffer,
    next: Option<FocusedField>,
) {
    if *focused != next {
        buffer.zeroize();
    }
    *focused = next;
}
/// What `focused_secure_submit` becomes when keyboard focus arrives, given the resolved trees of
/// [`App::keyboard_focus_scope`] and whatever is focused now. A total function: an empty scope (an
/// untracked surface, or none focused) and a scope with no sole `secure_submit` both answer `None`,
/// pushed through [`App::focus_secure_submit`] like any other result rather than leaving
/// `focused_secure_submit` untouched. `apply_secure_key` gates on focus alone, so a case that only
/// advanced `keyboard_focus` would leave keystrokes accumulating into the previous surface's field,
/// still addressed to its capability. What survives an `enter` is a field still inside the scope,
/// and only that: a press on a surface with several `secure_submit` fields picks one that
/// [`sole_secure_submit_in_scope`] refuses to, and the compositor's `enter` commonly follows that
/// press, so discarding it would make a multi-field surface untypable by clicking. Requiring the
/// tree to still declare that destination keeps a reload from pointing at a deleted field.
fn focus_on_enter(scope: &[(&str, &layout::ResolvedNode)], current: Option<&FocusedField>) -> Option<FocusedField> {
    let still_declared = |field: &&FocusedField| {
        scope.iter().any(|(id, tree)| *id == field.surface_id && secure_submit_targets(tree).contains(&field.target))
    };
    if let Some(current) = current.filter(still_declared) {
        return Some(current.clone());
    }
    let (surface_id, target) = sole_secure_submit_in_scope(scope)?;
    Some(FocusedField { surface_id: surface_id.to_string(), target })
}
/// Whether a focused field is still armed: its surface is one a keystroke now reaches, and still
/// exists as a live `wl_surface` in this process. Both clauses, neither redundant. The reachability
/// clause is defect 2: a pointer press arms focus on whatever surface it landed on, so without it a
/// field on a `keyboard_interactivity = none` panel stays armed while another surface actually
/// receives keys. The liveness clause is defect 3: a `wl_surface` this process destroyed may never
/// produce a `leave`, so a field on a torn-down lock screen would otherwise stay armed with a login
/// password in it. Asked at the point of use rather than enforced at each of the five or six sites
/// that can break it, so a field is armed only while both facts hold, by construction.
///
/// `scope`, not one `keyboard_focus` id, for the reason [`sole_secure_submit_in_scope`] takes one:
/// a field on a shown popup is reachable while the keyboard sits on the popup's parent. The two
/// must read the same scope or `enter` would arm a field the next keystroke immediately prunes.
fn focus_is_still_armed(field: &FocusedField, scope: &[String], its_surface_is_live: bool) -> bool {
    scope.contains(&field.surface_id) && its_surface_is_live
}
/// What one key event does to a focused `secure_submit` field. Borrowed rather than owned so the
/// decision costs no allocation: the `String` only ever exists because SCTK already built one on
/// the `KeyEvent`.
#[derive(Debug, PartialEq, Eq)]
enum KeyAction<'a> {
    Append(&'a str),
    Backspace,
    /// Escape: throw the whole entry away and stay in the field.
    Clear,
    Submit,
    Ignore,
}
/// One `wl_keyboard` key, as an edit to a focused `secure_submit` buffer. The keyboard, not
/// `zwp_text_input_v3`: text-input-v3 only produces a `commit_string` when an input method is
/// bound, so with no IME running no byte would reach `shared::SecureBuffer`; also security-correct
/// on its own, why swaylock and hyprlock read xkb directly too, since a password must not route
/// through an input method. The binding is gone entirely, not left dormant, since a dormant
/// text-input object is still an IME session the compositor may route keystrokes into, and keeping
/// it would put two independent writers on one `shared::SecureBuffer`, exactly what ADR-0027's
/// amendment forbids. ADR-0027 still covers the *other* field kind: an ordinary Lua-readable
/// `textfield` with `on_change`/`on_submit` (§ 5.2 item 8's unmasked half) needs IME composition
/// and shares nothing with this path but the node kind. This adds no IDL surface: the bytes go into
/// a native buffer and out to the Supervisor, the whole definition of a `secure_submit` field
/// (ADR-0005); a key that misses is [`Ignore`d] (§ 5.2 still declares no `on_key`).
///
/// [`Ignore`d]: KeyAction::Ignore
///
/// Control characters are filtered by their text, not a keysym allow-list: `utf8` is `Some` for
/// Escape, Tab and Return alike (xkbcommon hands back the C0 control character), so an unfiltered
/// append would bury an ESC byte in a secret, with PAM rejecting it for no visible reason. `repeat`
/// exists so a held Enter cannot submit twice: a submit zeroizes the buffer as it reads it (see
/// [`secure_submit_frame`]), so a repeat would send an empty password to PAM and spend an attempt.
fn key_action<'a>(event: &'a KeyEvent, repeat: bool) -> KeyAction<'a> {
    match event.keysym {
        Keysym::Return | Keysym::KP_Enter => {
            if repeat {
                KeyAction::Ignore
            } else {
                KeyAction::Submit
            }
        }
        Keysym::BackSpace => KeyAction::Backspace,
        // A wrong attempt is counted by PAM, so Backspace-per-character to abandon a mistyped
        // password would be costly. Every other password prompt clears on Escape; so does this one.
        Keysym::Escape => KeyAction::Clear,
        _ => match event.utf8.as_deref() {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => KeyAction::Append(text),
            _ => KeyAction::Ignore,
        },
    }
}
/// The name `on_click`'s second argument carries for one evdev button code, or `None` for a button
/// this engine does not hand to Lua at all. A string, not the raw `273` or a normalized
/// `1`/`2`/`3`: every categorical value crossing this boundary already is one (`fit`, `layer`,
/// `anchor`, `align_h`). ADR-0050's second amendment argues the rest and is the place to change if
/// revisited. `None` means the press never arms and the release never fires, matching an unhandled
/// button: the set that fires has to equal the set a config can name, since handed `"other"` for
/// `BTN_TASK`, a config cannot tell it from `BTN_EXTRA` and would run whatever was written for the
/// left button. ponytail: back and forward do nothing on a mouse that has them, only
/// left/right/middle of the eight `BTN_*` codes `smithay_client_toolkit::seat::pointer` names being
/// handled. Upgrade: map `BTN_SIDE`/`BTN_EXTRA` (0x113/0x114) and `BTN_BACK`/`BTN_FORWARD`
/// (0x116/0x115) when asked.
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some("left"),
        BTN_RIGHT => Some("right"),
        BTN_MIDDLE => Some("middle"),
        _ => None,
    }
}
/// Whether a release of `button` ends the press `armed` is holding, whether or not it completes it.
/// The narrower half of the pair with [`release_completes_click`]: completing needs the same
/// surface, rect and button, ending needs only the same button, since dragging off and releasing
/// ends the press exactly as clicking does. A release of a different button must not end it, or
/// pressing left, then right, then releasing left would clear the slot and lose the right click.
fn release_ends_press(armed: Option<&ArmedClick>, button: u32) -> bool {
    armed.is_some_and(|armed| armed.button == button)
}
/// Whether a release on `instance_id`, over the button at `released_on`, completes `armed`
/// (ADR-0050 decision 2). Both halves must be a button hit, not merely the same coordinates: a
/// release landing in the armed rect but on something no longer a handled button (a re-resolve put
/// a plain `rect` there) is not the click the press started. `released_on` is
/// [`clickable_button`]'s answer, not the raw pointer position.
fn release_completes_click(
    armed: Option<&ArmedClick>,
    instance_id: &str,
    released_on: Option<LogicalRect>,
    button: u32,
) -> bool {
    match (armed, released_on) {
        (Some(armed), Some(rect)) => armed.instance_id == instance_id && armed.rect == rect && armed.button == button,
        _ => false,
    }
}
/// `on_click`'s single argument: the button's rect as `{ x, y, width, height }` in its surface's
/// logical coordinates (ADR-0050 decision 3). The rect travels to the callback, not back from it: §
/// 6's `popup` entry has the anchor rect "passed straight from the rect `button`'s `on_click` hands
/// back", the round trip through `on_click = function(rect) menu_anchor:set(rect) end` and
/// `popup`'s `anchor_rect = menu_anchor`. `Err` names the step as well as the error, so a rect
/// table this engine could not build (its own bug) is not confused with a handler that raised (the
/// config's).
fn call_on_click(
    lua: &Lua,
    on_click: &Function,
    rect: LogicalRect,
    button: &str,
) -> Result<(), (&'static str, mlua::Error)> {
    let argument = rect_table(lua, rect).map_err(|e| ("could not build on_click's rect argument", e))?;
    on_click.call::<()>((argument, button)).map_err(|e| ("on_click raised, ignoring it", e))
}
/// `on_click`'s first argument: the button's rect as `{ x, y, width, height }` in its surface's
/// logical coordinates (ADR-0050 decision 3).
pub(super) fn rect_table(lua: &Lua, rect: LogicalRect) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", rect.x)?;
    table.set("y", rect.y)?;
    table.set("width", rect.width)?;
    table.set("height", rect.height)?;
    Ok(table)
}
/// Builds one `RendererFrame::SecureSubmit` out of `buffer` (ADR-0005/ADR-0027). The one sanctioned
/// read (`expose_secret`) and the explicit `.zeroize()` sit on adjacent lines, so the accumulated
/// secret stops existing the instant it is copied into the outgoing envelope, not left live while
/// the frame travels to the socket thread. The frame's own plaintext copy is the socket thread's to
/// scrub, right after its wire write (`crate::socket`'s `pump`). A free function, not a `&mut self`
/// method, for [`retarget_secure_submit`]'s reason: it makes the read/zeroize contract
/// unit-testable, which nothing involving a live `wl_surface` is.
fn secure_submit_frame(
    generation_id: u32,
    capability: &str,
    action: &str,
    buffer: &mut shared::SecureBuffer,
) -> RendererFrame {
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
    /// enforces the buffer's lifetime; assigning the field directly anywhere else reopens the leak
    /// that function closes.
    fn focus_secure_submit(&mut self, next: Option<FocusedField>) {
        retarget_secure_submit(&mut self.focused_secure_submit, &mut self.secure_buffer, next);
        // A focus change zeroizes the buffer, so the field that had dots must be repainted without
        // them, the same reason a keystroke sets this.
        self.field_input_changed = true;
    }

    /// Whether `instance_id` is still a surface this process has a live `wl_surface` for.
    /// `TrackedRole::wl_surface` is the right test: it answers `None` for both shapes a gone
    /// surface takes, the entry removed outright ([`App::destroy_surface_by_id`]) or kept with its
    /// role object dropped ([`App::hide_window`], [`App::teardown_lock_surfaces`],
    /// [`App::drop_popup_object`]).
    fn surface_is_live(&self, instance_id: &str) -> bool {
        self.surfaces.iter().any(|tracked| tracked.surface_id == instance_id && tracked.role.wl_surface().is_some())
    }

    /// The surfaces a keystroke arriving now can reach: whichever surface holds keyboard focus,
    /// followed by every popup currently shown under it. Empty when nothing here holds the
    /// keyboard, or when the compositor's focus names a surface this process no longer tracks.
    ///
    /// The popups are the point. `wl_keyboard` focus is one surface, but an `xdg_popup` is only
    /// handed it by niri when its parent already held the keyboard at the moment the popup mapped
    /// -- and `modules/bar/init.lua` raises the bar's `keyboard_interactivity` in response to
    /// `network.password_ssid`, which is set by a click *inside* the already-open panel popup. So
    /// the keys land on the bar while the field that wants them is on `panel_host`. Asking the
    /// parent alone made the prompt untypable until the panel was closed and reopened, which is
    /// what a second map fixed by accident.
    ///
    /// Reuses `xdg_shell`'s [`App::shown_popups_under`], the same walk `hide_popup` destroys by, so
    /// "shown under this surface" has one definition. Ids rather than trees: the per-keystroke
    /// caller ([`App::prune_secure_focus`]) needs only the ids, and `Scene::surface` rebuilds an
    /// owned tree per call.
    fn keyboard_focus_scope(&self) -> Vec<String> {
        let Some(focused) = self.keyboard_focus.as_deref() else {
            return Vec::new();
        };
        let Some(index) = self.surfaces.iter().position(|tracked| tracked.surface_id == focused) else {
            return Vec::new();
        };
        let mut popups = Vec::new();
        self.shown_popups_under(index, &mut popups);
        let mut scope = vec![focused.to_string()];
        scope.extend(popups.into_iter().map(|popup| self.surfaces[popup].surface_id.clone()));
        scope
    }

    /// [`focus_on_enter`] over a scope's resolved trees. Split out because two callers ask the same
    /// question at different moments: `KeyboardHandler::enter`, when focus arrives, and
    /// [`App::arm_secure_focus_if_the_scope_now_declares_one`], when the trees change under a focus
    /// that already arrived.
    fn field_the_scope_declares(&self, scope: &[String], current: Option<FocusedField>) -> Option<FocusedField> {
        // `Scene::surface` hands back an owned tree, so the borrow of `self.client` ends with
        // `trees` and the caller's write is free to take `&mut self`.
        let trees: Vec<(&str, layout::ResolvedNode)> =
            scope.iter().filter_map(|id| self.client.scene().surface(id).map(|tree| (id.as_str(), tree))).collect();
        let borrowed: Vec<(&str, &layout::ResolvedNode)> = trees.iter().map(|(id, tree)| (*id, tree)).collect();
        focus_on_enter(&borrowed, current.as_ref())
    }

    /// Arms the scope's sole `secure_submit` field when the *tree* is what changed, rather than the
    /// focus.
    ///
    /// `KeyboardHandler::enter` is not enough on its own, and the network password prompt is the
    /// case that proves it. The bar takes the keyboard when a panel opens, which is one `enter`;
    /// the prompt appears later, when a click inside that panel sets `network.password_ssid`. No
    /// second `enter` follows, because focus never moved -- so the field that just became visible
    /// would never be armed, and the prompt would sit there refusing every keystroke.
    ///
    /// Why the panel cannot simply take the keyboard when the prompt appears instead: changing a
    /// mapped layer surface's `keyboard_interactivity` makes the compositor re-evaluate focus,
    /// which breaks the popup's grab, and niri then dismisses the popup the prompt is drawn in.
    /// Measured: the field armed and the panel vanished in the same frame.
    ///
    /// Only when nothing is armed, so this can never take a field away from the press that chose
    /// it on a surface declaring several -- the guess [`sole_secure_submit_in_scope`] refuses to
    /// make (ADR-0050 decision 4). Re-arming what is already armed is left to
    /// `KeyboardHandler::enter`, which keeps a still-declared field by construction.
    pub(super) fn arm_secure_focus_if_the_scope_now_declares_one(&mut self) {
        if self.focused_secure_submit.is_some() || self.keyboard_focus.is_none() {
            return;
        }
        let scope = self.keyboard_focus_scope();
        let Some(field) = self.field_the_scope_declares(&scope, None) else {
            return;
        };
        eprintln!(
            "[oblisk-renderer] {}'s `secure_submit` field ({}/{}) became typable under the keyboard focus already held",
            field.surface_id, field.target.capability, field.target.action
        );
        self.focus_secure_submit(Some(field));
    }

    /// Drops the focused field, and the half-typed secret with it, the moment
    /// [`focus_is_still_armed`] stops holding, through [`App::focus_secure_submit`] so the scrub is
    /// the same one every other transition gets. Called before every keystroke, so the rule is
    /// load-bearing rather than advisory: nothing can reach `secure_buffer` through a stale focus,
    /// whatever took the surface away and whether or not a `leave` followed.
    fn prune_secure_focus(&mut self) {
        let scope = self.keyboard_focus_scope();
        let armed = self
            .focused_secure_submit
            .as_ref()
            .is_some_and(|field| focus_is_still_armed(field, &scope, self.surface_is_live(&field.surface_id)));
        if self.focused_secure_submit.is_some() && !armed {
            eprintln!(
                "[oblisk-renderer] the focused secure_submit field is no longer the one receiving keys; dropping it and scrubbing its buffer"
            );
            self.focus_secure_submit(None);
        }
    }

    /// Whichever `textfield` on `surface_id` has the keyboard, in the shape paint reads. Here
    /// rather than in `wayland::surface`: this is the one place that knows a masked focus is a
    /// surface plus a `{ capability, action }` pair and a plain one a surface plus a box, and
    /// `paint` should not learn either shape to ask one question. The masked arm hands back the
    /// count and never the bytes; see `layout::paint::FieldFocus`.
    pub(super) fn field_focus_for(&self, surface_id: &str) -> Option<layout::paint::FieldFocus<'_>> {
        if let Some(focused) = self.focused_secure_submit.as_ref().filter(|f| f.surface_id == surface_id) {
            return Some(layout::paint::FieldFocus::Masked {
                target: &focused.target,
                filled: self.secure_buffer.char_count(),
            });
        }
        let focused = self.focused_text_field.as_ref().filter(|f| f.surface_id == surface_id)?;
        Some(layout::paint::FieldFocus::Plain { id: focused.id, text: &focused.buffer })
    }

    /// The half of [`App::prune_secure_focus`] that does not wait for a keystroke: a field whose
    /// surface this process destroyed is dropped, and its buffer scrubbed, on the next poll turn.
    /// Only the liveness clause, deliberately: the routing clause is only ever wrong at the moment
    /// a key arrives, which is where it is asked. Applying it once a turn would also disarm a field
    /// a press on a multi-field surface just chose, before the compositor's matching `enter` lands,
    /// which `sole_secure_submit` cannot re-choose (see [`focus_on_enter`]). This caps the liveness
    /// clause's residency: a `wl_surface` this process destroys, such as `teardown_lock_surfaces`
    /// tearing down the lock screen, may never produce a `leave`, and without this the plaintext
    /// would sit in `secure_buffer` until a keystroke that never comes.
    pub(super) fn drop_secure_focus_if_its_surface_is_gone(&mut self) {
        let gone = self.focused_secure_submit.as_ref().is_some_and(|field| !self.surface_is_live(&field.surface_id));
        if gone {
            eprintln!(
                "[oblisk-renderer] the surface holding the focused secure_submit field is gone; dropping it and scrubbing its buffer"
            );
            self.focus_secure_submit(None);
        }
    }

    /// One key event applied to the focused `secure_submit` field, or nothing when the focused
    /// field is not one (ADR-0005). The focus check is the gate: `focused_secure_submit` is `Some`
    /// only when a field named a destination, so a keystroke that reaches the buffer already has
    /// somewhere to be sent. A masked `textfield` that names none is never focused at all (see
    /// [`focused_field`]): buffering a password for a field that can never submit it is a secret
    /// held for no reason. Nothing here touches Lua: the bytes go from the `KeyEvent` into a native
    /// `shared::SecureBuffer` and out to the Supervisor -- which is the whole difference from
    /// [`App::apply_plain_key`], where the text is the point.
    ///
    /// Pruning is [`App::apply_key`]'s, done once for both field kinds before either gate.
    fn apply_secure_key(&mut self, event: &KeyEvent, repeat: bool) {
        if self.focused_secure_submit.is_none() {
            return;
        }
        // Every arm below moves what the field draws: two change the character count and the third
        // clears it. Set once here rather than in each.
        self.field_input_changed = true;
        match key_action(event, repeat) {
            KeyAction::Append(text) => self.secure_buffer.push_str(text),
            // `pop_char` zeroizes the dropped bytes rather than only shortening the buffer, keeping
            // a corrected character from staying readable in this process's heap.
            KeyAction::Backspace => {
                self.secure_buffer.pop_char();
            }
            // Through the seam, not the buffer directly: the scrub Escape wants is the one
            // `retarget_secure_submit` performs on a transition, and re-arming the identical field
            // right after leaves the user still in it, free to retype.
            KeyAction::Clear => {
                let field = self.focused_secure_submit.clone();
                self.focus_secure_submit(None);
                self.focus_secure_submit(field);
            }
            KeyAction::Submit => self.finish_secure_submit(),
            KeyAction::Ignore => {}
        }
    }

    /// One key event, to whichever field kind has the keyboard (ADR-0092).
    ///
    /// Pruning happens once, here, before either gate: a focus whose surface is gone or is no
    /// longer receiving keys is exactly the state this key must not reach, and that is true of a
    /// half-typed reply for the same reason it is true of a half-typed password (see
    /// [`focus_is_still_armed`]). The two focuses are mutually exclusive, so the order of the
    /// arms below decides nothing.
    fn apply_key(&mut self, event: &KeyEvent, repeat: bool) {
        self.prune_secure_focus();
        self.prune_text_field_focus();
        self.apply_secure_key(event, repeat);
        self.apply_plain_key(event, repeat);
    }

    /// Every write to `focused_text_field`, funnelled the way [`App::focus_secure_submit`] is --
    /// for repainting rather than for scrubbing. There is no secret here to zeroize; what the two
    /// share is that the field they leave must stop drawing a caret and the field they arrive at
    /// must start.
    fn focus_text_field(&mut self, next: Option<FocusedTextField>) {
        if self.focused_text_field.is_none() && next.is_none() {
            return;
        }
        self.focused_text_field = next;
        self.field_input_changed = true;
    }

    /// [`App::prune_secure_focus`]'s counterpart. The same two clauses -- the surface is still
    /// alive, and it is still one the keyboard can reach -- because a plain field goes stale for
    /// exactly the reasons a masked one does. What it does not share is the urgency: dropping a
    /// half-typed reply loses a sentence, not a secret, so there is no once-a-turn sweep matching
    /// [`App::drop_secure_focus_if_its_surface_is_gone`]; the check before each keystroke is
    /// enough, and a `leave` clears it anyway.
    fn prune_text_field_focus(&mut self) {
        let Some(field) = self.focused_text_field.as_ref() else {
            return;
        };
        let scope = self.keyboard_focus_scope();
        if self.surface_is_live(&field.surface_id) && scope.contains(&field.surface_id) {
            return;
        }
        eprintln!(
            "[oblisk-renderer] the focused textfield is no longer the one receiving keys; dropping what was typed"
        );
        self.focus_text_field(None);
    }

    /// One key event applied to the focused plain `textfield` (ADR-0092), the mirror of
    /// [`App::apply_secure_key`] for the half of § 5.2 item 8 whose text a config is meant to read.
    ///
    /// Every edit calls `on_change` and a submit calls `on_submit`, both with the whole text rather
    /// than the delta: a config binding a `state` signal to it wants the value, and reassembling a
    /// string from deltas is work every caller would repeat. `on_submit` leaves the field focused
    /// and empty, so a reply box takes the next message without another click.
    ///
    /// Escape clears, and on a field with no `on_cancel` it stays, the same answer the masked half
    /// gives: a config cannot observe focus, so a field that silently stopped taking keys would
    /// have no way to say so on the glass. A field that declared `on_cancel` *can* be told, so
    /// there Escape also drops the focus and then says so (ADR-0102) -- the callback is the last
    /// thing to run, after the field has already let go, since it will usually take the field or
    /// its surface's keyboard away and must not find the focus still pointing at it.
    fn apply_plain_key(&mut self, event: &KeyEvent, repeat: bool) {
        let Some(field) = self.focused_text_field.as_mut() else {
            return;
        };
        let edit = edit_plain_buffer(&mut field.buffer, key_action(event, repeat), field.on_cancel.is_some());
        if edit == (PlainEdit { changed: false, submitted: false, cancelled: false }) {
            return;
        }
        // Cloned out before any callback runs: a handler is free to write a signal that
        // re-resolves the scene, and holding a `&mut` into `self` across that is not on offer.
        let (text, on_change, on_submit, on_cancel, surface_id) = {
            let field = self.focused_text_field.as_ref().expect("the focus was Some a moment ago");
            (
                field.buffer.clone(),
                field.on_change.clone(),
                field.on_submit.clone(),
                field.on_cancel.clone(),
                field.surface_id.clone(),
            )
        };
        if edit.submitted {
            // Emptied before the call, not after: `on_submit` may open a popup or write a signal,
            // and the field it comes back to must be the empty one, not the text it just consumed.
            if let Some(field) = self.focused_text_field.as_mut() {
                field.buffer.clear();
            }
        }
        if edit.cancelled {
            self.focus_text_field(None);
        }
        self.field_input_changed = true;
        if edit.changed
            && let Some(on_change) = on_change
            && let Err(e) = on_change.call::<()>(if edit.submitted { String::new() } else { text.clone() })
        {
            eprintln!("[oblisk-renderer] {surface_id}: on_change raised, ignoring it: {e}");
        }
        if edit.submitted
            && let Some(on_submit) = on_submit
            && let Err(e) = on_submit.call::<()>(text)
        {
            eprintln!("[oblisk-renderer] {surface_id}: on_submit raised, ignoring it: {e}");
        }
        if edit.cancelled
            && let Some(on_cancel) = on_cancel
            && let Err(e) = on_cancel.call::<()>(())
        {
            eprintln!("[oblisk-renderer] {surface_id}: on_cancel raised, ignoring it: {e}");
        }
    }

    /// A completed `secure_submit`: builds the outgoing frame from the accumulated buffer and
    /// queues it for the socket thread. [`submit_frame_for`] performs the one sanctioned read and
    /// leaves `self.secure_buffer` scrubbed and empty on either branch. [`submit_frame_for`]
    /// refuses on two counts, both logged here: no focused destination (ADR-0050 decision 4),
    /// almost always a `textfield` missing its `secure_submit` table, and an empty buffer, which
    /// explains itself on the glass.
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
    /// (ADR-0050 decisions 1 and 4). `position` is surface-local and logical, the space
    /// `layout::hit` walks `ResolvedNode::rect` in, so there is no conversion here. That holds only
    /// while `paint_surface` paints at scale `1.0` and nothing calls
    /// `wl_surface::set_buffer_scale`; ADR-0050's consequences name this a caller a future HiDPI
    /// change must move together with `paint_surface` and `apply_input_region`. A surface with no
    /// resolved tree answers like a point that missed everything: no button, no focused
    /// destination. Owned on the way out, every part: `Scene::surface` clones into a `ResolvedNode`
    /// (the same property `paint_surface` relies on), ending the borrow of `self.client` there, and
    /// both the `Function` and the target are cloned out before the local tree is dropped.
    fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit { button: None, field: None };
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(&tree, point);
        PointerHit {
            button: clickable_button(&path).map(|(rect, on_click)| (rect, on_click.clone())),
            field: focused_field(&path),
        }
    }

    /// One notch or one swipe, applied to the innermost scrollable container under the pointer.
    /// **Pixels when sent, a step otherwise** (ADR-0069 decision 6): a touchpad reports `absolute`
    /// in logical pixels, a notched wheel only `value120` (120 per logical step); `discrete` is
    /// ignored as deprecated, since every compositor still sending it sends `value120` too.
    /// **Innermost wins and nothing chains**: a wheel over a list inside a scrollable panel moves
    /// the list and moves nothing once it hits its end, unlike a browser's chaining to the parent,
    /// a rule with edge cases no config here has asked for. The offset written is unclamped:
    /// `layout::scene`'s positioning pass owns the bound, the only place that knows the content
    /// extent, and writes back what it used.
    fn scroll_at(
        &mut self,
        index: usize,
        position: (f64, f64),
        horizontal_px: f64,
        horizontal_steps: i32,
        vertical_px: f64,
        vertical_steps: i32,
    ) {
        if !crate::lua::signal::any_scroll_registered(self.client.lua()) {
            return;
        }
        let surface_id = &self.surfaces[index].surface_id;
        let Some(tree) = self.client.scene().surface(surface_id) else {
            return;
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(&tree, point);
        // Innermost first, so the deepest scrollable container under the pointer takes it.
        let Some((signal, axis)) = path.iter().rev().find_map(|node| {
            let signal = layout::scene::scroll_signal_of(node)?;
            let axis = layout::scene::scrolling_axis(&node.kind, &node.properties).ok()??;
            Some((signal, axis))
        }) else {
            return;
        };
        let (pixels, steps) = match axis {
            layout::scene::MainAxis::Horizontal => (horizontal_px, horizontal_steps),
            layout::scene::MainAxis::Vertical => (vertical_px, vertical_steps),
        };
        let delta = wheel_delta(pixels, steps);
        if delta == 0.0 {
            return;
        }
        let Some(handle) = signal.scroll_handle() else {
            return;
        };
        let current = signal.scroll_offset().unwrap_or(0.0);
        handle.set_changed(mlua::Value::Number(f64::from(current + delta)));
    }

    /// Writes every `hover` signal in one surface against the pointer's position, or turns them all
    /// off when `position` is `None` (ADR-0062). Two phases, forced by borrowing:
    /// `layout::hover::hover_writes` borrows the tree from `self.client`, so the writes must be
    /// collected before `LiveSignalHandle::set_changed` can run; cloning the `Signal`s (an `Rc`
    /// handle, a refcount bump) ends the borrow. `set_changed` marks the scene dirty only when the
    /// value moved (decision 4), so a pointer sitting still inside one button re-resolves nothing,
    /// which matters since `wl_pointer` reports motion at device rate and one mark re-resolves
    /// every surface in the generation (ADR-0044 decision 2). ponytail: `Scene::surface`
    /// deep-clones the retained subtree into a `ResolvedNode` on every motion event, a few hundred
    /// small `HashMap` clones per event on the dispatch thread. Upgrade: a borrowing accessor
    /// (`Scene` handing out `&RetainedNode`), once a profile demands it.
    fn sync_hover(&mut self, index: usize, position: Option<(f64, f64)>) {
        // Before the tree is touched, because the tree is the expensive part: a config that never
        // called `hover(name)` has nothing to write and skips all of it.
        if !crate::lua::signal::any_hover_registered(self.client.lua()) {
            return;
        }
        let surface_id = &self.surfaces[index].surface_id;
        let Some(tree) = self.client.scene().surface(surface_id) else {
            return;
        };
        let point = position.map(|(x, y)| layout::hit::LogicalPoint { x: x as f32, y: y as f32 });
        let writes = layout::hover::hover_writes(&tree, point);
        let lua = self.client.lua();
        for write in writes {
            // `None` for anything that is not a hover signal, which is how a config binding `hover
            // = oblisk.network` fails to make the pointer overwrite a capability snapshot (ADR-0062
            // decision 2).
            let Some(handle) = write.signal.hover_handle() else {
                continue;
            };
            // The boolean gates the rect and the callback below, so it is checked first.
            let crossed = handle.set_changed(mlua::Value::Boolean(write.hovered));
            // Only on the edge, which is the whole reason `on_hover` rides on the signal's write
            // (ADR-0095): `wl_pointer` reports motion at device rate, so firing per event would
            // call a config handler a few hundred times for one pass across a button. Raised
            // errors are logged and swallowed on `fire_on_click`'s terms -- a broken handler is a
            // config bug and must not take down a shell that is otherwise painting fine.
            if crossed
                && let Some(on_hover) = &write.on_hover
                && let Err(err) = on_hover.call::<()>(write.hovered)
            {
                eprintln!("[oblisk-renderer] {}: on_hover handler raised: {err}", self.surfaces[index].surface_id);
            }
            // Only on the edge into the node, and not just an optimisation: `set_changed` compares
            // with `PartialEq`, and two `mlua` tables holding identical numbers are not equal since
            // table equality is identity, so a freshly built rect table always counts as a change.
            // Writing it per motion event would undo decision 4; once per entry is all that is
            // wanted, since the node does not move while the pointer sits inside it.
            if crossed
                && let Some(rect) = write.rect
                && let Some(rect_handle) = write.signal.hover_rect_handle()
            {
                match rect_table(lua, rect) {
                    Ok(table) => rect_handle.set(mlua::Value::Table(table)),
                    // The engine's own failure, not the config's, and not worth taking a shell down
                    // for: the boolean already landed, so a tooltip opens where it last was rather
                    // than not opening.
                    Err(err) => eprintln!(
                        "[oblisk-renderer] {}: could not build a hover rect: {err}",
                        self.surfaces[index].surface_id
                    ),
                }
            }
        }
    }

    /// Calls one `button`'s `on_click` with its rect (ADR-0050 decision 3). A raise is logged
    /// against the surface it happened on and swallowed: a broken `on_click` is a config bug and
    /// must not take down a shell that is otherwise painting fine. ADR-0046's rescue path is for a
    /// failed evaluation, not a misbehaving handler, so this deliberately does not set `self.exit`
    /// or enter rescue.
    fn fire_on_click(&mut self, instance_id: &str, rect: LogicalRect, button: &str, on_click: &Function) {
        // Nothing marks the scene dirty here: a handler changes what is painted by writing a
        // `state(name, initial)` signal, and `signal:set()` marks the flag itself (ADR-0044
        // decision 5), so a handler that writes nothing causes no re-resolve.
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

    /// Pointer and keyboard, and no touch object at all: § 5.2 has no touch-specific property for
    /// one to serve. Idempotent by the `is_none` guards, not by trusting the compositor:
    /// `wl_seat::capabilities` restates the full set on every change, so a seat gaining a keyboard
    /// re-announces its pointer and SCTK turns each announcement into this call. A second
    /// `wl_pointer`/`wl_keyboard` would deliver duplicate events into one `armed` slot, or two
    /// `enter`/`leave` streams into one `keyboard_focus`.
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                // Not fatal: a shell with no pointer still paints, still reloads, and still takes
                // `wp-text-input-v3` input. Only `on_click` stops working, which is what this says.
                Err(e) => {
                    eprintln!("[oblisk-renderer] wl_seat::get_pointer failed; no button's on_click will ever fire: {e}")
                }
            },
            // `None` rmlvo: take the compositor's own keymap. This shell never interprets a keysym
            // (there is no `on_key` in § 5.2), so imposing a layout of its own would be policy
            // serving nothing.
            Capability::Keyboard if self.keyboard.is_none() => match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                // Also not fatal, and narrower than it looks: losing this loses the `enter`/`leave`
                // that clears a focused `textfield`, so a stale focus can outlive the user.
                Err(e) => eprintln!(
                    "[oblisk-renderer] wl_seat::get_keyboard failed; keyboard focus will never be tracked: {e}"
                ),
            },
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer => {
                // A pointer that is gone will never send the `release` this press was waiting for,
                // which is the same reason `leave` clears it (ADR-0050 decision 2).
                self.armed = None;
                if let Some(pointer) = self.pointer.take() {
                    // `wl_pointer::release` is `since="3"`; below that dropping the proxy is the
                    // whole cleanup. Same guard SCTK's own `ThemedPointer::drop` applies
                    // (src/seat/pointer/mod.rs:572).
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
            }
            Capability::Keyboard => {
                // No keyboard means nothing will ever report the user leaving, so the focus this
                // was holding is stale from here on (ADR-0050 decision 4), and whatever was
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

/// Pointer input to `on_click` (ADR-0050). See `delegate_dispatch2!(App)` at the bottom of this
/// file for why no `delegate_pointer!` call accompanies this.
impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // A surface this process does not own: a `wl_pointer` is per seat, not per surface, and
            // nothing stops the compositor from having delivered an event for a surface that has
            // since been destroyed by a `visible` flip or an output change.
            let Some(index) = self.index_of_surface(&event.surface) else {
                continue;
            };
            match event.kind {
                // Left, right and middle (ADR-0050's second amendment). Any other code is not a
                // button a config can name, so it arms nothing and fires nothing.
                PointerEventKind::Press { button, serial, .. } => {
                    if pointer_button_name(button).is_none() {
                        continue;
                    }
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // ADR-0049's amendment: armed here, read by this turn's re-resolve if one
                    // creates a popup, cleared by `run`'s poll loop at the end of the turn either
                    // way. See [`ArmedSerial`] for why both edges of a click arm it.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    // ADR-0051's first amendment: the same "the user asked again" fact, kept past
                    // the end-of-turn disarm.
                    self.pointer_input_count += 1;
                    let hit = self.hit_under(index, event.position);
                    // The press decides focus, not the release (decision 4): a press whose path
                    // holds a `textfield` focuses it, a press landing anywhere else clears it.
                    // Bound to the surface the press landed on, per [`FocusedField`].
                    //
                    // Both halves are written on every press, including the clears, because the
                    // two are one focus: pressing into a reply box has to take the keyboard away
                    // from a password prompt, and vice versa.
                    let (masked, plain) = match hit.field {
                        Some(FieldTarget::Masked(target)) => {
                            (Some(FocusedField { surface_id: instance_id.clone(), target }), None)
                        }
                        Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel }) => (
                            None,
                            Some(FocusedTextField {
                                surface_id: instance_id.clone(),
                                id,
                                buffer: String::new(),
                                on_change,
                                on_submit,
                                on_cancel,
                            }),
                        ),
                        None => (None, None),
                    };
                    // Through the seam: this site *reassigns* rather than clears, the A-to-B
                    // transition [`retarget_secure_submit`] exists for.
                    let focused_a_field = masked.is_some() || plain.is_some();
                    self.focus_secure_submit(masked);
                    self.focus_text_field(plain);
                    // A press that focused a `textfield` arms no click, so the ancestor `button`
                    // does not also fire on release (ADR-0092). `textfield` is a leaf -- § 5.2
                    // gives it no `children` -- so any `button` on the path is above it, and
                    // clicking into a text field inside a clickable row is not a click on the row.
                    // The notification card is the case: its whole surface activates the sender's
                    // default action, and its reply box sits inside that.
                    self.armed = hit.button.filter(|_| !focused_a_field).map(|(rect, _)| ArmedClick {
                        instance_id,
                        rect,
                        button,
                    });
                }
                PointerEventKind::Release { button, serial, .. } => {
                    let Some(name) = pointer_button_name(button) else {
                        continue;
                    };
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // Overwrites the press's, and that is the point: a click fires on the release
                    // (ADR-0050 decision 2), so a popup opened by `on_click` is opened by *this*
                    // event and carries this serial.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    self.pointer_input_count += 1;
                    // Focus is untouched here. The press already decided it, and a release that
                    // drags off a `textfield` must not un-focus the field the user is typing into.
                    let hit = self.hit_under(index, event.position).button;
                    let fires = release_completes_click(
                        self.armed.as_ref(),
                        &instance_id,
                        hit.as_ref().map(|(rect, _)| *rect),
                        button,
                    );
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
                // else. This is the drag-off-and-cancel decision 2 is built around. The `None`
                // position turns every hover in this surface off (ADR-0062): no `Motion` will
                // arrive to say the pointer has gone, so a tooltip left open here stays open.
                PointerEventKind::Leave { .. } => {
                    self.armed = None;
                    self.sync_hover(index, None);
                }
                // A motion that leaves the armed rect deliberately does *not* disarm. Dragging back
                // onto the button and releasing still clicks it, which is what every toolkit does.
                // Both kinds carry a position and both update hover, because an `Enter` is the only
                // event a pointer that appears already inside a surface sends.
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.sync_hover(index, Some(event.position));
                }
                // The wheel (ADR-0069). `Enter`/`Motion`/`Leave` above have already kept
                // `sync_hover` fed, so the position this needs is the event's own.
                PointerEventKind::Axis { horizontal, vertical, .. } => {
                    self.scroll_at(
                        index,
                        event.position,
                        horizontal.absolute,
                        horizontal.value120,
                        vertical.absolute,
                        vertical.value120,
                    );
                }
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
        // here, so they are dropped along with every other key event below. A `wl_keyboard` is per
        // seat, not per surface, so an `enter` can name a surface this process destroyed since the
        // compositor sent it (a `visible` flip, an output change); that is the `None` below and it
        // is not an error.
        self.keyboard_focus = self.surface_id_for(surface).map(str::to_string);
        // Not just the entering surface: the keys it is about to receive also reach the popups
        // shown under it, which is where a panel's password prompt lives (see
        // [`App::keyboard_focus_scope`]).
        let scope = self.keyboard_focus_scope();
        // The rule that makes a lock screen typable with no click: keyboard focus on a scope
        // declaring exactly one `secure_submit` field focuses it (see [`sole_secure_submit_in_scope`]).
        let next = self.field_the_scope_declares(&scope, self.focused_secure_submit.clone());
        match (&self.keyboard_focus, &next) {
            (None, _) => eprintln!("[oblisk-renderer] keyboard focus entered an untracked surface; not tracking it"),
            (Some(id), Some(field)) => eprintln!(
                "[oblisk-renderer] keyboard focus entered {id} and takes {}'s `secure_submit` field ({}/{})",
                field.surface_id, field.target.capability, field.target.action
            ),
            // The scope is named, not just the surface: "declares no field" has two very different
            // causes -- the popup holding the field is not in reach, or it is in reach and its
            // field is not visible -- and they are indistinguishable without knowing what was
            // searched.
            (Some(id), None) => eprintln!(
                "[oblisk-renderer] keyboard focus entered {id}, and neither it nor its shown popups {:?} declare a sole `secure_submit` field",
                &scope[1..]
            ),
        }
        // Unconditional: a case that arms nothing must still disarm, or `apply_secure_key` would
        // keep appending keystrokes to the previous surface's field and submitting to its
        // capability.
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
        // ADR-0050 decision 4's third clearing source: the user is demonstrably elsewhere, so the
        // `textfield` stops owning the next secret and the armed press never sees its release, the
        // same answer `PointerEventKind::Leave` gives. Load-bearing: no submit is coming for these.
        self.focus_secure_submit(None);
        self.focus_text_field(None);
        self.armed = None;
        eprintln!("[oblisk-renderer] keyboard focus left {left}");
    }

    // A key reaches exactly one place and it is not a config: § 5.2 declares no key-handler
    // property, and ADR-0050's consequences say this ADR does not invent one. What it has is the
    // `secure_submit` field ADR-0005 defines: [`App::apply_secure_key`] pushes bytes into a native
    // `shared::SecureBuffer` and out to the Supervisor with no Lua value ever existing, adding no
    // IDL surface. See [`key_action`] for why this is the keyboard, not text-input.
    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.apply_key(&event, false);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.apply_key(&event, true);
    }

    // Genuinely empty, and the two below with it: a release carries no `utf8` at all (SCTK's own
    // `KeyEvent` doc says so), and neither a modifier latch nor a layout change edits a buffer.
    // They exist because `KeyboardHandler` has no default bodies for them.
    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

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
    use crate::layout::secure_submit::sole_secure_submit;
    use crate::layout::secure_submit::tree_can_authenticate;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Hands each hand-built `ResolvedNode` its own id. A `Scene` allocates these in production and
    /// these tests have no `Scene`; the only property that matters is that two nodes are never
    /// accidentally the same node.
    static NEXT_TEST_NODE_ID: AtomicU64 = AtomicU64::new(1);

    /// ADR-0069 decision 6. The rest of `scroll_at` needs a compositor to deliver a notch;
    /// this is the half that does not.
    #[test]
    fn a_touchpads_pixels_are_used_as_sent_and_a_wheels_steps_are_converted() {
        assert_eq!(wheel_delta(17.5, 0), 17.5, "a touchpad reports a distance and it is taken");
        assert_eq!(wheel_delta(-17.5, 0), -17.5, "including upward");
        assert_eq!(wheel_delta(0.0, 120), WHEEL_STEP_PIXELS, "one notch is one step");
        assert_eq!(wheel_delta(0.0, -240), -2.0 * WHEEL_STEP_PIXELS, "two notches up");
        assert_eq!(wheel_delta(0.0, 60), WHEEL_STEP_PIXELS / 2.0, "high-resolution wheels send fractions of a notch");
    }

    /// A compositor that sends both is sending the same motion twice, so the distance wins and the
    /// step count is not added on top of it.
    #[test]
    fn a_step_count_is_ignored_when_a_distance_came_with_it() {
        assert_eq!(wheel_delta(17.5, 120), 17.5);
    }

    #[test]
    fn a_wheel_event_carrying_no_motion_scrolls_nothing() {
        assert_eq!(wheel_delta(0.0, 0), 0.0);
    }

    /// The `value120` that would have been silently dropped by narrowing it to an `i16` first.
    ///
    /// Reachable rather than absurd: `AxisScroll::merge` sums `value120` across every axis event
    /// queued before the next `Frame` (`ret.value120 += other.value120`, smithay-client-toolkit
    /// 0.21.1), so one frame of dispatch lag behind a fast wheel accumulates past `i16::MAX`. The
    /// old narrowing turned that into a delta of zero, which returns early without writing the
    /// signal at all -- a hard flick scrolling nothing, at exactly the moment the client was
    /// already behind.
    #[test]
    fn an_implausibly_large_step_count_still_scrolls_rather_than_becoming_zero() {
        assert!(wheel_delta(0.0, 120 * 400) > 0.0);
    }

    #[test]
    fn secure_submit_frame_carries_the_accumulated_secret_and_zeroizes_the_buffer_it_read() {
        // ADR-0005, ADR-0027: the frame carries the exact secret
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

    fn hit_node(
        lua: &Lua,
        kind: &str,
        (x, y, width, height): (f32, f32, f32, f32),
        on_click: bool,
    ) -> layout::ResolvedNode {
        let mut properties = HashMap::new();
        if on_click {
            properties.insert("on_click".to_string(), Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        }
        layout::ResolvedNode {
            // Distinct per node, since `focused_field` now reads an identity off one of these and
            // a shared id would make every hand-built field the same field.
            id: layout::scene::NodeId::test(NEXT_TEST_NODE_ID.fetch_add(1, Ordering::Relaxed)),
            kind: kind.to_string(),
            paint: node::paint_style(kind, &properties).unwrap(),
            rect: LogicalRect { x, y, width, height },
            visible: true,
            opacity: 1.0,
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
        // Re-derived rather than hand-written, because `layout::secure_submit` reads the parsed
        // style now and `Scene::apply` is what fills it in production: a fixture that set it by
        // hand could declare a destination the parser would never have found.
        node.paint = node::paint_style(&node.kind, &node.properties).unwrap();
        node
    }

    fn secure_submit_table(lua: &Lua, capability: &str, action: &str) -> Value {
        let table = lua.create_table().unwrap();
        table.set("capability", capability).unwrap();
        table.set("action", action).unwrap();
        Value::Table(table)
    }

    /// The masked destination [`focused_field`] found, or `None` for anything else. Most of these
    /// tests only care about that half.
    fn masked_target(path: &[&layout::ResolvedNode]) -> Option<node::SecureSubmitTarget> {
        match focused_field(path)? {
            FieldTarget::Masked(target) => Some(target),
            FieldTarget::Plain { .. } => None,
        }
    }

    /// A `textfield` carrying `on_submit`, the plain half's minimum for being worth focusing.
    fn plain_textfield(lua: &Lua) -> layout::ResolvedNode {
        let mut node = textfield(lua, None);
        let on_submit = lua.create_function(|_, _text: String| Ok(())).unwrap();
        node.properties.insert("on_submit".to_string(), Value::Function(on_submit));
        node
    }

    #[test]
    fn a_press_landing_on_no_textfield_leaves_no_destination_focused() {
        let lua = Lua::new();
        let button = hit_node(&lua, "button", (0.0, 0.0, 40.0, 24.0), true);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(focused_field(&[&root, &button]).is_none());
    }

    #[test]
    fn the_innermost_textfield_on_the_path_is_the_one_that_owns_the_next_secret() {
        // Same deep-end scan `clickable_button` makes, and for the same reason (ADR-0050
        // decision 1): one traversal, two questions.
        let lua = Lua::new();
        let outer = textfield(&lua, Some(secure_submit_table(&lua, "outer", "ignored")));
        let inner = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);

        assert_eq!(
            masked_target(&[&root, &outer, &inner]),
            Some(node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() })
        );
    }

    /// Neither a destination nor a callback: nothing downstream could read a keystroke, so taking
    /// the keyboard for it would only strand the user in a field that swallows keys (ADR-0092).
    #[test]
    fn a_textfield_that_can_report_nothing_is_not_worth_focusing() {
        let lua = Lua::new();
        let field = textfield(&lua, None);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(focused_field(&[&root, &field]).is_none());
    }

    /// The unmasked half of § 5.2 item 8 (ADR-0092): no `secure_submit`, a callback, so the press
    /// focuses it as a plain field carrying the node identity paint will find it by (ADR-0099).
    #[test]
    fn a_textfield_with_a_callback_and_no_secure_submit_focuses_as_a_plain_field() {
        let lua = Lua::new();
        let field = plain_textfield(&lua);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        match focused_field(&[&root, &field]) {
            Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel }) => {
                assert_eq!(id, field.id, "the field's own node, not the root it was reached through");
                assert!(on_change.is_none());
                assert!(on_submit.is_some());
                assert!(on_cancel.is_none());
            }
            other => panic!("expected a plain field, got {}", if other.is_some() { "masked" } else { "nothing" }),
        }
    }

    /// A `secure_submit` beats a callback on the same node, and it has to: the masked path is the
    /// one that keeps bytes out of the Lua VM (ADR-0005), so a field declaring both must not have
    /// its keystrokes handed to a config.
    #[test]
    fn a_field_declaring_both_a_destination_and_a_callback_stays_masked() {
        let lua = Lua::new();
        let mut field = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let on_submit = lua.create_function(|_, _text: String| Ok(())).unwrap();
        field.properties.insert("on_submit".to_string(), Value::Function(on_submit));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(matches!(focused_field(&[&root, &field]), Some(FieldTarget::Masked(_))));
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
        // already being typed into (ADR-0050 decision 4), and wiping there would delete half
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
        // ADR-0050 decision 4: addressing this to `"unknown"/"unknown"` would put a password
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
        assert_eq!(focus_on_enter(&[], Some(&armed)), None, "an `enter` on a surface this process already destroyed");
        assert_eq!(
            focus_on_enter(&[("bar@TEST", &untypable)], Some(&armed)),
            None,
            "a surface whose tree names no destination"
        );

        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert_eq!(focus_on_enter(&[("screen@TEST", &typable)], None), Some(armed.clone()));

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
        assert_eq!(focus_on_enter(&[("screen@TEST", &two_fields)], Some(&pressed)), Some(pressed));
        assert_eq!(
            focus_on_enter(&[("screen@TEST", &two_fields)], Some(&field("bar@TEST", "network", "connect"))),
            None,
            "a field belonging to no surface in scope is not this scope's to keep"
        );
    }

    #[test]
    fn keyboard_focus_on_a_panel_takes_the_field_on_the_popup_shown_under_it() {
        // The network password prompt. `modules/bar/init.lua` raises the bar's
        // `keyboard_interactivity` when `network.password_ssid` appears, but that appears from a
        // click inside the panel popup that is already open, and niri only hands a popup the
        // keyboard if its parent held it when the popup mapped. So the keys arrive on the bar while
        // the only field in reach is on `panel_host`. Scoped to the entering surface alone this
        // armed nothing, and the prompt stayed dead until the panel was closed and reopened.
        let lua = Lua::new();
        let bar = tree_with(&lua, vec![]);
        let panel = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "network", "connect")))]);

        assert_eq!(
            focus_on_enter(&[("bar@TEST", &bar), ("panel_host@TEST", &panel)], None),
            Some(field("panel_host@TEST", "network", "connect")),
            "the field is armed on the surface that declares it, not on the one holding the keyboard"
        );

        // And the sole-field rule still spans the whole scope rather than each tree separately:
        // one field on the bar and one on its popup is still two destinations to guess between.
        let typable_bar =
            tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert_eq!(focus_on_enter(&[("bar@TEST", &typable_bar), ("panel_host@TEST", &panel)], None), None);
    }

    #[test]
    fn a_field_is_armed_only_while_its_own_surface_holds_the_keyboard_and_still_exists() {
        // The one question every keystroke asks, in place of a clearing call bolted onto each of the
        // five or six sites that can take a surface away. The liveness half is the traced leak: type
        // a login password on the lock screen, the compositor sends `finished`,
        // `teardown_lock_surfaces` destroys the `wl_surface` with no `leave` required to follow, so
        // the plaintext used to stay live in `App::secure_buffer`.
        let armed = field("screen@TEST", "lock", "authenticate");
        let scope = |ids: &[&str]| ids.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
        assert!(focus_is_still_armed(&armed, &scope(&["screen@TEST"]), true));
        assert!(
            !focus_is_still_armed(&armed, &scope(&["screen@TEST"]), false),
            "its `wl_surface` is gone, whether or not a `leave` ever came"
        );
        assert!(!focus_is_still_armed(&armed, &scope(&["bar@TEST"]), true), "another surface is receiving the keys");
        assert!(!focus_is_still_armed(&armed, &[], true), "the keyboard is on a surface this process does not own");
        // The half `keyboard_focus_scope` buys: the keyboard sits on the bar, the field is on the
        // popup shown under it, and a keystroke reaches it. Without this clause every key pruned
        // the focus `enter` had just armed.
        let on_popup = field("panel_host@TEST", "network", "connect");
        assert!(focus_is_still_armed(&on_popup, &scope(&["bar@TEST", "panel_host@TEST"]), true));
        assert!(!focus_is_still_armed(&on_popup, &scope(&["bar@TEST"]), true), "the popup is no longer shown");
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
        // guess at (ADR-0050 decision 4). A press still picks one, because a press names a node.
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
        let wrong_destination =
            tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")))]);
        assert!(!tree_can_authenticate(&wrong_destination));
    }

    fn key(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent { time: 0, raw_code: 0, keysym, utf8: utf8.map(str::to_string) }
    }

    #[test]
    fn a_focused_secure_field_reads_the_keyboard_directly() {
        // `zwp_text_input_v3` alone did not deliver this: it only produces a `commit_string` when
        // the compositor has an input method bound, so on a session with no IME not one byte
        // reached `SecureBuffer`.
        assert_eq!(key_action(&key(Keysym::a, Some("a")), false), KeyAction::Append("a"));
        assert_eq!(key_action(&key(Keysym::Return, Some("\r")), false), KeyAction::Submit);
        assert_eq!(key_action(&key(Keysym::KP_Enter, Some("\r")), false), KeyAction::Submit);
        assert_eq!(key_action(&key(Keysym::BackSpace, Some("\u{8}")), false), KeyAction::Backspace);
    }

    #[test]
    fn a_control_key_never_becomes_a_character_of_the_password() {
        // `utf8` is not empty for Escape, Tab or Return -- xkbcommon hands back the C0 control
        // character for each -- so an unfiltered append would silently put an ESC byte in the
        // middle of a secret that PAM then rejects with no visible reason.
        assert_eq!(key_action(&key(Keysym::Tab, Some("\t")), false), KeyAction::Ignore);
        assert_eq!(key_action(&key(Keysym::Shift_L, None), false), KeyAction::Ignore);
    }

    #[test]
    fn escape_throws_the_entry_away_instead_of_being_ignored() {
        // Escape used to reach the control-character filter above and be dropped, which left one
        // Backspace per character as the only way to abandon a mistyped password -- on the surface
        // where a wrong guess costs a counted PAM attempt and a `pam_unix` failure delay.
        assert_eq!(key_action(&key(Keysym::Escape, Some("\u{1b}")), false), KeyAction::Clear);
    }

    // ---- edit_plain_buffer (ADR-0102) ----

    #[test]
    fn escape_on_a_plain_field_without_on_cancel_clears_and_keeps_the_focus() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, false);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: false });
        assert!(buffer.is_empty());
    }

    #[test]
    fn escape_on_a_plain_field_with_on_cancel_clears_and_gives_the_field_up() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: true });
        assert!(buffer.is_empty());
    }

    /// An empty field has nothing for `on_change` to report, but Escape is still a cancel: the
    /// field was open and the user asked to leave it.
    #[test]
    fn escape_on_an_empty_field_cancels_without_reporting_a_change() {
        let mut buffer = String::new();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: false, submitted: false, cancelled: true });
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, false);
        assert_eq!(edit, PlainEdit { changed: false, submitted: false, cancelled: false }, "nothing at all to do");
    }

    #[test]
    fn typing_and_submitting_a_plain_field_never_cancel() {
        let mut buffer = String::new();
        assert_eq!(
            edit_plain_buffer(&mut buffer, KeyAction::Append("a"), true),
            PlainEdit { changed: true, submitted: false, cancelled: false }
        );
        assert_eq!(
            edit_plain_buffer(&mut buffer, KeyAction::Submit, true),
            PlainEdit { changed: true, submitted: true, cancelled: false }
        );
        assert_eq!(buffer, "a", "the caller empties the buffer after the submit, not this");
    }

    #[test]
    fn holding_enter_down_does_not_resubmit_an_already_scrubbed_buffer() {
        // A submit zeroizes the buffer as it reads it, so the second submit of a key repeat would
        // send an *empty* password to PAM and burn one of the user's attempts. Backspace and
        // ordinary characters repeat normally, which is what every text field does.
        assert_eq!(key_action(&key(Keysym::Return, Some("\r")), true), KeyAction::Ignore);
        assert_eq!(key_action(&key(Keysym::BackSpace, Some("\u{8}")), true), KeyAction::Backspace);
        assert_eq!(key_action(&key(Keysym::a, Some("a")), true), KeyAction::Append("a"));
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
        // ADR-0050 decision 3's exact worked example, which every config in the tree uses.
        let lua = Lua::new();
        let anchor: Function = lua.load(r#"anchor = nil return function(rect) anchor = rect end"#).eval().unwrap();
        call_on_click(&lua, &anchor, LogicalRect { x: 40.0, y: 0.0, width: 86.0, height: 24.0 }, "left").unwrap();

        let recorded: Table = lua.globals().get("anchor").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 40.0);
        assert_eq!(recorded.get::<f32>("height").unwrap(), 24.0);
    }
}
