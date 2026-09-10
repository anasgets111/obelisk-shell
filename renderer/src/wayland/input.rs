//! Pointer (`on_click`, ADR-0050; `on_drag`/`on_wheel`, ADR-0116) and keyboard input.
//! `secure_submit`
//! keystrokes become a `SecureSubmit` frame without a Lua value holding plaintext (ADR-0005/0027).

use super::*;
use crate::layout::secure_submit::{sole_secure_submit_in_scope, typable_secure_submit_targets};

/// Mouse-wheel notch size in logical pixels (ADR-0069 decision 6): flat 39, approximating three
/// lines of the shipped config's 13px text. A per-container step would need a font size; touchpads
/// report pixels and never use it.
const WHEEL_STEP_PIXELS: f32 = 39.0;

/// Scroll distance in logical pixels (ADR-0069 decision 6). Use touchpad `pixels` as sent; use
/// `value120` (120 per notch) only without pixels, or compositors sending both double the motion.
fn wheel_delta(pixels: f64, steps: i32) -> f32 {
    if pixels != 0.0 {
        return pixels as f32;
    }
    steps as f32 / 120.0 * WHEEL_STEP_PIXELS
}

/// `on_wheel`'s notch value (ADR-0116 decision 2): positive away from the user. Negate Wayland's
/// positive-towards-user axis and divide by 39; touchpad pixels therefore produce fractional,
/// smooth slider motion.
fn wheel_steps(pixels: f64, steps: i32) -> f32 {
    -wheel_delta(pixels, steps) / WHEEL_STEP_PIXELS
}

/// One press waiting for its release (ADR-0050 decision 2). The rect stands in for identity:
/// moving the button between press and release cancels the click.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ArmedClick {
    instance_id: String,
    rect: LogicalRect,
    /// The `href` under the press inside `text` (ADR-0106), so a paragraph's two links do not share
    /// a click identity.
    link: Option<String>,
    /// Evdev code, requiring release of the same button (ADR-0050 second amendment). ponytail: one
    /// armed click, so chording drops both; upgrade to an `ArrayVec` of three or small map for
    /// chords.
    button: u32,
}
/// Serial for `xdg_popup.grab` and its surface (ADR-0049 amendment, ADR-0051 decision 1). Set by
/// [`PointerHandler::pointer_frame`], consumed by [`run`]'s poll loop after `dispatch_pending`:
/// resolving inside the callback would nest `Scene::apply` and Lua/Wayland creation while its
/// queue is being dispatched. Press and release both arm it, latest wins. `xdg_shell` only
/// requires a serial from "a real input event", so a stale real-input serial gets normal
/// `popup_done` refusal (ADR-0051 decision 3). Keep `instance_id`, not the tracked index, because
/// hotplug can rename indices before re-resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArmedSerial {
    pub(super) serial: u32,
    pub(super) instance_id: String,
}
/// Innermost `button` with callable `on_click` in a hit path (ADR-0050 decision 1). Scan inward:
/// the deepest node is normally the button's `text` child. A button without a handler is
/// transparent, and only `Value::Function` counts; `layout::node` leaves the key opaque (§ 5.2).
fn clickable_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(LogicalRect, Option<&'a Function>, bool)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let on_click = match node.properties.get("on_click") {
            Some(Value::Function(f)) => Some(f),
            _ => None,
        };
        let submit = matches!(node.properties.get("submit"), Some(Value::Boolean(true)));
        if on_click.is_none() && !submit {
            return None;
        }
        Some((layout::hit::absolute_rect(&path[..=depth])?, on_click, submit))
    })
}
/// Innermost `button` with callable `on_drag` (ADR-0116 decision 1); unhandled buttons are
/// transparent, so a handle inside a draggable track leaves the track draggable.
fn draggable_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(LogicalRect, &'a Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let Some(Value::Function(on_drag)) = node.properties.get("on_drag") else {
            return None;
        };
        Some((layout::hit::absolute_rect(&path[..=depth])?, on_drag))
    })
}
/// Innermost `button` with callable `on_wheel` (ADR-0116 decision 2).
fn wheel_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(usize, LogicalRect, &'a Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let Some(Value::Function(on_wheel)) = node.properties.get("on_wheel") else {
            return None;
        };
        Some((depth, layout::hit::absolute_rect(&path[..=depth])?, on_wheel))
    })
}
/// Left press on a button with `on_drag`, held until release (ADR-0116 decision 1). Every Motion
/// calls it wherever the pointer goes; config clamping keeps a slider pinned at its end.
#[derive(Clone)]
pub(super) struct ActiveDrag {
    instance_id: String,
    rect: LogicalRect,
    handler: Function,
}
/// Press/release target: handler, click-identity rect (see [`ArmedClick`]), and link `href`.
#[derive(Clone)]
struct Clickable {
    rect: LogicalRect,
    handler: Option<Function>,
    link: Option<String>,
    /// `submit = true` sends the scope's armed `secure_submit` field on release, like Enter
    /// (ADR-0114); it is the only button path to a password, with no Lua callback.
    submit: bool,
}
/// Links precede buttons (ADR-0106): a text `on_link` with an `href` under `point` wins over an
/// ancestor button, while plain text is transparent. A `textfield` similarly arms no click
/// (ADR-0092 decision 7).
fn clickable(
    path: &[&layout::ResolvedNode],
    point: layout::hit::LogicalPoint,
    shaping: &ShapingHandle,
) -> Option<Clickable> {
    let link = path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "text" {
            return None;
        }
        let Some(Value::Function(on_link)) = node.properties.get("on_link") else {
            return None;
        };
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        let local = layout::hit::LogicalPoint { x: point.x - rect.x, y: point.y - rect.y };
        let href = layout::hit::link_under(node, local, shaping)?;
        Some(Clickable { rect, handler: Some(on_link.clone()), link: Some(href), submit: false })
    });
    link.or_else(|| {
        clickable_button(path).map(|(rect, on_click, submit)| Clickable {
            rect,
            handler: on_click.cloned(),
            link: None,
            submit,
        })
    })
}
/// Both targets from decision 1's single [`layout::hit::hit_path`] traversal. Two walks could
/// re-resolve between them and give one event two answers.
struct PointerHit {
    button: Option<Clickable>,
    field: Option<FieldTarget>,
    /// `on_drag` button under the press, with its rect (ADR-0116 decision 1).
    drag: Option<(LogicalRect, Function)>,
}
/// Innermost pressed `textfield` (ADR-0092): masked fields address a capability and never Lua
/// (ADR-0005); plain fields send edits to Lua.
#[derive(Debug)]
enum FieldTarget {
    Masked(node::SecureSubmitTarget),
    Plain {
        /// Node identity lets paint find it across passes that move it (ADR-0099).
        id: layout::scene::NodeId,
        on_change: Option<Function>,
        on_submit: Option<Function>,
        on_cancel: Option<Function>,
        on_navigate: Option<Function>,
    },
}
/// Innermost pressed `textfield` (ADR-0050 decision 4, § 5.2 item 8, ADR-0092). A `secure_submit`
/// table is masked and targets the whole capability/action identity, since that is where its bytes
/// go; otherwise callbacks make it plain. `paint_style`
/// parses the destination during `Scene::apply`, so malformed secure targets fail the pass.
/// Masked fields without a destination and plain fields without callbacks return `None` rather
/// than taking a keyboard they cannot use. Plain fields use `ResolvedNode`'s stable `NodeId`
/// (ADR-0099); [`ArmedClick`] still uses a rect because press/release trees rarely move.
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
    Some(FieldTarget::Plain {
        id: field.id,
        on_change,
        on_submit,
        on_cancel: function("on_cancel"),
        on_navigate: function("on_navigate"),
    })
}
/// First plain `autofocus = true` field in scope document order (ADR-0112). Skip masked fields and
/// fields without callbacks; unlike two `secure_submit` fields, duplicate search boxes are a config
/// mistake, so deterministic order beats refusing both. A hidden subtree is skipped whole: it is
/// frozen (ADR-0124) and cannot take keys, and one surface that holds several modals' cards keeps
/// the closed ones hidden beside the open one.
fn autofocus_field_in_scope(scope: &[(&str, &layout::ResolvedNode)]) -> Option<(String, FieldTarget)> {
    for (surface_id, tree) in scope {
        let mut stack = vec![*tree];
        while let Some(node) = stack.pop() {
            if !node.visible || node.leaving {
                continue;
            }
            if node.kind == "textfield"
                && matches!(node.properties.get("autofocus"), Some(Value::Boolean(true)))
                && let Some(target @ FieldTarget::Plain { .. }) = focused_field(&[node])
            {
                return Some((surface_id.to_string(), target));
            }
            stack.extend(node.children.iter().rev());
        }
    }
    None
}
/// Focused plain `textfield`, its Lua callbacks, and readable draft (ADR-0092, § 5.2 item 8).
/// The `String` outlives keyboard focus while the node exists (ADR-0108); `typing` records whether
/// a press selected it, while `keyboard_focus` controls current keys and caret drawing.
#[derive(Debug, Clone)]
pub(super) struct FocusedTextField {
    surface_id: String,
    id: layout::scene::NodeId,
    buffer: String,
    /// A press selected it; off keeps text without a caret and sends keys nowhere.
    typing: bool,
    on_change: Option<Function>,
    on_submit: Option<Function>,
    on_cancel: Option<Function>,
    on_navigate: Option<Function>,
}

