# A rounded clip is an offscreen pass, not a rounded scissor

Amends build-steps.md Phase 19 item 17, which shipped a rectangular clip and left a `ponytail:`
comment in `layout::paint::build_node` naming femtovg's `intersect_rounded_scissor` as the upgrade
path. That upgrade path does not work. This ADR records why, and what replaced it.

The symptom it was left for is in `dev-config`'s battery indicator: a pill with a ground that fills
left to right with the charge. The fill is a child of the pill, so the pill's clip is what should cut
it, and a rectangular clip cuts it square. The config worked around that by giving the fill the
pill's own radius, which rounds its right edge too and draws a lozenge sitting inside the left cap
instead of a filled arc.

## Decision 1: `clip = "Rounded"` renders its subtree aside and composites it through the node's path

`layout::paint::build` emits a `Draw::Clipped` group holding the node's children. `execute` allocates
an offscreen image the size of that node's clip rectangle, draws the group into it, and then fills
the node's own rounded path with that image as the paint. The path is the mask, so the clip's edge is
an antialiased path edge and nothing else has to agree with it.

`Draw::Clipped` is the one recursive variant in the display list. Every other clip in the module is
an axis-aligned rectangle, which is what lets `DrawCmd` carry a single flattened `clip` per command
instead of a `save`/`intersect_scissor`/`restore` nest (docs/adr/0063 relies on that list being
comparable, and it still is: the variant derives `PartialEq` like the rest). A rounded shape does not
intersect into a rectangle, so the subtree it applies to has to stay grouped.

Three commands come out where the rectangular case emits one: the node's fill, the group, then the
node's border. QML's `ClippingRectangle` paints its border over the content it clips, and for the
same reason -- a ground reaching the arc otherwise covers the border exactly along the arc, which on
a pill is the half of the outline most worth seeing.

## Decision 2: femtovg's `intersect_rounded_scissor` is not the alternative it looks like

femtovg 0.26 keeps one scissor in its canvas state and it is a single rounded rectangle, so "this
rectangle and that arc" has nowhere to live. Asked to intersect a rectangle into an existing rounded
clip it takes one of three branches (`src/lib.rs`): keep the rounded clip when the requested
rectangle contains it, drop the radius when the rounded shape contains all four requested corners, or
re-round the intersection with the old radius. A part-width child of a pill takes the third, which
rounds the child's own box.

Measured on the battery's exact shape, an 80x32 pill at radius 16 over a red ground with a child
filling its left 30px, sampling where the pill's top edge is straight:

| route | outside the arc | at the waist | over the straight top edge |
| ----- | --------------- | ------------ | -------------------------- |
| `rounded_scissor` then `intersect_scissor` | ground | fill | ground bleeds through at 8% |
| offscreen and composite | ground | fill | fill |

The scissor route re-rounds the 30px child to a lozenge, which is the artefact the config was already
working around. It would have moved the bug, not fixed it.

The rounded scissor is still what femtovg's fragment shader applies for a *single* clip, and this
crate does not use it anywhere: `execute` sets each command's finished rectangle with `scissor`.

## Decision 3: `radius` does not imply a rounded clip; a config asks for one

`clip` takes `"Box"` (the default) or `"Rounded"`. `"Box"` is what every node has always done: its
own rectangle, square corners, whatever its `radius` says.

A rounded clip costs an offscreen render target plus a composite per clipping node per repaint, where
a square one is a scissor rectangle the GPU applies for free. Most rounded boxes on this bar have no
child that overflows them, and charging every one of them for a pass none of them needs is the wrong
default. QML draws the same line: `Item.clip` is rectangular and ignores `radius`, and reaching the
rounded shape means reaching for Quickshell's `ClippingRectangle`, which spends two offscreen targets
because its mask has to be a texture for a fragment shader to sample. femtovg fills a path with an
image paint directly, so the path is the mask and one target does it.

A childless node that asks for a rounded clip buys no pass at all: there is nothing to clip, and the
empty group would still cost a render target and a composite.

## What this does not do

`layout::hit` intersects the same rectangles and knows nothing about the arc, so the corner of a pill
is outside its fill and still takes a click. Four pixels on a 34px control. The honest fix is hit
testing sharing paint's walk rather than a second copy of the rounding rule, and nothing has asked.

The offscreen image is allocated and freed per clipping node per repaint, after the flush that
consumed it -- femtovg records draw calls and executes them at `flush`, so an image freed any earlier
is freed out from under a queued draw. A pool keyed by size next to `ImageCache` is the upgrade path,
worth writing when a config puts a rounded clip on something that repaints at pointer rate. Today the
bar repaints when a signal changes (docs/adr/0063).
