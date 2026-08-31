# A display list is what makes a repaint skippable

An idle bar showing a clock repainted every surface it had mapped, once a second, forever. On this
machine that meant a 1920x34 bar and a 1920x1200 wallpaper, both redrawn and both committed to the
compositor, because one text node's seconds digit had advanced.

`repaint_mapped_surfaces`'s own doc comment named the cause exactly:

> Every surface, not the changed ones: ADR-0044 decision 2's dirty flag is one flag for the whole
> scene, so which surfaces changed is not information this process has.

That is true, and it stays true. This ADR is about getting the information anyway, without giving
up the single flag.

## The measurement first

Per second, so one digit could change: the Supervisor's 1 Hz `system` push marked the scene dirty,
`re_resolve_if_dirty` re-resolved all eleven surfaces, each was deep-cloned into a `ResolvedNode`,
and `repaint_mapped_surfaces` then repainted every mapped one and swapped its buffers.

Measured on an idle session, release build, over a 30 second window: the wallpaper was repainting
2.23 times a second and the bar 2.23 times a second. Neither number is 1.0 because a second
capability was pushing at cadence too, and finding out which was worth the probe: it was
`workspaces`, because the focused window's title genuinely changes when a terminal animates a
spinner into it. Not a defect, and worth writing down so nobody else goes looking for one.

## Decision 1: paint through a display list, not straight to the canvas

`paint_tree` walked the resolved tree and issued femtovg calls as it went. Nothing existed in
between, so there was nothing to compare, so every surface had to be redrawn on the chance it had
changed.

The walk is now two halves:

- `build(root, scale) -> DisplayList` flattens the tree into `Vec<DrawCmd>` of plain Rust data:
  a rect, a clip, and one of four `Draw` variants carrying already-parsed values.
- `execute(painter, images, list, scale)` turns that into femtovg calls.

`wayland::App::paint_surface` builds the list, compares it against the one that surface last
painted, and returns before touching the GL context when they match.

The list is the single source of truth for what gets drawn, which is the property that matters. A
comparison computed *alongside* the drawing (a hash of "the things paint reads", say) is two lists
that must agree forever, and the day they stop agreeing the symptom is a surface that silently
stops updating. Here, if it is not in the list it is not drawn, so equal lists cannot mean
different pixels.

## Decision 2: compare plain data, never `ResolvedNode`

The shorter-looking route is `#[derive(PartialEq)]` on `ResolvedNode` and comparing trees.

It cannot work. A node's properties are a `HashMap<String, mlua::Value>`, and mlua compares tables
by identity, not structure. A property whose signal resolves to a table gets a freshly built table
on every pass, so it compares unequal every time and the surface repaints forever. This is not a
guess: `Signal::set_changed` carries a test named
`set_changed_cannot_dedupe_a_table_because_table_equality_is_identity`, written when hover rects
hit the same wall.

Nothing in `Draw` holds a Lua value. `Rgba`, `BorderColor`, `EdgeInsets`, `Fit`, `LogicalRect` and
`PhysicalRect` all already derived `PartialEq` before this change.

Float equality is deliberate and is the right comparison here, though it is usually the wrong one.
Both sides come from the same parsers over the same property values, so an unchanged input is
bit-identical rather than merely close. `NaN` compares unequal to itself and so repaints forever,
which is the safe direction: too many frames, never a stale one.

## Decision 3: the clip is precomputed, not a save/restore nest

`paint_node` pushed `save` + `intersect_scissor` per node and `restore` on the way out. A flat list
has no nesting to hang that on, so each `DrawCmd` carries the intersection of its own snapped box
with every ancestor's, and `execute` calls `scissor` outright.

The two are equivalent because every clip here is an axis-aligned rect, intersection is
associative, and this crate applies no canvas transform: `TextPainter::resize` calls
`set_size(w, h, 1.0)` and nothing else touches it. The existing pixel tests are what actually
prove it, since they read the framebuffer back: `an_oversized_child_rect_is_clipped_to_its_parents_box`,
`a_text_wider_than_its_box_paints_nothing_outside_it` and
`a_childs_padded_offset_position_is_honoured_across_two_levels_of_nesting` all pass unchanged.

A subtree whose clip is empty is left out of the list entirely. The old walk recursed into it and
had every draw discarded by the scissor, so this is the same pixels for less work, and it also
means moving something fully off-screen produces no list change and so no repaint.

## Decision 4: invalidate on anything that makes the buffer undefined

`last_painted` holds `((width, height), DisplayList)`, and `None` means "must paint".

The size is in there because the same list at a new size is a different frame. It is cleared
outright when the surface is bound, because a fresh `EGLSurface`'s buffers hold nothing and the
pixels it recorded went with the old one. It is written only after `eglSwapBuffers` returns
success, since recording a frame that never reached the compositor would let the next identical
list skip a paint the screen never got.

Every branch that cannot prove the buffer still holds what `last_painted` claims clears it. The
failure modes are not symmetric: painting once too often costs a frame, while skipping once too
often leaves a stale surface with nothing scheduled to correct it.

## What it bought

Measured A/B on the same release binary, the skip gated behind an env var, 25 second windows on an
idle session:

| | renderer | niri |
|---|---|---|
| skip off | 0.80% of a core | 0.52% |
| skip on | 0.60% | 0.36% |

The wallpaper went from 2.23 repaints a second to zero. The bar still repaints when the clock's
seconds digit changes, and skips the pushes that do not touch it: 1.20 paints against 1.03 skips
per second.

The compositor's share is the part worth noting. A full-screen commit makes niri recomposite the
screen behind it, so a third of what this saved was never in this process at all.

## What it did not fix

The scene still re-resolves all eleven surfaces on every push, nine of them invisible, and each is
still deep-cloned into a `ResolvedNode` first. That is the next thing to measure, and it is a
separate decision because skipping an invisible subtree's resolution changes documented semantics:
`lock.lua` and `popup.lua` both note that a `:map` runs whether or not the node it feeds is
visible, and were written total because of it.

The single dirty flag stays. It is why `build` runs for every surface on every push, and with the
paint now skipped that is a tree walk and some parsing rather than a GPU round trip. Worth
revisiting only against a measurement that says it still matters.
