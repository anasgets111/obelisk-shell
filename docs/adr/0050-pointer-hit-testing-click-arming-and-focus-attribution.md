# Pointer hit-testing walks a path, a click is press-and-release on one node, and focus attributes the secret

`on_click` has been an inert `mlua::Value` in a node's property map since ADR-0021 item 2. Phase 21
calls it. Three things have to be decided before it can be called at all, and none of them are
recoverable cheaply once a config depends on the answer.

## Decision 1: hit-testing returns the path, not the topmost node

The obvious shape is "return the deepest visible node containing the point." It is wrong for the
only tree anyone writes: a `button` whose child is a `text`. The deepest node under the pointer is
the `text`, which has no `on_click`, so the button never fires.

Hit-testing returns the whole chain instead, root-first and deepest-last, and each caller scans it
from the deep end for what it wants. `on_click` looks for the innermost `button`; focus attribution
(decision 3) looks for the innermost `textfield`. One traversal, two questions, no predicate
threaded through the recursion.

Containment gates descent: a node whose rect does not hold the point is not entered, and neither are
its children. That is what makes the result a path rather than a set.

ponytail: nothing clips. `layout::scene` packs children inside their parent's content box, so a child
that overflows its parent is only reachable by declaring a size larger than the space given, and such
a child is unhittable in the overflowing region even though `layout::paint` still draws it. The fix
is a real clip rect carried down both walks together, not a looser rule in one of them. Painting
without clipping and hitting without clipping at least disagree in the safe direction: a click on the
overflow falls through to whatever is behind rather than firing the wrong callback.

Bounds are half-open, `x <= point.x < x + width`. Two buttons sharing an edge must not both claim it.

## Decision 2: a click is a press and a release on the same node

Firing on press is one line shorter and wrong. Every toolkit a user has ever touched lets them press
a button, notice the mistake, drag off it, and release harmlessly. Firing on press removes that.

So a press arms, and a release fires only if it lands on the same node. "Same node" is the pair of
the surface's instance id and the armed node's rect, because a `ResolvedNode` carries no identity a
later event could match against (`NodeId` lives on `RetainedNode` and does not survive
`to_resolved`). The rect is a proxy, and it is exact enough: a re-resolve between press and release
that moves the button cancels the click, which is the same answer a real identity would give for a
button that moved out from under the pointer.

Only `BTN_LEFT`. A right-click has no meaning in the IDL, and inventing one here would be policy the
config cannot override.

## Decision 3: `on_click` receives the button's rect

ADR-0040 and ADR-0049 both say the anchor rect comes from "the rect `on_click` returns", and § 6's
`popup` entry says it is "normally passed straight from the rect `button`'s `on_click` hands back."
Read literally as a return value that would mean the engine consumes what the callback gives back,
which it cannot: the engine does not know which `popup` a given click was meant to open, and the
button's own rect is something it already has.

The rect travels the other way. `on_click` is called with one argument, a table
`{ x, y, width, height }` holding the button's rect in its surface's logical coordinates, and the
config hands that on:

```lua
on_click = function(rect) menu_anchor:set(rect) end
```

with the `popup` declaring `anchor_rect = menu_anchor`. "Hands back" describes the round trip through
the config, not a Rust-side return. Phase 22 item 2 gets exactly what it needs and gets it in the
coordinate space its positioner wants.

A callback that raises is logged and swallowed. A broken `on_click` is a config bug, and a config bug
must not take down a shell that is otherwise painting; ADR-0046's rescue path exists for evaluation
failures, not for one misbehaving handler.

## Decision 4: focus attributes the secret, and no focus means no frame

`wayland/mod.rs`'s `PLACEHOLDER_SECURE_SUBMIT_CAPABILITY` and `PLACEHOLDER_SECURE_SUBMIT_ACTION`
both read `"unknown"`. They exist because a completed `wp-text-input-v3` submit had no way to name
the `textfield` it belonged to. Decision 1's path gives it one: a click whose path contains a
`textfield` focuses that node, and the engine remembers that field's own `secure_submit`
`{ capability, action }` (§ 5.2 item 8).

A submit arriving with no focused field sends nothing at all. The old behaviour addressed the secret
to `"unknown"/"unknown"`, which no Supervisor capability routes and which puts a password on the wire
for no one; ADR-0005's whole point is that this buffer travels to exactly one named destination. The
buffer is zeroized either way, on the same line it would have been read on.

Focus is cleared by a click that lands on no `textfield`, by `zwp_text_input_v3`'s `leave`, and by
the keyboard leaving the surface. Three sources, one rule: the field stops being focused the moment
anything says the user is somewhere else.

## Consequences

Pointer coordinates arrive surface-local and logical, which is the space `ResolvedNode::rect` is
already in, so hit-testing needs no conversion. That holds only while `paint_surface` passes scale
`1.0` and nothing calls `set_buffer_scale`; the HiDPI upgrade path already named in Phase 20 gains a
third caller here, and all of them have to move together.

Item 2's keyboard focus is tracked and nothing consumes it beyond clearing decision 4's focus. There
is no `on_key` in the IDL and this ADR does not invent one. `keyboard_interactivity` decides who the
compositor focuses; `wl_keyboard`'s `enter`/`leave` is how the engine finds out.

Click-outside-to-dismiss for a `panel` stays unsolved and this changes nothing about it. Layer
surfaces have no compositor-agnostic grab. A `popup` does not have the problem, which is the reason
to reach for one.
