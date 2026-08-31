# Hover is a signal the engine writes, not a callback it calls

`build-steps.md`'s ranking puts `on_hover` fourth of nine, behind a one-line reason: "Two tooltips,
a hover-to-open panel, a hover highlight, and every expand-on-hover affordance. Needs an ADR on
whether the engine may emit a signal rather than only consume one." This is that ADR.

Nothing about reading the event is hard. `wl_pointer`'s `Enter`, `Motion` and `Leave` already
arrive at `renderer/src/wayland/input.rs`'s frame handler and fall into its `_ => {}` arm, and
`layout::hit::hit_path` already turns a position into the chain of nodes under it. What has to be
decided is what a hover *is* in a tree that re-resolves on every capability push, and who owns the
state.

## Decision 1: a signal, not a callback

The obvious shape is `on_click`'s: `on_hover = function(entered) ... end`, a property holding an
`mlua::Function` the engine calls on each edge.

It is the wrong shape here, and the reason is that a click is an event while a hover is a
*condition*. A tooltip is not "do something when the pointer arrives"; it is "be visible while the
pointer is here". Handed a callback, every config that wants a tooltip writes the same state
machine: a `state()` signal, a handler setting it true, a handler setting it false, and a bug where
the false never arrives because the node was replaced by a re-resolve between the two edges. That
last part is not hypothetical -- `active_window.lua` already carries a request counter for exactly
this class of problem on the `process.run` path, and `lib/ui_state.lua`'s `open_panel` carries a
paragraph about an ordering it cannot control.

So the engine holds the condition and the config reads it:

```lua
local hovered = hover("volume")
row { hover = hovered, children = { ... } }
popup { visible = hovered, grab = false, ... }
```

A tooltip is a declaration, not a protocol. There is no edge for a config to miss, no order for two
writers to get wrong, and the property that is bound is `visible`, which already takes a signal.

The cost is that a config wanting to *act* on the moment of entry -- fire a request, start a timer
-- cannot, because there is no callback to hang it off. Nothing in the reference shell wants that:
of the four hover affordances measured in `build-steps.md`, two are tooltips, one is a highlight,
and one opens a panel, and all four are conditions. If an action on the edge is ever wanted,
`on_hover` can be added beside this without changing it, the way `on_click` and `visible` already
coexist.

## Decision 2: the engine writes it, and that makes the Renderer a source of signals

Every signal in this process before now was written from outside the Renderer or from inside the
config: a capability's `Live` signal is fed by a `StateSnapshot` off the control socket (ADR-0029),
and a `state(name, initial)` signal is fed by Lua (ADR-0044 decision 5). Hover is neither. The
Renderer itself computes it, from an input event, and pushes it into a signal the config only
reads.

That is the part worth an ADR rather than a spec row, because it sets a direction: the engine may
own reactive state and publish it. Accepting it here means the next thing of this shape -- a
`focused` signal, a `pressed` signal, a window's `maximized` -- is a spec row and not another
argument.

The alternative was to keep the Renderer purely a consumer and route hover out to the Supervisor
and back as a capability, which is what a strict reading of the process split would ask for. It is
absurd on its face: a round trip over a Unix socket, through a JSON payload, to answer a question
the Renderer resolved from a rect it already had, at pointer-motion rates. The process boundary
exists to keep the Supervisor's long-lived connections out of a process that gets reaped
(`docs/adr/0020`), and hover has no connection, no lifetime, and no consumer outside the generation
that drew the rect.

A hover slot is two signals, not one. `hover(name)` is the boolean, and `hover_rect(name)` is the
node's absolute rect in its surface's coordinates -- the same space `on_click` hands a config
(ADR-0050 decision 3). The boolean alone would have made an expand-on-hover work and left a tooltip
unable to say *where* it goes, since § 6.3 gives a `popup` no way to position itself against a node.
Two names rather than one signal holding a record, because the two are bound to different properties
(`visible` and `anchor_rect`) and a config wanting only the first should not have to unpack the
second.

The rect keeps its last value when the pointer leaves rather than clearing. § 6.3 refuses a
zero-sized `anchor_rect`, and the popup is still resolving on the turn it closes; a cleared rect
would fail that evaluation on the way out.

