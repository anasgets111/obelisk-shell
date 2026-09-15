//! Seat input for `App`: the `wl_seat` capabilities here, pointer input in `pointer`, and keyboard
//! focus with `secure_submit` typing in `keyboard`.
use super::*;
mod keyboard;
mod pointer;

pub(super) use keyboard::{FocusedField, FocusedTextField};

#[cfg(test)]
pub(super) use pointer::rect_table;
pub(super) use pointer::{ActiveDrag, ArmedClick, ArmedSerial};

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    /// Pointer and keyboard only; there is no touch property. `is_none` guards are required:
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
                            "[obelisk-renderer] wl_seat::get_pointer failed; no button's on_click will ever fire: {e}"
                        )
                    }
                }
            }
            // Use the compositor keymap (`None` rmlvo); there is no `on_key` for this shell to
            // interpret, so imposing a layout would serve no policy.
            Capability::Keyboard if self.keyboard.is_none() => match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                // Nonfatal, but `enter`/`leave` stop tracking focus and stale textfield focus may
                // outlive the user.
                Err(e) => eprintln!(
                    "[obelisk-renderer] wl_seat::get_keyboard failed; keyboard focus will never be tracked: {e}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Hands each hand-built `ResolvedNode` its own id. A `Scene` allocates these in production and
    /// these tests have no `Scene`; the only property that matters is that two nodes are never
    /// accidentally the same node.
    static NEXT_TEST_NODE_ID: AtomicU64 = AtomicU64::new(1);

    pub(super) fn hit_node(
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
}