/// One plain-field key edit before callbacks (ADR-0092, ADR-0102); split from
/// [`App::apply_plain_key`] so Escape is testable without a Wayland seat.
#[derive(Debug, PartialEq, Eq)]
struct PlainEdit {
    /// Buffer text changed, so `on_change` fires.
    changed: bool,
    /// Enter submits the text and empties the buffer.
    submitted: bool,
    /// Escape with `on_cancel` drops focus and fires it.
    cancelled: bool,
    /// Arrow, Tab, or paging key: buffer stays; `on_navigate` hears the name.
    navigated: Option<&'static str>,
}

impl PlainEdit {
    /// No-op edit, allowing [`App::apply_plain_key`] to return early.
    const NONE: PlainEdit = PlainEdit { changed: false, submitted: false, cancelled: false, navigated: None };
}

/// Apply `action` to `buffer`. Escape always clears; with `on_cancel` it also leaves (ADR-0092
/// decision 6), otherwise the config cannot know the field stopped taking keys.
fn edit_plain_buffer(buffer: &mut String, action: KeyAction<'_>, cancels: bool) -> PlainEdit {
    match action {
        KeyAction::Append(text) => {
            buffer.push_str(text);
            PlainEdit { changed: true, ..PlainEdit::NONE }
        }
        KeyAction::Backspace => PlainEdit { changed: buffer.pop().is_some(), ..PlainEdit::NONE },
        KeyAction::Clear => {
            let had = !buffer.is_empty();
            buffer.clear();
            PlainEdit { changed: had, cancelled: cancels, ..PlainEdit::NONE }
        }
        KeyAction::Submit => PlainEdit { changed: true, submitted: true, ..PlainEdit::NONE },
        KeyAction::Navigate(key) => PlainEdit { navigated: Some(key), ..PlainEdit::NONE },
        KeyAction::Ignore => PlainEdit::NONE,
    }
}
/// A secure submit frame, or `None` without a destination (ADR-0050 decision 4). Do not send to
/// `"unknown"/"unknown"`; the buffer is zeroized on both branches.
///
/// Both refusals say so. Dropping a submit here is indistinguishable from a lock screen that has
/// stopped accepting the password: nothing reaches the Supervisor, so nothing downstream can
/// report it, and a session that will not unlock leaves no line anywhere. Neither branch names the
/// secret or its length.
fn submit_frame_for(
    generation_id: u32,
    target: Option<&node::SecureSubmitTarget>,
    buffer: &mut shared::SecureBuffer,
) -> Option<RendererFrame> {
    // An empty buffer would spend a PAM attempt and `pam_unix` failure delay, so reject it before
    // checking the destination.
    let empty = buffer.is_empty();
    let Some(target) = target.filter(|_| !empty) else {
        match target {
            None => eprintln!(
                "secure submit dropped: no field is focused to send it to, so a password typed here reaches nothing"
            ),
            Some(target) => {
                eprintln!("secure submit to {}/{} dropped: the field is empty", target.capability, target.action)
            }
        }
        buffer.zeroize();
        return None;
    };
    Some(secure_submit_frame(generation_id, &target.capability, &target.action, buffer))
}
/// Focused `secure_submit` field and declaring surface. The surface id distinguishes live keyboard
/// focus from a client-destroyed surface, which need not receive `wl_keyboard.leave`; otherwise a
/// lock password could remain in `App::secure_buffer` and later bar keys append to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FocusedField {
    /// `"{id}@{output}"` instance id declaring the field.
    surface_id: String,
    target: node::SecureSubmitTarget,
}
/// Sole writer of `focused_secure_submit`: `SecureBuffer` belongs to the field typed into, not its
/// transport. The three transitions (`KeyboardHandler::leave`, keyboard capability removal, and
/// pointer retarget) all scrub; any new caller inherits that guarantee. A destination change
/// scrubs, but re-arming the same field does not (ADR-0050 decision 4). Free for unit-testing the
/// read/zeroize contract without Wayland, as with [`secure_submit_frame`].
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
/// Reconcile secure focus on keyboard enter from the scoped trees. Empty/untracked scopes and
/// scopes without one `secure_submit` return `None` through [`App::focus_secure_submit`], so moving
/// focus cannot leave keys addressed to the old field. Keep a current field only if still declared
/// in scope, and still reachable there: a prompt that hides while focused is as gone as one a
/// reload deleted. The compositor's `enter` commonly follows a press, so discarding current focus
/// would make a multi-field surface untypable by clicking. Otherwise,
/// [`sole_secure_submit_in_scope`] refuses to guess among several fields; reloads cannot keep
/// deleted targets.
fn focus_on_enter(scope: &[(&str, &layout::ResolvedNode)], current: Option<&FocusedField>) -> Option<FocusedField> {
    let still_declared = |field: &&FocusedField| {
        scope
            .iter()
            .any(|(id, tree)| *id == field.surface_id && typable_secure_submit_targets(tree).contains(&field.target))
    };
    if let Some(current) = current.filter(still_declared) {
        return Some(current.clone());
    }
    let (surface_id, target) = sole_secure_submit_in_scope(scope)?;
    Some(FocusedField { surface_id: surface_id.to_string(), target })
}
/// A field is armed only if its surface is in the current key scope and still has a live
/// `wl_surface`. Both clauses are required: defect 2 left a field on a `keyboard_interactivity =
/// none` panel armed, and defect 3 left a destroyed lock-screen field armed because no `leave` was
/// guaranteed. Use the same parent-plus-popup `scope` as [`sole_secure_submit_in_scope`], or enter
/// could arm a field the next key prunes.
fn focus_is_still_armed(field: &FocusedField, scope: &[String], its_surface_is_live: bool) -> bool {
    scope.contains(&field.surface_id) && its_surface_is_live
}
/// Whether a plain field takes the keys arriving now. A masked field armed anywhere in scope takes
/// them all: the two focuses are held independently, and `apply_key` offers a key to both, so a
/// prompt revealed while a plain field was already typing would otherwise put every character of a
/// password through that field's `on_change` -- into Lua, which is the one place a `secure_submit`
/// secret must never reach (ADR-0005). "Masked focus wins" is the rule
/// `arm_autofocus_if_nothing_is_typing`
/// already states for arming; this is the same rule for the keys themselves.
///
/// The draft survives, exactly as it does when the surface loses the keyboard (ADR-0108): the field
/// stops taking keys and stops drawing a caret, and is typable again when the prompt is answered.
fn plain_field_takes_keys(typing: bool, its_surface_is_in_scope: bool, a_masked_field_is_armed: bool) -> bool {
    typing && its_surface_is_in_scope && !a_masked_field_is_armed
}
/// One key event's action for a focused `secure_submit`; borrow the SCTK `KeyEvent` text, avoiding
/// another allocation.
#[derive(Debug, PartialEq, Eq)]
enum KeyAction<'a> {
    Append(&'a str),
    Backspace,
    /// Escape clears and stays in the field.
    Clear,
    Submit,
    /// Navigation name for a plain field (ADR-0112); masked fields ignore it.
    Navigate(&'static str),
    Ignore,
}
/// Convert one `wl_keyboard` key for `secure_submit`. Use xkb, not `zwp_text_input_v3`: without an
/// IME, text-input-v3 emits no `commit_string`; a dormant binding could also let the compositor
/// route an IME into the buffer and create two writers (ADR-0027 amendment). The ordinary
/// Lua-readable `textfield` still needs IME composition (§ 5.2 item 8). No IDL is added: secure
/// bytes go to the native buffer and Supervisor (ADR-0005); misses are [`Ignore`]d.
///
/// [`Ignore`d]: KeyAction::Ignore
///
/// Filter control characters by text, not keysym: xkbcommon returns C0 text for Escape, Tab, and
/// Return, and appending it would put an invisible ESC in a PAM password. Ignore repeated Enter;
/// [`secure_submit_frame`] zeroizes on submit, so repeat would send an empty PAM attempt.
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
        // PAM counts wrong attempts; Escape clears a mistyped password without Backspace-per-char.
        Keysym::Escape => KeyAction::Clear,
        // Before `utf8`: xkbcommon returns Tab as `"\t"`, which the control filter would drop.
        Keysym::Up | Keysym::KP_Up => KeyAction::Navigate("up"),
        Keysym::Down | Keysym::KP_Down => KeyAction::Navigate("down"),
        Keysym::Page_Up | Keysym::KP_Page_Up => KeyAction::Navigate("page_up"),
        Keysym::Page_Down | Keysym::KP_Page_Down => KeyAction::Navigate("page_down"),
        Keysym::Tab | Keysym::KP_Tab => KeyAction::Navigate("tab"),
        Keysym::ISO_Left_Tab => KeyAction::Navigate("backtab"),
        _ => match event.utf8.as_deref() {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => KeyAction::Append(text),
            _ => KeyAction::Ignore,
        },
    }
}
/// Lua name for an evdev button, or `None` when this engine ignores it. Use strings, not raw 273 or
/// normalized 1/2/3, matching other categorical values. `None` means no arm or release: exposing
/// `BTN_TASK` as `"other"` would make it indistinguishable from `BTN_EXTRA` and could run a left
/// handler. ponytail: only left/right/middle of SCTK's eight names are handled. Upgrade:
/// `BTN_SIDE`/`BTN_EXTRA` (0x113/0x114), `BTN_BACK`/`BTN_FORWARD` (0x116/0x115).
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some("left"),
        BTN_RIGHT => Some("right"),
        BTN_MIDDLE => Some("middle"),
        _ => None,
    }
}
/// Whether this release ends the armed press. Completion also needs surface, rect, and link; ending
/// needs only the button, so drag-off releases end it. A different release must not clear it: left,
/// right, left must retain the right press.
fn release_ends_press(armed: Option<&ArmedClick>, button: u32) -> bool {
    armed.is_some_and(|armed| armed.button == button)
}
/// Whether a release completes `armed` (ADR-0050 decision 2). It must hit a handled button with the
/// same instance, rect, link, and button; matching coordinates on a re-resolved plain rect is not
/// the original click. `released_on` comes from [`clickable_button`], not raw position.
fn release_completes_click(
    armed: Option<&ArmedClick>,
    instance_id: &str,
    released_on: Option<(LogicalRect, Option<&str>)>,
    button: u32,
) -> bool {
    armed.zip(released_on).is_some_and(|(armed, (rect, link))| {
        armed.instance_id == instance_id
            && armed.rect == rect
            && armed.link.as_deref() == link
            && armed.button == button
    })
}
/// Call `on_click` with its button rect in surface logical coordinates (ADR-0050 decision 3). The
/// rect round-trips to popup `anchor_rect` through Lua (§ 6). Error labels distinguish building the
/// engine's argument from a raised config handler.
fn call_on_click(
    lua: &Lua,
    on_click: &Function,
    rect: LogicalRect,
    button: &str,
) -> Result<(), (&'static str, mlua::Error)> {
    let argument = rect_table(lua, rect).map_err(|e| ("could not build on_click's rect argument", e))?;
    on_click.call::<()>((argument, button)).map_err(|e| ("on_click raised, ignoring it", e))
}
/// Call `on_drag` with the rect, pointer in button-local coordinates, and gesture phase (ADR-0116
/// decision 1). Local coordinates avoid repeated subtraction in handlers; unclamped coordinates
/// let each config apply its own `min`/`max`.
fn call_on_drag(
    lua: &Lua,
    on_drag: &Function,
    rect: LogicalRect,
    position: (f64, f64),
    phase: &str,
) -> Result<(), (&'static str, mlua::Error)> {
    let rect_argument = rect_table(lua, rect).map_err(|e| ("could not build on_drag's rect argument", e))?;
    let pointer = lua.create_table().map_err(|e| ("could not build on_drag's pointer argument", e))?;
    pointer.set("x", position.0 as f32 - rect.x).map_err(|e| ("could not build on_drag's pointer argument", e))?;
    pointer.set("y", position.1 as f32 - rect.y).map_err(|e| ("could not build on_drag's pointer argument", e))?;
    on_drag.call::<()>((rect_argument, pointer, phase)).map_err(|e| ("on_drag raised, ignoring it", e))
}
/// Build `on_click`'s button rect `{ x, y, width, height }` in surface logical coordinates
/// (ADR-0050 decision 3).
pub(super) fn rect_table(lua: &Lua, rect: LogicalRect) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", rect.x)?;
    table.set("y", rect.y)?;
    table.set("width", rect.width)?;
    table.set("height", rect.height)?;
    Ok(table)
}
/// Build a `RendererFrame::SecureSubmit` (ADR-0005/ADR-0027). Read once with `expose_secret`, then
/// zeroize before the frame leaves this thread; the socket thread scrubs its copy after its wire
/// write in `crate::socket::pump`. Kept free for unit-testing without a live `wl_surface`.
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
    pub(super) fn focus_secure_submit(&mut self, next: Option<FocusedField>) {
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

    /// Shadow-gate key: the scope, in [`App::shown_popups_under`] order because
    /// [`autofocus_field_in_scope`] takes the first match, plus whether the focused root still has
    /// a live `wl_surface`.
    ///
    /// The liveness bit is separate because [`App::keyboard_focus_scope`] admits a tracked root
    /// without one, while popup membership already requires `popup: Some(_)`. Without it, a root
    /// whose surface died and returned reads as unchanged.
    pub(super) fn focus_key(&self) -> (Vec<String>, bool) {
        let scope = self.keyboard_focus_scope();
        let root_live = scope.first().is_some_and(|id| self.surface_is_live(id));
        (scope, root_live)
    }

    /// Ask [`focus_on_enter`] over scoped trees; called both on `enter` and when trees change under
    /// an existing focus.
    fn field_the_scope_declares(&self, scope: &[String], current: Option<FocusedField>) -> Option<FocusedField> {
        let trees: Vec<(&str, &layout::ResolvedNode)> =
            scope.iter().filter_map(|id| self.client.scene().surface(id).map(|tree| (id.as_str(), tree))).collect();
        focus_on_enter(&trees, current.as_ref())
    }

    /// Arm a newly visible sole `secure_submit` when the tree changes under existing focus. Enter
    /// alone misses the network prompt: the bar receives its one `enter` when the panel opens, then
    /// a click sets `network.password_ssid` and reveals the field without moving focus. Changing
    /// layer `keyboard_interactivity` instead breaks the popup grab; niri dismissed that popup in
    /// the same frame the field armed. Only arm when empty; [`sole_secure_submit_in_scope`] refuses
    /// to guess among several fields (ADR-0050 decision 4).
    pub(super) fn arm_secure_focus_if_the_scope_now_declares_one(&mut self) {
        if self.focused_secure_submit.is_some() || self.keyboard_focus.is_none() {
            return;
        }
        let scope = self.keyboard_focus_scope();
        let Some(field) = self.field_the_scope_declares(&scope, None) else {
            return;
        };
        // Destroyed surfaces may retain `keyboard_focus` without a `leave`; arming would scrub and
        // re-arm on every pass.
        if !self.surface_is_live(&field.surface_id) {
            return;
        }
        eprintln!(
            "[oblisk-renderer] {}'s `secure_submit` field ({}/{}) became typable under the keyboard focus already held",
            field.surface_id, field.target.capability, field.target.action
        );
        self.focus_secure_submit(Some(field));
    }

    /// Give keys to `autofocus` with a fresh empty buffer (ADR-0112). ADR-0108 preserves drafts
    /// when the user returns manually; automatic handoff must not append to a forgotten search.
    /// Fire `on_change("")` on every arm so launchers reset selection/scroll and state clears.
    fn arm_autofocus_field(&mut self, scope: &[String]) {
        let trees: Vec<(&str, &layout::ResolvedNode)> =
            scope.iter().filter_map(|id| self.client.scene().surface(id).map(|tree| (id.as_str(), tree))).collect();
        let Some((surface_id, FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate })) =
            autofocus_field_in_scope(&trees)
        else {
            return;
        };
        drop(trees);
        // A closed launcher can retain its tree and focus id without a `leave`; require its live
        // `wl_surface` or every turn would arm then prune the same field.
        if !self.surface_is_live(&surface_id) {
            return;
        }
        let opened = on_change.clone();
        eprintln!("[oblisk-renderer] {surface_id}'s `autofocus` textfield takes the keyboard");
        self.focus_text_field(Some(FocusedTextField {
            surface_id: surface_id.clone(),
            id,
            buffer: String::new(),
            typing: true,
            on_change,
            on_submit,
            on_cancel,
            on_navigate,
        }));
        if let Some(on_change) = opened
            && let Err(e) = on_change.call::<()>(String::new())
        {
            eprintln!("[oblisk-renderer] {surface_id}: on_change raised, ignoring it: {e}");
        }
    }

    /// Arm a newly appearing `autofocus` field under existing focus (ADR-0112), unless a plain
    /// field is typing or a press just stopped that same field. A different field is new; masked
    /// focus wins as on `enter`.
    pub(super) fn arm_autofocus_if_nothing_is_typing(&mut self) {
        if self.focused_secure_submit.is_some() || self.keyboard_focus.is_none() {
            return;
        }
        self.prune_text_field_focus();
        let scope = self.keyboard_focus_scope();
        if let Some(field) = self.focused_text_field.as_ref() {
            if field.typing {
                return;
            }
            let trees: Vec<(&str, &layout::ResolvedNode)> =
                scope.iter().filter_map(|id| self.client.scene().surface(id).map(|tree| (id.as_str(), tree))).collect();
            let same_field = matches!(
                autofocus_field_in_scope(&trees),
                Some((_, FieldTarget::Plain { id, .. })) if id == field.id
            );
            if same_field {
                return;
            }
        }
        self.arm_autofocus_field(&scope);
    }

    /// Drop stale secure focus and scrub its half-typed secret before every keystroke; all
    /// transitions use [`App::focus_secure_submit`], even when no `leave` followed.
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

    /// Focus for `surface_id` in `layout::paint::FieldFocus` form. Keep masked `{ capability,
    /// action }` routing here; paint receives only its filled count, never secret bytes.
    pub(super) fn field_focus_for(&self, surface_id: &str) -> Option<layout::paint::FieldFocus<'_>> {
        if let Some(focused) = self.focused_secure_submit.as_ref().filter(|f| f.surface_id == surface_id) {
            return Some(layout::paint::FieldFocus::Masked {
                target: &focused.target,
                filled: self.secure_buffer.char_count(),
            });
        }
        let focused = self.focused_text_field.as_ref().filter(|f| f.surface_id == surface_id)?;
        Some(layout::paint::FieldFocus::Plain {
            id: focused.id,
            text: &focused.buffer,
            caret: self.text_field_takes_keys(focused),
        })
    }

    /// Poll-turn cleanup for a destroyed surface. Only liveness is checked here: checking routing
    /// would disarm a multi-field press before its matching `enter`, which `sole_secure_submit`
    /// cannot re-choose. This bounds plaintext residency when lock teardown gets no `leave`.
    pub(super) fn drop_secure_focus_if_its_surface_is_gone(&mut self) {
        let gone = self.focused_secure_submit.as_ref().is_some_and(|field| !self.surface_is_live(&field.surface_id));
        if gone {
            eprintln!(
                "[oblisk-renderer] the surface holding the focused secure_submit field is gone; dropping it and scrubbing its buffer"
            );
            self.focus_secure_submit(None);
        }
    }

    /// Apply one secure key (ADR-0005). Focus is the destination gate; masked fields without one
    /// are never focused. Bytes go `KeyEvent` → native `SecureBuffer` → Supervisor, never Lua.
    fn apply_secure_key(&mut self, event: &KeyEvent, repeat: bool) {
        let action = key_action(event, repeat);
        if self.focused_secure_submit.is_none() {
            // Enter with nothing focused is the shape a stuck lock screen takes: the keys went
            // nowhere, `finish_secure_submit` is never reached, and every refusal log lives below
            // this return. Only the submit key says so, or an unfocused keyboard would log per
            // keystroke.
            //
            // A plain field taking keys is not that shape: both focuses see every key, so Enter in
            // a notification reply or a search box arrives here with an `on_submit` waiting for it.
            if matches!(action, KeyAction::Submit)
                && !self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field))
            {
                eprintln!(
                    "[oblisk-renderer] submit pressed while no secure field holds focus; nothing was typed into one and nothing was sent"
                );
            }
            return;
        }
        // Append/backspace/clear all change the drawn character count.
        self.field_input_changed = true;
        match action {
            KeyAction::Append(text) => self.secure_buffer.push_str(text),
            // `pop_char` zeroizes dropped bytes, not just the length.
            KeyAction::Backspace => {
                self.secure_buffer.pop_char();
            }
            // Use the transition seam to scrub, then re-arm the same field for retyping.
            KeyAction::Clear => {
                let field = self.focused_secure_submit.clone();
                self.focus_secure_submit(None);
                self.focus_secure_submit(field);
            }
            KeyAction::Submit => self.finish_secure_submit(),
            // Password prompts have no navigation.
            KeyAction::Navigate(_) | KeyAction::Ignore => {}
        }
    }

    /// Apply one key to either field kind (ADR-0092), pruning both focuses once before dispatch.
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
        // Liveness of the surface and of the node, not of the keyboard (ADR-0108): a field whose
        // surface lost the keyboard keeps its draft and simply takes no keys until it is back
        // ([`App::text_field_takes_keys`]). A field whose node is gone -- the reply was sent or
        // closed and the row removed -- has nowhere to show a draft, and its callbacks belong to a
        // card that no longer exists, so the next key is what finally lets it go.
        let node_exists = self
            .client
            .scene()
            .surface(&field.surface_id)
            .is_some_and(|tree| layout::hit::contains_node(tree, field.id));
        if self.surface_is_live(&field.surface_id) && node_exists {
            return;
        }
        eprintln!("[oblisk-renderer] the focused textfield is gone; dropping what was typed");
        self.focus_text_field(None);
    }

    /// Whether a key arriving now belongs to `field`: a press chose it, and its surface is one the
    /// keyboard is on (ADR-0108). The same question decides the caret, so what is drawn as live is
    /// what a key would land in.
    fn text_field_takes_keys(&self, field: &FocusedTextField) -> bool {
        plain_field_takes_keys(
            field.typing,
            self.keyboard_focus_scope().contains(&field.surface_id),
            self.focused_secure_submit.is_some(),
        )
    }

    /// Apply one plain `textfield` key (ADR-0092, § 5.2 item 8). Callbacks receive whole text, not
    /// deltas: state bindings want the snapshot, and reassembling deltas is caller work. Submit
    /// leaves the field focused and empty. Escape clears; without `on_cancel`, focus stays because
    /// config cannot observe focus and a silent key stop has no visible signal. With `on_cancel`,
    /// drop focus first, then call it (ADR-0102), so a callback changing the surface finds no stale
    /// focus.
    fn apply_plain_key(&mut self, event: &KeyEvent, repeat: bool) {
        if !self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field)) {
            return;
        }
        let Some(field) = self.focused_text_field.as_mut() else {
            return;
        };
        let edit = edit_plain_buffer(&mut field.buffer, key_action(event, repeat), field.on_cancel.is_some());
        if edit == PlainEdit::NONE {
            return;
        }
        // Clone before callbacks can write a signal and re-resolve the scene.
        let (text, on_change, on_submit, on_cancel, on_navigate, surface_id) = {
            let field = self.focused_text_field.as_ref().expect("the focus was Some a moment ago");
            (
                field.buffer.clone(),
                field.on_change.clone(),
                field.on_submit.clone(),
                field.on_cancel.clone(),
                field.on_navigate.clone(),
                field.surface_id.clone(),
            )
        };
        // Navigation changes neither text nor caret, so it needs no repaint.
        if let Some(key) = edit.navigated {
            if let Some(on_navigate) = on_navigate
                && let Err(e) = on_navigate.call::<()>(key)
            {
                eprintln!("[oblisk-renderer] {surface_id}: on_navigate raised, ignoring it: {e}");
            }
            return;
        }
        if edit.submitted {
            // Empty before the callback can open a popup or re-resolve the scene.
            if let Some(field) = self.focused_text_field.as_mut() {
                field.buffer.clear();
            }
        }
        if edit.cancelled {
            self.focus_text_field(None);
        }
        self.field_input_changed = true;
        deliver_plain_edit(&surface_id, edit, text, PlainCallbacks { on_change, on_submit, on_cancel });
    }

    /// Build and queue a completed `secure_submit`; [`submit_frame_for`] reads once and scrubs on
    /// both the destination-missing and empty-buffer paths (ADR-0050 decision 4).
    fn finish_secure_submit(&mut self) {
        // Which of the two, not "one of these". A lock screen that will not open needs the log to
        // separate "the password went nowhere" from "you submitted an empty field", and the old
        // line named both and settled neither.
        let addressed = self.focused_secure_submit.as_ref().map(|field| field.target.clone());
        let nothing_typed = self.secure_buffer.is_empty();
        let target = self.focused_secure_submit.as_ref().map(|field| &field.target);
        let Some(frame) = submit_frame_for(self.generation_id, target, &mut self.secure_buffer) else {
            match addressed {
                None => eprintln!(
                    "[oblisk-renderer] secure_submit dropped: no focused textfield named a capability and action to address it to, so nothing was sent"
                ),
                Some(target) if nothing_typed => eprintln!(
                    "[oblisk-renderer] secure_submit to {}/{} dropped: nothing had been typed",
                    target.capability, target.action
                ),
                Some(target) => eprintln!(
                    "[oblisk-renderer] secure_submit to {}/{} dropped for no recorded reason; this is a bug",
                    target.capability, target.action
                ),
            }
            return;
        };
        if let Err(e) = self.outbound_tx.send(frame) {
            eprintln!("[oblisk-renderer] failed to queue SecureSubmit for the socket thread: {e}");
        }
    }

    /// One hit-test answers button, field, and drag (ADR-0050 decisions 1/4). Coordinates are
    /// logical surface-local while `paint_surface` remains scale `1.0`; a future HiDPI change must
    /// move this conversion with `paint_surface` and `apply_input_region`. Handlers and targets are
    /// cloned out of the lent tree; the tree itself is not.
    fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit { button: None, field: None, drag: None };
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(tree, point);
        PointerHit {
            button: clickable(&path, point, &self.shaping),
            field: focused_field(&path),
            drag: draggable_button(&path).map(|(rect, handler)| (rect, handler.clone())),
        }
    }

    /// Fire one held drag edge for `instance_id` (ADR-0116 decision 1); `end` clears it before the
    /// callback so re-entry finds nothing held. Handler raises are logged and swallowed.
    fn fire_on_drag(&mut self, instance_id: &str, position: (f64, f64), phase: &str) {
        let Some(drag) = self.drag.as_ref().filter(|drag| drag.instance_id == instance_id).cloned() else {
            return;
        };
        if phase == "end" {
            self.drag = None;
        }
        if let Err((what, e)) = call_on_drag(self.client.lua(), &drag.handler, drag.rect, position, phase) {
            eprintln!("[oblisk-renderer] {instance_id}: {what}: {e}");
        }
    }

    /// Apply one wheel event to the deepest scrollable or `on_wheel` button (ADR-0116 decision 2).
    /// Use touchpad pixels or `value120` (ADR-0069 decision 6); ignore deprecated `discrete`,
    /// which compositors that still send also accompany with `value120`.
    /// Innermost wins with no parent chaining: a wheel over a list stops there at its end, unlike
    /// browser chaining to the parent; no config needs those edge cases. The offset written is
    /// unclamped: `layout::scene` owns the bound and writes back what it used.
    /// `on_wheel` receives the vertical axis only.
    fn scroll_at(
        &mut self,
        index: usize,
        position: (f64, f64),
        horizontal_px: f64,
        horizontal_steps: i32,
        vertical_px: f64,
        vertical_steps: i32,
    ) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(tree) = self.client.scene().surface(&surface_id) else {
            return;
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(tree, point);
        // Deepest scrollable under the pointer wins.
        let scrollable = path.iter().enumerate().rev().find_map(|(depth, node)| {
            let signal = layout::scene::scroll_signal(&node.properties)?;
            let axis = layout::scene::main_axis_of(&node.kind, &node.properties).ok()??;
            Some((depth, signal, axis))
        });
        let wheel = wheel_button(&path);
        if let Some((depth, rect, on_wheel)) = wheel
            && scrollable.as_ref().is_none_or(|(scroll_depth, ..)| depth > *scroll_depth)
        {
            let steps = wheel_steps(vertical_px, vertical_steps);
            if steps == 0.0 {
                return;
            }
            let on_wheel = on_wheel.clone();
            drop(path);
            match rect_table(self.client.lua(), rect) {
                Ok(rect) => {
                    if let Err(e) = on_wheel.call::<()>((rect, steps)) {
                        eprintln!("[oblisk-renderer] {surface_id}: on_wheel raised, ignoring it: {e}");
                    }
                }
                Err(e) => eprintln!("[oblisk-renderer] {surface_id}: could not build on_wheel's rect argument: {e}"),
            }
            return;
        }
        let Some((_, signal, axis)) = scrollable else {
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

    /// The cursor [`layout::hit::cursor_under`] chooses under `position` (ADR-0107). Split from
    /// [`Self::show_cursor`] so the choosing ends its borrow of the lent tree before the showing
    /// writes through `&mut self`.
    fn cursor_for(&self, tree: Option<&layout::ResolvedNode>, position: (f64, f64)) -> cursor_icon::CursorIcon {
        let Some(tree) = tree else {
            return cursor_icon::CursorIcon::Default;
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        layout::hit::cursor_under(&layout::hit::hit_path(tree, point), point, &self.shaping)
    }

    /// Set the cursor only when it changes; motion arrives per pixel and each `set_shape` would add
    /// compositor work. `Leave` clears the cache because the shape is bound to the next enter
    /// serial.
    fn show_cursor(&mut self, shape: cursor_icon::CursorIcon) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        if self.cursor_shown == Some(shape) {
            return;
        }
        match pointer.set_cursor(&self.conn, shape) {
            Ok(()) => self.cursor_shown = Some(shape),
            // Remember failure as shown; a missing themed cursor is reported once, not per pixel.
            Err(err) => {
                eprintln!("[oblisk-renderer] could not set the cursor to {}: {err}", shape.name());
                self.cursor_shown = Some(shape);
            }
        }
    }

    /// Re-run hover writes after layout moves rows under a stationary pointer, without `on_hover`
    /// (ADR-0112 amendment): the row that slid away stops reading hovered and the row now under
    /// the pointer starts, or stale tint follows the old row off the viewport. No user crossing
    /// occurred.
    pub(super) fn refresh_hover_after_layout(&mut self) {
        let Some((surface_id, position)) = self.pointer_at.clone() else {
            return;
        };
        let Some(index) = self.surfaces.iter().position(|tracked| tracked.surface_id == surface_id) else {
            return;
        };
        let tree = self.client.scene().surface(&surface_id);
        self.sync_hover(index, tree, Some(position), false);
    }

    /// Write all `hover` signals, or clear them for `None` (ADR-0062). Collect writes before
    /// `set_changed` because the tree borrow must end; only moved values dirty the scene (decision
    /// 4), so a stationary pointer inside one button re-resolves nothing while device-rate motion
    /// continues (ADR-0044 decision 2). `fire` enables `on_hover` only for motion/leave, not enter-
    /// motion or re-layout.
    ///
    /// Takes the tree rather than fetching it so one lookup serves this and the cursor.
    ///
    fn sync_hover(&self, index: usize, tree: Option<&layout::ResolvedNode>, position: Option<(f64, f64)>, fire: bool) {
        // Skip the expensive tree walk when no config registered `hover(name)`.
        if !crate::lua::signal::any_hover_registered(self.client.lua()) {
            return;
        }
        let Some(tree) = tree else {
            return;
        };
        let point = position.map(|(x, y)| layout::hit::LogicalPoint { x: x as f32, y: y as f32 });
        let writes = layout::hover::hover_writes(tree, point);
        let lua = self.client.lua();
        for write in writes {
            // Non-hover signals stay untouched, so `hover = oblisk.network` cannot overwrite a
            // capability snapshot (ADR-0062 decision 2).
            let Some(handle) = write.signal.hover_handle() else {
                continue;
            };
            // The boolean gates rect and callback writes.
            let crossed = handle.set_changed(mlua::Value::Boolean(write.hovered));
            // Fire only on edges (ADR-0095): device-rate motion could call a handler hundreds of
            // times across one button. Swallow handler errors like `fire_on_click`.
            if crossed
                && fire
                && let Some(on_hover) = &write.on_hover
                && let Err(err) = on_hover.call::<()>(write.hovered)
            {
                eprintln!("[oblisk-renderer] {}: on_hover handler raised: {err}", self.surfaces[index].surface_id);
            }
            // Rects are edge-only, not merely an optimization: mlua table equality is identity, so
            // a fresh equal table would undo decision 4 on every motion.
            if crossed
                && let Some(rect) = write.rect
                && let Some(rect_handle) = write.signal.hover_rect_handle()
            {
                match rect_table(lua, rect) {
                    Ok(table) => rect_handle.set(mlua::Value::Table(table)),
                    // The boolean landed; keep the last tooltip position on table-build failure.
                    Err(err) => eprintln!(
                        "[oblisk-renderer] {}: could not build a hover rect: {err}",
                        self.surfaces[index].surface_id
                    ),
                }
            }
        }
    }

    /// Call a button's `on_click` with its rect (ADR-0050 decision 3); swallow handler raises.
    /// ADR-0046 rescue is for failed evaluation, not a misbehaving callback.
    fn fire_on_click(&mut self, instance_id: &str, rect: LogicalRect, button: &str, on_click: &Function) {
        // `signal:set()` marks its own dirty flag (ADR-0044 decision 5); this call need not.
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

    /// Pointer and keyboard only; § 5.2 has no touch property. `is_none` guards are required:
    /// `wl_seat::capabilities` restates the full set: gaining a keyboard re-announces the pointer,
    /// and duplicate SCTK objects would duplicate events into one armed/focus state.
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            // Themed pointer (ADR-0107): SCTK uses `wp_cursor_shape_v1` when available, otherwise
            // XCursor via `wl_shm`; its cursor surface dies with the pointer.
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor_state.create_surface(qh);
                match self.seat_state.get_pointer_with_theme::<Self, ()>(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::default(),
                ) {
                    Ok(pointer) => self.pointer = Some(pointer),
                    // Nonfatal: painting, reload, and keyboard input remain; only `on_click` stops.
                    Err(e) => {
                        eprintln!(
                            "[oblisk-renderer] wl_seat::get_pointer failed; no button's on_click will ever fire: {e}"
                        )
                    }
                }
            }
            // Use the compositor keymap (`None` rmlvo); § 5.2 has no `on_key` for this shell to
            // interpret, so imposing a layout would serve no policy.
            Capability::Keyboard if self.keyboard.is_none() => match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                // Nonfatal, but `enter`/`leave` stop tracking focus and stale textfield focus may
                // outlive the user.
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
                // No pointer means no release; clear the press like `leave` (ADR-0050 decision 2).
                self.armed = None;
                self.cursor_shown = None;
                // `ThemedPointer::drop` releases `wl_pointer` (`since="3"`), shape device, and
                // cursor surface (src/seat/pointer/mod.rs:567).
                self.pointer = None;
            }
            Capability::Keyboard => {
                // No keyboard means no leave; clear stale focus and its half-typed secret
                // (ADR-0050 decision 4).
                self.keyboard_focus = None;
                self.focus_secure_submit(None);
                if let Some(keyboard) = self.keyboard.take() {
                    // `wl_keyboard::release` is `since="3"` too (wayland.xml).
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

/// Pointer input to `on_click` (ADR-0050); dispatch is delegated by `delegate_dispatch2!(App)`.
impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // `wl_pointer` is per seat; an event may name a surface destroyed by `visible` or
            // output change.
            let Some(index) = self.index_of_surface(&event.surface) else {
                continue;
            };
            match event.kind {
                // Left/right/middle only (ADR-0050 second amendment); other codes cannot arm or
                // fire a config handler.
                PointerEventKind::Press { button, serial, .. } => {
                    if pointer_button_name(button).is_none() {
                        continue;
                    }
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // ADR-0049 amendment: this turn's re-resolve consumes it; `run` clears it at
                    // turn end. Both click edges arm it (see [`ArmedSerial`]).
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    // ADR-0051 amendment: preserve the "user asked again" stamp past disarm.
                    self.pointer_input_count += 1;
                    let hit = self.hit_under(index, event.position);
                    // Press, not release, chooses focus (decision 4). Rewrite both focus halves on
                    // every press: reply and password fields must displace each other. Remember
                    // whether this press hit a field before consuming the match; a held draft is
                    // still a plain field (ADR-0108).
                    let pressed_a_field = hit.field.is_some();
                    let (masked, plain) = match hit.field {
                        Some(FieldTarget::Masked(target)) => {
                            (Some(FocusedField { surface_id: instance_id.clone(), target }), None)
                        }
                        // Re-pressing the same field resumes its draft (ADR-0108).
                        Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate }) => {
                            let buffer = self
                                .focused_text_field
                                .as_ref()
                                .filter(|field| field.id == id)
                                .map(|field| field.buffer.clone())
                                .unwrap_or_default();
                            (
                                None,
                                Some(FocusedTextField {
                                    surface_id: instance_id.clone(),
                                    id,
                                    buffer,
                                    typing: true,
                                    on_change,
                                    on_submit,
                                    on_cancel,
                                    on_navigate,
                                }),
                            )
                        }
                        // Elsewhere stops plain typing but keeps its draft (ADR-0108). Masked focus
                        // remains until another field takes it (ADR-0114 decision 8), so scrim,
                        // card, and Authenticate clicks do not discard a secret before `submit`.
                        None => (
                            self.focused_secure_submit.clone(),
                            self.focused_text_field.clone().map(|field| FocusedTextField { typing: false, ..field }),
                        ),
                    };
                    // Reassign through the zeroizing transition seam.
                    self.focus_secure_submit(masked);
                    self.focus_text_field(plain);
                    // A textfield press arms no click, so an ancestor button cannot fire
                    // (ADR-0092); § 5.2 makes textfield a leaf. This keeps notification reply boxes
                    // from also activating the card.
                    self.armed = hit.button.filter(|_| !pressed_a_field).map(|clickable| ArmedClick {
                        instance_id: instance_id.clone(),
                        rect: clickable.rect,
                        link: clickable.link,
                        button,
                    });
                    // Left `on_drag` holds until release (ADR-0116 decision 1); other buttons stay
                    // free for clicks, and fields drag nothing just as they click nothing.
                    if button == BTN_LEFT
                        && !pressed_a_field
                        && let Some((rect, handler)) = hit.drag
                    {
                        self.drag = Some(ActiveDrag { instance_id: instance_id.clone(), rect, handler });
                        self.fire_on_drag(&instance_id, event.position, "start");
                    }
                }
                PointerEventKind::Release { button, serial, .. } => {
                    let Some(name) = pointer_button_name(button) else {
                        continue;
                    };
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // Clicks fire on release (ADR-0050 decision 2), so this serial arms a popup
                    // opened by `on_click`.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    self.pointer_input_count += 1;
                    // Release does not change focus; drag-off must not un-focus a textfield. End
                    // drag before click so a combined control commits before its click handler.
                    if button == BTN_LEFT {
                        self.fire_on_drag(&instance_id, event.position, "end");
                    }
                    let hit = self.hit_under(index, event.position).button;
                    let fires = release_completes_click(
                        self.armed.as_ref(),
                        &instance_id,
                        hit.as_ref().map(|clickable| (clickable.rect, clickable.link.as_deref())),
                        button,
                    );
                    // Clear before callback re-entry; only the matching button ends the slot.
                    if release_ends_press(self.armed.as_ref(), button) {
                        self.armed = None;
                    }
                    if let Some(clickable) = hit.filter(|_| fires) {
                        match (clickable.link, clickable.handler) {
                            // Links take `href`, not the paragraph rect; a link is not a button.
                            (Some(href), Some(handler)) => {
                                if let Err(e) = handler.call::<()>(href) {
                                    eprintln!("[oblisk-renderer] {instance_id}: on_link raised, ignoring it: {e}");
                                }
                            }
                            (_, handler) => {
                                if clickable.submit {
                                    self.prune_secure_focus();
                                    self.finish_secure_submit();
                                }
                                if let Some(handler) = handler {
                                    self.fire_on_click(&instance_id, clickable.rect, name, &handler);
                                }
                            }
                        }
                    }
                }
                // Release will land elsewhere: cancel the armed click. `None` clears all hover
                // (ADR-0062), since no later Motion may arrive to close a tooltip.
                PointerEventKind::Leave { .. } => {
                    // End held drag at the last position; no release reaches this surface.
                    if let Some((_, position)) = self.pointer_at.clone() {
                        let instance_id = self.surfaces[index].surface_id.clone();
                        self.fire_on_drag(&instance_id, position, "end");
                    }
                    self.armed = None;
                    self.cursor_shown = None;
                    self.pointer_at = None;
                    let tree = self.client.scene().surface(&self.surfaces[index].surface_id);
                    self.sync_hover(index, tree, None, true);
                }
                // Motion off the armed rect does not disarm; return and release still click. Enter
                // and Motion update hover, but only Motion fires `on_hover` (ADR-0112 amendment):
                // an Enter over a surface opened under a resting pointer is layout movement, not a
                // user choice. Actual movement sends Motion shortly after.
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let moved = matches!(event.kind, PointerEventKind::Motion { .. });
                    let instance_id = self.surfaces[index].surface_id.clone();
                    self.pointer_at = Some((instance_id.clone(), event.position));
                    if moved {
                        self.fire_on_drag(&instance_id, event.position, "move");
                    }
                    // One lookup serves both; `Scene::surface` lends its tree.
                    let tree = self.client.scene().surface(&self.surfaces[index].surface_id);
                    self.sync_hover(index, tree, Some(event.position), moved);
                    // Chosen while the tree is still borrowed, shown once that borrow has ended.
                    let shape = self.cursor_for(tree, event.position);
                    self.show_cursor(shape);
                }
                // Wheel (ADR-0069); use the event's own position.
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

/// Keyboard focus selected by the compositor; `keyboard_interactivity` controls eligibility, and
/// `wl_keyboard` enter/leave reports the result. Dispatch is delegated by `delegate_dispatch2!`.
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
        // Ignore already-held `raw`/`keysyms`. An enter may name a surface destroyed after the
        // compositor sent it (`visible` flip or output change); `None` is not an error.
        self.keyboard_focus = self.surface_id_for(surface).map(str::to_string);
        // Redraw the caret of a field whose keyboard returned (ADR-0108; see `leave`).
        self.field_input_changed |= self.focused_text_field.is_some();
        // Include shown child popups, where a panel password prompt lives
        // (see [`App::keyboard_focus_scope`]).
        let scope = self.keyboard_focus_scope();
        // A scope with exactly one `secure_submit` becomes typable without a click.
        let next = self.field_the_scope_declares(&scope, self.focused_secure_submit.clone());
        match (&self.keyboard_focus, &next) {
            (None, _) => eprintln!("[oblisk-renderer] keyboard focus entered an untracked surface; not tracking it"),
            (Some(id), Some(field)) => eprintln!(
                "[oblisk-renderer] keyboard focus entered {id} and takes {}'s `secure_submit` field ({}/{})",
                field.surface_id, field.target.capability, field.target.action
            ),
            // Report the searched popup scope so "no field" distinguishes out-of-reach from hidden.
            (Some(id), None) => eprintln!(
                "[oblisk-renderer] keyboard focus entered {id}, and neither it nor its shown popups {:?} declare a sole `secure_submit` field",
                &scope[1..]
            ),
        }
        // Always disarm when nothing is found, or keys remain addressed to the previous field.
        let secure_armed = next.is_some();
        self.focus_secure_submit(next);
        // ADR-0112: absent masked focus or an already-typing plain field, scope `autofocus` takes
        // keys; focus-follows-mouse may enter repeatedly, so a typing field keeps its draft.
        let typing_here =
            self.focused_text_field.as_ref().is_some_and(|field| field.typing && scope.contains(&field.surface_id));
        if !secure_armed && !typing_here {
            self.arm_autofocus_field(&scope);
        }
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        // Clear unconditionally: protocol orders old-surface leave before new-surface enter.
        let left = self.keyboard_focus.take().unwrap_or_else(|| "an untracked surface".to_string());
        // ADR-0050 decision 4: elsewhere means no submit will arrive; clear secure focus and the
        // armed press like pointer Leave.
        self.focus_secure_submit(None);
        // Keep the plain draft (ADR-0108): OnDemand/focus-follows-mouse temporarily removes the
        // keyboard, not the reply. It stops keys/caret until focus returns.
        self.field_input_changed |= self.focused_text_field.is_some();
        self.armed = None;
        eprintln!("[oblisk-renderer] keyboard focus left {left}");
    }

    // § 5.2 has no key-handler property, and ADR-0050 adds none: `secure_submit` (ADR-0005) sends
    // `KeyEvent` bytes through native `SecureBuffer` to Supervisor, never Lua. See [`key_action`].
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

    // Empty by design: SCTK release events have no `utf8`, and modifiers/layout do not edit a
    // buffer. `KeyboardHandler` provides no default bodies.
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

