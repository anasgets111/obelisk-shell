# Pointer hit-testing walks a path, a click is press-and-release on one node, and focus attributes the secret

> **Amended when built (Phase 21 item 1, commit `c317aa2`).** Two statements below were written
> against code that had already moved. Both are corrected in the amendment section at the end; the
> decisions themselves stand. Read decision 1's ponytail and the consequences section's first
> paragraph together with that section.

> **Amended again: decision 2's `BTN_LEFT`-only rule is gone.** `on_click` now fires for left, right
> and middle, and takes the button's name as a second argument. See the last amendment section.
> Decisions 1 and 3 are untouched. Decision 4's *rule* is untouched and its *trigger* is not: "the
> press decides focus" now runs on three buttons rather than one, which the amendment's "Two
> knock-on effects" section records. The press-arms/release-fires rule that is the rest of decision 2
> also stands.

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

## Amendment: two things above were already false when written

### `ResolvedNode::rect` is parent-relative

The consequences section says pointer coordinates "arrive surface-local and logical, which is the
space `ResolvedNode::rect` is already in, so hit-testing needs no conversion." Half right.
`layout::paint::paint_node` carries a running `origin_x + node.rect.x` down the tree, so a node's
rect is relative to its parent and only the root's is surface-local.

The point-to-node comparison genuinely needs no conversion, as long as the walk accumulates the same
running origin paint does. What does not exist for free is the *absolute* rect: decision 3 hands
`on_click` the button's rect, and Phase 22's positioner wants it in surface coordinates, and that
number is only recoverable as the sum along the path that reached the node. `layout::hit` exports
`absolute_rect` for exactly that, which is a second reason the return type is the whole chain rather
than a node and a depth.

### Paint does clip, so hitting and painting agree exactly

Decision 1's ponytail says "painting without clipping and hitting without clipping at least disagree
in the safe direction." That describes code deleted before this ADR was written: Phase 19 item 17
added an `intersect_scissor` chain to `paint_node`.

The real outcome is better than the ponytail settles for. Containment-gated descent makes a node's
hittable region exactly its intersection with every ancestor's rect, and the scissor chain makes its
painted region exactly the same intersection. The two walks agree exactly, without either one
carrying a clip rect, because they are computing the same intersection by different means. The
overflow case the ponytail worried about is not a gap to close.

What survives of that ponytail is smaller and still true: neither walk carries an explicit clip
rect, so anything that gives one of them a scroll offset or a transform has to give the other the
same one in the same commit.

### Decision 4's premise was missing a piece

Decision 4 and ADR-0049 both assume `on_click` writes named state and the write marks the scene
dirty. `state(name, initial)` (ADR-0044 decision 5) was not built, so nothing a handler could do
marked anything. Phase 21 item 1 shipped an unconditional dirty mark after every handler as a
stopgap and named it as one; the following commit built `state` and deleted it. Phase 22 depends on
the same mechanism, which is why it was worth fixing rather than documenting.

## Amendment: `BTN_LEFT` alone was too narrow, and the fix is one argument

Decision 2's last paragraph says `BTN_LEFT` only, because "a right-click has no meaning in the IDL,
and inventing one here would be policy the config cannot override." The premise was right and the
conclusion was backwards. Handing the config the button is what stops the engine having a policy.
Refusing to hand it over is the policy.

`build-steps.md` section 6 measured the cost against a Quickshell config that already ships. Six of
its nineteen bar modules put a different action on the right button: the tray opens an item's menu,
the power menu cancels its countdown, the update checker opens its panel, the idle inhibitor opens
its settings, the wallpaper button randomizes every monitor, and the screen recorder opens options.
None of them is expressible here, and none of them is exotic.

### `on_click` takes a second argument, and it is a string

```lua
on_click = function(rect, button)
    if button == "right" then menu_open:set(true) else oblisk.tray:invoke("activate", id) end
end
```

A second argument rather than a fifth field on the rect table. Lua drops arguments a function does
not declare, so every handler written against decision 3's one-argument form keeps working with no
edit, and the table a config forwards to a `popup`'s `anchor_rect` stays four fields wide instead of
carrying a `button` into the positioner.

A string, not the raw evdev `273` and not a normalized `1`/`2`/`3`. Every categorical value that
crosses this boundary is already a lowercase string matched against named literals: `fit`, `layer`,
`anchor`, `align_h`. `if button == "right"` needs no lookup table in the author's head; `if button ==
273` needs the evdev header, and `if button == 3` needs to know which of the two disagreeing
small-integer conventions is in play (the DOM's own `MouseEvent.button` and `MouseEvent.buttons`
number the same three buttons differently). Qt's `MouseArea`, which is the prior art a Quickshell
author already has muscle memory for, normalizes to a name for the same reason. The direction is also
the cheap one to be wrong about: adding the raw code later is one more argument, while shipping codes
now and normalizing later rewrites every `if` in every config.

### Three buttons, and an unhandled code still does nothing

`left`, `right`, `middle`. A press carrying any other code arms nothing and a release carrying one
fires nothing, which is exactly what decision 2's original match did for the seven other codes
`smithay_client_toolkit::seat::pointer` names.

Not `"other"` for the rest, which is the tempting alternative. The set that fires has to equal the
set a config can name. A config handed `"other"` for `BTN_TASK` cannot tell it from `BTN_EXTRA`, so
it cannot write a correct handler for either, and the practical effect is that a side button runs
whatever handler was written for the left one. That is decision 2's "policy the config cannot
override" with a different button in it.

Back and forward are the two a mouse plausibly has next, and they are left out because a correct
mapping is not obvious and there is no caller to check it against. Real mice emit `BTN_SIDE` (0x113)
and `BTN_EXTRA` (0x114) for back and forward, while `BTN_BACK` (0x116) and `BTN_FORWARD` (0x115)
carry the literal names and are rarer, so the mapping is four codes onto two names. Add them with the
first config that asks, and `pointer_button_name` is the only place that changes.

### The press has to remember which button armed it

`ArmedClick` carries the evdev code alongside the surface id and the rect, and a release completes
the click only if all three match. A mouse holds more than one button at a time, so pressing right,
then pressing left, then releasing left is a real sequence, and without the third comparison it
completes the right-button press with a left-button release.

### Two knock-on effects, both wanted

A right or middle press now decides focus the way a left press does, because decision 4's rule is
"the press decides focus" and these are presses. Right-clicking away from a `textfield` clears it,
where before that event did nothing. Every toolkit answers the same way, and the alternative is a
focus rule that depends on which button you used.

A right or middle press also arms the `xdg_popup.grab` serial and counts toward
`pointer_input_count` (docs/adr/0049's amendment, docs/adr/0051's first). Both exist to answer "did
real user input cause this", and a right-click is real user input. Without it, a `popup` opened from
a right-click handler would be refused for having no serial, which is the whole feature.

### What this changes for a config that ignores the argument

A handler that takes no `button` argument now runs on a right or middle click as well as a left one,
where before those events did nothing. That is a real behavior change and there is no way to have the
feature without it, short of a second property per button, which does not scale past three and
matches no toolkit. A config that wants the old behavior asks for it: `if button ~= "left" then
return end`. `dev-config/oblisk/shell.lua`'s `lock_button` does exactly that, because locking the
session on a stray right-click is the one handler in the tree where the difference is not cosmetic.