Identity is the name, exactly as `state(name, initial)` does it (ADR-0044 decision 5): one
generation keeps one `name -> Signal` map that outlives any single evaluation, so an in-place
reload finds the signal it built last time still holding its value, and a tooltip open across a
`config/theme.lua` edit stays open. The name is also what two files use to reach one hover slot,
the way `lib/ui_state.lua` already shares `state` signals by name.

`hover(name)` returns its own signal kind rather than reusing the capability kind. `signal:set()`
already refuses everything but a `state` signal by name (ADR-0044 decision 5), so a config cannot
write a hover slot, and the refusal says "a hover signal" instead of misreporting it as a
capability. The variant earns its place on the other side too: the engine's writer accepts a hover
signal and nothing else, so a config writing `hover = oblisk.network` cannot get the pointer to
overwrite a capability's snapshot.

## Decision 3: the `hover` property carries the handle, so it does not resolve

`layout::node::resolve_properties` replaces every `Signal` in a property map with its current value
(ADR-0044 decision 1). A `hover` property resolved that way would arrive at the pointer handler as
the boolean `false`, which says nothing about *which* signal to write.

`hover` is therefore a structural property: `is_structural_property` copies it through raw, so the
handle itself reaches `ResolvedNode::properties` and the pointer handler can recover it with
`signal::from_userdata`. This is the mechanism `id` and a panel's `layer`/`anchor`/`monitor`/
`namespace` already use, for the same reason -- a property that names a thing rather than carrying
a value has nothing to resolve.

One consequence worth stating: the resolved property map is documented as a complete snapshot of
one pass, and a raw handle in it is not a value. It was already not complete in that sense, for the
five properties above. The rule is the one that was already true: structural properties are
identities, and identities do not resolve.

## Decision 4: one write per boundary crossed, not one per motion event

`wl_pointer` reports motion at device rate. Every `LiveSignalHandle::set` marks the one scene-dirty
flag (ADR-0044 decision 2), and any mark re-resolves every surface in the generation. Writing the
hover boolean on each motion event would re-resolve the whole scene tens of times a second for a
pointer sitting still inside one button, on the Wayland dispatch thread that also runs the config
VM (ADR-0039).

So the write compares first: a `set` storing the value already there does nothing and marks
nothing. Moving across a button re-resolves twice, once for the node being left and once for the
node being entered, and moving *within* it re-resolves not at all.

The hit test still runs per motion event. That is a walk of the surface's resolved tree, bounded by
`MAX_TREE_DEPTH`, against a pointer position -- the same walk a click already does, and cheap
next to the resolve it is protecting.

## Decision 5: hovered means on the hit path, so ancestors are hovered too

`layout::hit::hit_path` returns the whole chain of nodes containing the point, root-first, entering
only visible nodes whose rect holds the point and taking the topmost child at each level
(ADR-0050 decision 1). Every node on that chain is hovered; every other node in the surface is not.

Ancestors being hovered is what makes the common case work. A `pill` is a `row` wrapping a
`button` wrapping a `text`, and the thing a config wants to know is whether the pointer is over the
pill, which is the ancestor. Restricting hover to the innermost node would make `hover` useless on
every module in `dev-config` without also restructuring them.

The topmost-child rule carries over unchanged: two siblings overlapping means the one painted last
is hovered and the one beneath it is not, which is what the click path already decides and what
the paint order already shows.

## What this does not build

- **No tooltip node.** § 6.3's `popup` with `grab = false` is the tooltip surface, and it already
  works. A `tooltip` role that positions itself would be a second positioner and a second lifetime
  rule for no new capability.
- **No `on_scroll`.** `Axis` still falls into the same `_ => {}` arm. It is ranked next and is a
  different problem: the open question there is what a scrollable container *is*, not what the
  callback looks like.
- **No animation.** An expand-on-hover snaps. `build-steps.md` ranks the animation model behind
  this one for exactly this reason: an expanding pill needs the hover before it needs the easing.
- **No hover for the pointer's own shape.** `wl_pointer.set_cursor` is untouched, so a hovered
  button does not become a hand.
- **No keyboard equivalent.** A hover signal reports a pointer, and a shell driven entirely from
  the keyboard reads false everywhere. `focused` is the signal that answers that question and it is
  not this one.