/// The three callbacks one plain-field edit may deliver. Grouped so [`deliver_plain_edit`] takes an
/// argument per idea rather than one per callback.
struct PlainCallbacks {
    on_change: Option<Function>,
    on_submit: Option<Function>,
    on_cancel: Option<Function>,
}

/// Delivers one plain-field edit's callbacks in the order a config can rely on (ADR-0189).
///
/// `on_submit` before `on_change`. A submit clears the buffer, and the `on_change` that reports the
/// clearing carries the empty string; delivering it first hands a config the empty field before the
/// text that filled it. `dev-config`'s launcher derives its selection from its query, so that order
/// wiped the query, resolved the rows against an empty needle, and launched the first entry of the
/// unfiltered list rather than the row on screen. Submit carries the user's intent and goes first;
/// the clear is bookkeeping and follows.
///
/// Free rather than a method so the ordering can be tested with recording closures; the dispatch it
/// came out of needs a whole `App`, which is why this was never covered.
fn deliver_plain_edit(surface_id: &str, edit: PlainEdit, text: String, callbacks: PlainCallbacks) {
    let PlainCallbacks { on_change, on_submit, on_cancel } = callbacks;
    if edit.submitted
        && let Some(on_submit) = on_submit
        && let Err(e) = on_submit.call::<()>(text.clone())
    {
        eprintln!("[oblisk-renderer] {surface_id}: on_submit raised, ignoring it: {e}");
    }
    if edit.changed
        && let Some(on_change) = on_change
        && let Err(e) = on_change.call::<()>(if edit.submitted { String::new() } else { text })
    {
        eprintln!("[oblisk-renderer] {surface_id}: on_change raised, ignoring it: {e}");
    }
    if edit.cancelled
        && let Some(on_cancel) = on_cancel
        && let Err(e) = on_cancel.call::<()>(())
    {
        eprintln!("[oblisk-renderer] {surface_id}: on_cancel raised, ignoring it: {e}");
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
    fn wheel_steps_are_notches_positive_away_from_the_user() {
        assert_eq!(wheel_steps(0.0, -120), 1.0, "one notch up is +1");
        assert_eq!(wheel_steps(0.0, 240), -2.0, "two notches down are -2");
        assert_eq!(wheel_steps(-WHEEL_STEP_PIXELS as f64 / 2.0, 0), 0.5, "a touchpad swipe is a fraction of a notch");
        assert_eq!(wheel_steps(0.0, 0), 0.0);
    }

    #[test]
    fn the_innermost_on_drag_button_is_the_one_that_takes_the_drag_and_a_bare_button_is_transparent() {
        let lua = Lua::new();
        let inner = hit_node(&lua, "button", (5.0, 2.0, 20.0, 20.0), true);
        let mut track = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), false);
        track.properties.insert("on_drag".to_string(), Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        track.children.push(inner);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(track);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let (rect, _) = draggable_button(&path).expect("the track carries the on_drag");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
        assert!(wheel_button(&path).is_none(), "nothing on this path declares on_wheel");
    }

    #[test]
    fn on_drag_is_handed_the_pointer_in_the_buttons_own_coordinates() {
        let lua = Lua::new();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let handler = lua
            .create_function(move |_, (rect, pointer, phase): (Table, Table, String)| {
                sink.borrow_mut().push((
                    rect.get::<f32>("width").unwrap(),
                    pointer.get::<f32>("x").unwrap(),
                    pointer.get::<f32>("y").unwrap(),
                    phase,
                ));
                Ok(())
            })
            .unwrap();
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        call_on_drag(&lua, &handler, rect, (30.0, 10.0), "start").unwrap();
        // Past the right edge: unclamped, so the config's own clamp is what pins the slider.
        call_on_drag(&lua, &handler, rect, (60.0, 10.0), "end").unwrap();
        assert_eq!(*seen.borrow(), vec![(40.0, 20.0, 6.0, "start".to_string()), (40.0, 50.0, 6.0, "end".to_string())]);
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
        // ADR-0005, ADR-0027: the frame carries the exact secret this thread accumulated, tagged
        // with this process's own generation_id, and the source buffer is scrubbed in the same
        // breath as the read rather than left live.
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
            displayed_source: None,
            dissolve: None,
            tweens: Vec::new(),
            leaving: false,
            blur: false,
            transform: crate::layout::node::Transform::default(),
            margin: crate::layout::node::EdgeInsets::default(),
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
        let (rect, ..) = clickable_button(&path).expect("the button carries an on_click");
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
        let (rect, ..) = clickable_button(&path).expect("the outer button carries the on_click");
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
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_LEFT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
        // Dragged off the button, then released: the release hits no button at all.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", None, BTN_LEFT));
        // Dragged onto a different button on the same surface.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((moved, None)), BTN_LEFT));
        // Same button geometry, different surface -- two panels can resolve identical rects.
        assert!(!release_completes_click(Some(&armed), "notification_area@eDP-1", Some((rect, None)), BTN_LEFT));
        // A release with nothing armed (a press that hit no button, or a `leave` in between).
        assert!(!release_completes_click(None, "bar@eDP-1", Some((rect, None)), BTN_LEFT));
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
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_RIGHT };

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
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_RIGHT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_RIGHT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_MIDDLE));
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

    /// ADR-0189. A submit clears the buffer and reports that clearing through `on_change("")`. If
    /// that lands before `on_submit`, a config reading its own query at submit time sees an empty
    /// one -- which launched the first entry of an unfiltered list instead of the row on screen.
    #[test]
    fn a_submit_reaches_on_submit_before_the_on_change_that_reports_the_clearing() {
        let lua = mlua::Lua::new();
        lua.load("log = {}").exec().unwrap();
        let record = |name: &'static str| {
            let lua_ref = &lua;
            lua_ref
                .create_function(move |lua, text: mlua::Value| {
                    let seen = match text {
                        mlua::Value::String(s) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    let log: mlua::Table = lua.globals().get("log")?;
                    log.push(format!("{name}({seen})"))?;
                    Ok(())
                })
                .unwrap()
        };
        let callbacks =
            PlainCallbacks { on_change: Some(record("change")), on_submit: Some(record("submit")), on_cancel: None };
        let edit = PlainEdit { changed: true, submitted: true, ..PlainEdit::NONE };
        deliver_plain_edit("launcher@eDP-1", edit, "calc".to_string(), callbacks);

        let order: Vec<String> =
            lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(
            order,
            vec!["submit(calc)".to_string(), "change()".to_string()],
            "submit must carry the text, and the empty change must follow it"
        );
    }

    /// An ordinary keystroke is unaffected: `on_change` carries the text and nothing else fires.
    #[test]
    fn a_plain_keystroke_reports_the_text_through_on_change_alone() {
        let lua = mlua::Lua::new();
        lua.load("log = {}").exec().unwrap();
        let on_change = lua
            .create_function(|lua, text: String| {
                let log: mlua::Table = lua.globals().get("log")?;
                log.push(format!("change({text})"))?;
                Ok(())
            })
            .unwrap();
        let callbacks = PlainCallbacks { on_change: Some(on_change), on_submit: None, on_cancel: None };
        deliver_plain_edit(
            "launcher@eDP-1",
            PlainEdit { changed: true, ..PlainEdit::NONE },
            "cal".to_string(),
            callbacks,
        );

        let order: Vec<String> =
            lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(order, vec!["change(cal)".to_string()]);
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
            Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate }) => {
                assert_eq!(id, field.id, "the field's own node, not the root it was reached through");
                assert!(on_navigate.is_none());
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
        // One per-keystroke question replaces clearing calls at five or six teardown sites. The
        // liveness half is the traced leak: type a login password on the lock screen, the
        // compositor sends `finished`, `teardown_lock_surfaces` destroys the `wl_surface` with no
        // `leave` required to follow, so the plaintext used to stay live in `App::secure_buffer`.
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

    /// A scene `lock` tree with a password field somewhere under its root.
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
    fn an_armed_password_prompt_takes_the_keys_away_from_a_plain_field_that_was_typing() {
        // The two focuses are independent and `apply_key` offers a key to both. The panel host
        // reveals its network prompt while a notification reply may already be typing, so without
        // this every character of that password would also arrive at the reply field's `on_change`.
        assert!(plain_field_takes_keys(true, true, false));
        assert!(!plain_field_takes_keys(true, true, true), "the password is not also typed into the reply box");
        // Unchanged either way: a field no press chose, and one on a surface the keyboard left.
        assert!(!plain_field_takes_keys(false, true, false));
        assert!(!plain_field_takes_keys(true, false, false));
    }

    #[test]
    fn a_hidden_secure_submit_field_neither_takes_the_keyboard_nor_hides_the_shown_one() {
        // The single-tree panel host declares one prompt per panel body and shows one at a time.
        // The walk used to ignore `visible`, so the network password field -- invisible, and only
        // ever revealed by `password_ssid` -- was the scope's sole destination whenever that
        // surface held the keyboard: it swallowed keys meant for the panel that was open, and an
        // `autofocus` plain field beside it never armed at all.
        let lua = Lua::new();
        let mut hidden = textfield(&lua, Some(secure_submit_table(&lua, "network", "connect")));
        hidden.visible = false;
        let mut leaving = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        leaving.leaving = true;
        let shown = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let tree = tree_with(&lua, vec![hidden, leaving, shown]);

        assert_eq!(
            sole_secure_submit(&tree),
            Some(target("lock", "authenticate")),
            "the one field a key can arrive at is the sole one, whatever the hidden siblings declare"
        );
        // The unfiltered reading still sees all three, because that is the one capability startup
        // wants: a prompt has to register its agent before the capability can ask it for anything.
        assert_eq!(crate::layout::secure_submit::secure_submit_targets(&tree).len(), 3);
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
        // middle of a secret that PAM then rejects with no visible reason. Tab is a navigation
        // key now (ADR-0112); what matters here is that it is still not an `Append`.
        assert_eq!(key_action(&key(Keysym::Tab, Some("\t")), false), KeyAction::Navigate("tab"));
        assert_eq!(key_action(&key(Keysym::Shift_L, None), false), KeyAction::Ignore);
        assert_eq!(key_action(&key(Keysym::Control_L, Some("\u{1b}")), false), KeyAction::Ignore);
    }

    #[test]
    fn escape_throws_the_entry_away_instead_of_being_ignored() {
        // Escape used to reach the control-character filter above and be dropped, which left one
        // Backspace per character as the only way to abandon a mistyped password -- on the surface
        // where a wrong guess costs a counted PAM attempt and a `pam_unix` failure delay.
        assert_eq!(key_action(&key(Keysym::Escape, Some("\u{1b}")), false), KeyAction::Clear);
    }

    // ---- edit_plain_buffer (ADR-0102) ----

    /// A paragraph holding two links is one rect (ADR-0106): pressing one and releasing over the
    /// other is not a click on either, and a release on the plain words is not a click on the link.
    #[test]
    fn a_link_click_completes_only_on_the_same_link() {
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 200.0, height: 40.0 };
        let armed = ArmedClick {
            instance_id: "bar@eDP-1".to_string(),
            rect,
            link: Some("https://a/".to_string()),
            button: BTN_LEFT,
        };
        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, Some("https://a/"))), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, Some("https://b/"))), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
    }

    #[test]
    fn escape_on_a_plain_field_without_on_cancel_clears_and_keeps_the_focus() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, false);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: false, navigated: None });
        assert!(buffer.is_empty());
    }

    #[test]
    fn escape_on_a_plain_field_with_on_cancel_clears_and_gives_the_field_up() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: true, navigated: None });
        assert!(buffer.is_empty());
    }

    /// An empty field has nothing for `on_change` to report, but Escape is still a cancel: the
    /// field was open and the user asked to leave it.
    #[test]
    fn escape_on_an_empty_field_cancels_without_reporting_a_change() {
        let mut buffer = String::new();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: false, submitted: false, cancelled: true, navigated: None });
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Clear, false);
        assert_eq!(
            edit,
            PlainEdit { changed: false, submitted: false, cancelled: false, navigated: None },
            "nothing at all to do"
        );
    }

    #[test]
    fn typing_and_submitting_a_plain_field_never_cancel() {
        let mut buffer = String::new();
        assert_eq!(
            edit_plain_buffer(&mut buffer, KeyAction::Append("a"), true),
            PlainEdit { changed: true, submitted: false, cancelled: false, navigated: None }
        );
        assert_eq!(
            edit_plain_buffer(&mut buffer, KeyAction::Submit, true),
            PlainEdit { changed: true, submitted: true, cancelled: false, navigated: None }
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

    /// ADR-0112: the keys a single-line field cannot edit with reach the config by name. Tab is the
    /// one that has to be checked, since xkbcommon hands it back as `"\t"` and the `utf8` arm
    /// below it would drop that as a control character.
    #[test]
    fn arrow_paging_and_tab_keys_navigate_instead_of_editing() {
        assert_eq!(key_action(&key(Keysym::Up, None), false), KeyAction::Navigate("up"));
        assert_eq!(key_action(&key(Keysym::Down, None), true), KeyAction::Navigate("down"), "held Down keeps moving");
        assert_eq!(key_action(&key(Keysym::Page_Down, None), false), KeyAction::Navigate("page_down"));
        assert_eq!(key_action(&key(Keysym::Tab, Some("\t")), false), KeyAction::Navigate("tab"));
        assert_eq!(key_action(&key(Keysym::ISO_Left_Tab, None), false), KeyAction::Navigate("backtab"));

        let mut buffer = "fire".to_string();
        let edit = edit_plain_buffer(&mut buffer, KeyAction::Navigate("down"), true);
        assert_eq!(edit, PlainEdit { navigated: Some("down"), ..PlainEdit::NONE });
        assert_eq!(buffer, "fire", "moving through the results is not an edit");
    }

    fn autofocus_textfield(lua: &Lua) -> layout::ResolvedNode {
        let mut node = plain_textfield(lua);
        node.properties.insert("autofocus".to_string(), Value::Boolean(true));
        node
    }

    /// ADR-0112: the field the keyboard is handed to unasked. Only a plain field that could take
    /// keys qualifies, and with two the first in document order does, since two search boxes on one
    /// surface is a mistake to pick through rather than a secret to refuse routing.
    #[test]
    fn the_first_plain_autofocus_field_in_the_scope_is_the_one_armed() {
        let lua = Lua::new();
        let first = autofocus_textfield(&lua);
        let second = autofocus_textfield(&lua);
        let (first_id, second_id) = (first.id, second.id);
        let tree = tree_with(&lua, vec![plain_textfield(&lua), first, second]);
        match autofocus_field_in_scope(&[("launcher@eDP-1", &tree)]) {
            Some((surface, FieldTarget::Plain { id, .. })) => {
                assert_eq!(surface, "launcher@eDP-1");
                assert_eq!(id, first_id, "document order, not {second_id:?}");
            }
            other => panic!("expected the first autofocus field, got {other:?}"),
        }

        // A hidden card's field is out of reach: the next visible one is armed instead.
        let mut hidden = autofocus_textfield(&lua);
        hidden.visible = false;
        let shown = autofocus_textfield(&lua);
        let shown_id = shown.id;
        let tree = tree_with(&lua, vec![hidden, shown]);
        match autofocus_field_in_scope(&[("modal_host@eDP-1", &tree)]) {
            Some((_, FieldTarget::Plain { id, .. })) => assert_eq!(id, shown_id),
            other => panic!("expected the visible field, got {other:?}"),
        }

        // Masked, or declaring nothing that could read the keys: not candidates, whatever they say.
        let mut masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        masked.properties.insert("autofocus".to_string(), Value::Boolean(true));
        let mut mute = textfield(&lua, None);
        mute.properties.insert("autofocus".to_string(), Value::Boolean(true));
        let none = tree_with(&lua, vec![masked, mute, plain_textfield(&lua)]);
        assert!(autofocus_field_in_scope(&[("launcher@eDP-1", &none)]).is_none());
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
