# A scroll offset is engine state the layout pass clamps

`build-steps.md`'s ranking puts `on_scroll` fifth of nine and says the open question is "what a
scrollable container means, not what the callback looks like". This is that ADR.

Reading the event is not the hard part. `wl_pointer`'s `Axis` arrives at
`renderer/src/wayland/input.rs`'s frame handler and is now the only kind still falling into its
`_ => {}` arm, and clipping has been built since Phase 19 item 17: `layout::paint` intersects every
node's box with its ancestors', so a subtree is already cut to its parent. What has to be decided is
where the offset lives, who is allowed to clamp it, and what § 5.2 gains, since it has no container
that owns a viewport.

Nine of the reference config's surfaces need this, including eight of its thirteen bar panels. Two
of those already exist on the Lua side as 32-line stubs, and they are stubs because of this.

## Decision 1: the offset is in the scene, not beside the surface

The cheap shape is to keep the offset on `wayland::App` next to the surface it belongs to, apply it
when painting, and apply it again when hit-testing. A wheel event then repaints without touching
Lua, which at first looks like the only affordable option.

It is the wrong shape, and the reason is that it puts geometry in two places. Hit testing, hover,
the clip stack and the display list all read `RetainedNode::rect` today. An offset the scene does not
know about has to be re-applied, correctly and in the same direction, by each of them, and the first
one that forgets is a row that paints where you can see it and clicks where you cannot. ADR-0067 has
just finished separating what the Wayland client addresses by position from what the scene addresses
by identity; this would reintroduce exactly that split one layer down.

So the offset is applied in `position_children`, and everything that already reads `rect` follows for
free.

The cost is real and was measured rather than assumed, because it is the thing that would make this
decision wrong. A wheel event marks the scene dirty, which re-resolves and re-lays out. Release
build, p50 over 100 applies of one `list` in a 400x600 panel:

| rows | before the shape cache | after |
| :--- | :--- | :--- |
| 200 | 4.86ms | 2.19ms |
| 500 | 12.78ms | 6.14ms |

At 12.78ms a 500-row list would have dropped frames on every wheel event, and this decision would
have been indefensible. Timing it is what found that text shaping was most of the pass and that
`layout::scene` had already named the cache that fixes it, so that landed first. What is left is
2.19ms at 200 rows, comfortably inside a 120Hz frame, and 6.14ms at 500, inside 60Hz. This machine
has 61 desktop entries, so the shipped launcher is the 50-row case at about 1.2ms.

The upgrade path, if a list ever outgrows that, is a narrower dirty flag or a virtualized `list` that
builds only the rows near the viewport. Both are additive and neither needs this decision reversed.

## Decision 2: the engine owns the value, the config reads it

The alternative is `on_scroll = function(delta) ... end` with the config keeping the offset in its
own `state()` signal, which is what a callback-shaped API would give.

It cannot be made correct in config code. An offset has to be clamped to the content extent minus the
viewport extent, and a config knows neither number: the content extent exists only after the pass
resolves and measures the children, and the viewport extent is whatever the surface was configured
to. Handed a raw delta, every list in every config scrolls past its own end and keeps going.

So the shape follows ADR-0062's, which settled that the engine may emit a signal:

```lua
local list_scroll = scroll("network_aps")
column { scroll = list_scroll, height = 400, children = { ... } }
```

`scroll(name)` is name-keyed like `hover(name)` and `state(name, initial)`, so an in-place reload
finds the offset the user left and a panel does not jump to the top when the config is edited.

`on_scroll` is deliberately not built. Nothing in the reference config wants the wheel *event*; the
two places that read `onWheel` want a value to change, which this gives them.

## Decision 3: a property, not a node kind

A `scroll` node kind would duplicate `column`'s entire layout arm to add one field, and `list` needs
a viewport as much as `column` does. `scroll = <signal>` is a property on the containers that already
flow, exactly as `hover` is.

## Decision 4: the layout pass clamps, and writes back what it used

`position_children`'s row and column arms already compute `total_main`, the content extent counting
visible children and the spacing between them, and already hold `content_width`/`content_height`, the
viewport. The clamp bound is `(total_main - content_main).max(0.0)` at the point where `spare` is
computed today, so no new parameter is threaded anywhere.

The clamped value is written back to the signal rather than merely used. A config that reads
`scroll("x")` after the pass sees where the list actually is, not what the last wheel event asked
for, so a scrollbar indicator built on it cannot disagree with the rows.

## Decision 5: a container with no stated extent on the scroll axis does not scroll

A `Content`-sized column grows to fit its children, so its content extent and its viewport are the
same number and the clamp bound is zero. This is a no-op rather than an error, which is the same
answer `Fill` gives in a `Content` parent (ADR-0023 item 10, and the `ponytail:` on
`resolve_and_reconcile`'s `child_budget`) and for the same reason: there is no remainder.

## Decision 6: pixels when the compositor sends them, a step when it does not

`AxisScroll` carries `absolute` in logical pixels, `value120` where 120 is one logical step, and a
deprecated `discrete`. A touchpad sends `absolute`; a notched wheel sends only a step count and needs
one chosen for it.

`absolute` when non-zero, otherwise `value120 / 120.0` steps of three lines. `discrete` is ignored
entirely: it is deprecated, and the compositors that send it send `value120` too.

## Consequences

§ 5.2 gains its first container that owns a viewport, which is a spec change and the larger half of
this work.

`Axis` stops being dropped, and `input.rs`'s `_ => {}` arm is deleted: with the wheel handled, the
match over `PointerEventKind` is exhaustive and the compiler says so. That arm had carried a comment
since the pointer was written explaining which events it swallowed. There are none left.

A surface whose scroll offset changed produces a different display list and repaints; every other
surface compares equal and does not (ADR-0063). Scrolling one panel does not repaint the bar.

Nothing here gives a config a scrollbar. Drawing one needs the content extent, which this ADR does
not expose, and the first config that wants one is the place to decide whether it should be.
