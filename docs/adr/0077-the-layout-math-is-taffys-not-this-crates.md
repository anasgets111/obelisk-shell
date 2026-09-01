# The layout math is taffy's, not this crate's

Supersedes docs/adr/0023. That ADR's title is "Layout engine ships a stacking model, not a full
constraint solver", and this reverses the second half of it. Its items 1 to 9 and 12 are about what
Phase 12 did not build and are untouched; what changes is item 4's arrangement formula, item 10's
one-pass budget, item 11's un-repositioned descendants, and the decision section's "one recursive
function doing all three of § 3's passes".

The reason to reopen rather than amend: item 11 is a defect the ADR names, names the fix for ("a
second constraint pass", upgrade path (g)), and no phase claimed in the 53 ADRs since. A solver is
that second pass. Item 10 shares the cause and does not get fixed, for a reason given below.

`docs/oblisk-layout-engine-geometry.md` § 3 opens by rejecting "the expensive multi-pass layout
trees and cyclic binding graphs used in QtQuick/QML". That objection does not reach taffy, which is
a bounded flexbox and grid solver with a per-node cache and no binding graph at all. The "single
pass" wording it shares a paragraph with is what this supersedes.

## Decision 1: `taffy` 0.14 owns sizing and positioning; `layout::scene` owns everything else

`renderer/src/layout/scene.rs` keeps node identity and the reconcile rules (docs/adr/0045), the
lease and its child-first teardown, the depth cap, the once-per-node property resolution and parse
(build-steps.md Phase 19 item 5), the scroll clamp and writeback (docs/adr/0069), and text elision.
It no longer computes geometry. A pass is now three steps:

- `prepare` walks the fresh tree in declaration order, resolving and parsing each node once,
  matching it to the retained node it continues, retiring what it replaces, and building one taffy
  node per node;
- `solve` runs taffy over that tree;
- `finish` reads the geometry back, applies the scroll offset, and elides over-wide text.

`taffy_style` is the seam, and the only place that knows what a `row` or a `Fill` means. Everything
else in the module treats geometry as something that arrives.

**The mapping.** `row` and `column` are `Display::Flex` with a direction, and a `list` takes
whichever its own `direction` names. The stacking model (ADR-0023 item 4) is `Display::Grid` with
every child pinned to row 1, column 1: one auto-sized cell, children overlapping, each aligned in
the full content box on both axes independently, and the container's `Content` size their bounding
union. That is the model item 4 describes, expressed rather than reimplemented. `Fill` on the axis
the parent flows along is `flex_grow: 1.0` over a zero basis; `Fill` anywhere else is a stretch.
`spacing` is `gap`, `padding` is `padding`, `margin` is `margin`, and `visible = false` is
`Display::None`.

Two taffy defaults are turned off, because this engine has no shrink concept. `flex_shrink` is set
to zero so a fixed child keeps the width it asked for when its siblings already overflow, and
`min_size` is set to zero so taffy's automatic minimum size does not floor a `Fill` child at its own
content instead of letting it collapse. Rounding is disabled: this engine's geometry is fractional,
and `text::snap` is what turns it into pixels at paint time, per surface scale.

**Cost.** One crate. `arrayvec`, `slotmap` and `smallvec` were already in the tree, so the
Renderer's dependency count goes 151 to 152. Default features are off; `flexbox`, `grid`,
`taffy_tree` and `std` are on, and the block and float layout engines, the calc parser and
`content_size` are not compiled. `content_size` was on at first, for taffy's
`scrollable_overflow_rect`, and came off when the scroll extent turned out to need summing here
anyway: CSS scrollable overflow excludes the children's margins and this engine's footprint
includes them, which the amendment on docs/adr/0069 records. `scene.rs` loses 78 lines of production code, and the change is a net
deletion across the workspace.

**The measure callback** is the one geometry question this crate still answers, and only for the two
kinds whose size is their content: a `text`'s shaped extent and an `icon`'s square. taffy asks each
several times per pass rather than once, over a small set of distinct `(text, size, wrap width)`
tuples, and `ShapingHandle`'s memo is keyed on exactly that tuple, so every repeat after the first is
answered without crossing the channel. `icon`'s `size` and `text`'s `content`/`font_size` are parsed
in `prepare`, not in the callback, because a callback returns a `Size<f32>` and has nowhere to put a
`LayoutError`.

## Decision 2: item 11 is fixed, item 10 is not, and the difference is not effort

Item 11 said a `Stretch` child of a `Content`-sized parent does not reposition its own descendants:
the parent's size is not known until after the child resolves, so the hand-written pass patched the
child's `rect` afterwards and left everything below it positioned against the pre-stretch size. The
solver relayouts the stretched item. The test is
`a_stretch_child_of_a_content_sized_row_repositions_its_descendants_too`, which is exactly the shape
ADR-0023 item 11 said no fixture exercised.

Item 10 said a `Fill` or `Percent` child of a `Content`-sized row resolves to zero. It still does,
and that is the same answer CSS gives: a percentage against an indefinite container is indefinite,
and a flex item with a zero basis growing into a container that has no free space gets none.
`a_fill_child_of_a_content_sized_row_still_resolves_to_zero` stays green unchanged. No win, no
regression, and the reason is now a specification rather than a `ponytail:`.

## Decision 3: two behaviours change, both deliberately, and one that could have does not

**An invisible node leaves the layout entirely.** `Display::None` gives a hidden subtree no size and
no position, where the hand-written pass resolved its geometry in full and then declined to place
it. Nothing outside `layout::scene` can tell: `layout::paint`, `layout::hit` and
`overlay_input_regions` all filter on `visible` before reading a rect, and the two readers that do
look at a hidden node -- `layout::hover`, and `wayland::surface`'s `panel_spec` re-derive -- read
`properties`, which is still carried in full. What ADR-0023 actually specified is the part that
holds: a hidden child reserves no space and no `spacing` gap.
`an_invisible_subtree_resolves_to_no_geometry_at_all` pins it so it stays a decision.

**Property getters now fire in plain declaration order.** The hand-written pass recursed into a
`Fill` child last, so it could be sized from what its siblings left, and had to lift property
resolution out of the recursion to keep siblings in source order; the grandchildren ended up
interleaved the other way, which the old test spelled `abBA`. There are no rounds now, so one walk
goes depth-first in declaration order and the same fixture reads `aAbB`. The guarantee
build-steps.md Phase 19 item 5 states -- every getter exactly once, in the order the config wrote it
-- is unchanged and is now the obvious reading. The test was rewritten rather than worked around,
and renamed to the guarantee instead of the artefact.

**A `Stretch` alignment still outranks an explicit size on the same axis.** CSS applies
`align-self: stretch` only to an `auto` cross size, so `height = 5, align_v = "Stretch"` would keep
its 5. This engine has always let the stretch win, because the hand-written pass overrode the
resolved size outright, and `row_child_stretch_alignment_fills_the_cross_axis` pins it. The mapping
reproduces that by blanking the size on any axis whose applied alignment is a stretch. Kept rather
than corrected: swapping the solver is the change being made here, and whether a stated size should
outrank a stated stretch is a config-facing question that should not be answered as a side effect of
a dependency swap.

## Decision 4: the depth cap stays at 64, on a better measurement

`MAX_TREE_DEPTH`'s comment used to model the worst-case stack from instrumented frame addresses,
counting only this module's own recursion, and admitted it excluded the mlua frames below the
deepest node. The model was optimistic. Measured by shrinking a thread's stack until the process
aborts, which counts everything, the shipped hand-written pass needed about 1,400 KiB for the
compound worst case (a tree at the cap with a 31-deep `computed` chain on every level's `margin`),
a 1.44x margin on the 2 MiB debug test thread rather than the 2.6x the comment claimed.

The solver lowers it. The same worst case now peaks at about 1,040 KiB, a 1.97x margin, and a
64-level tree with no signals in it costs about 590 KiB, near enough 8,960 B per level. Part of that
is deliberate: a `taffy::Style` is 552 bytes, so `new_solver_node` builds and consumes one in a frame
that returns before the recursion descends, rather than in `prepare`'s, which is multiplied by the
cap. So the cap stays at 64 and is on firmer ground than when it was written.

## What this does not decide

Whether the retained tree should hold its taffy nodes across passes. One `TaffyTree` is built and
dropped inside each `apply_one_instance` call, which is why `apply_admitting`'s rollback has nothing
extra to undo: a failed walk drops the tree on the way out. Reusing one and marking dirty nodes is
what taffy's per-node cache is for, and it is the obvious next move if a pass ever measures too
slow. It is not built now because nothing has measured it, and because a tree that outlives a pass
has to be reconciled against the retained tree, which is a second identity problem on top of the one
docs/adr/0045 already solved.
