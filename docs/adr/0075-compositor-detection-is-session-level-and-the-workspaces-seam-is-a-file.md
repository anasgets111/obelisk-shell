# Compositor detection is session-level, and `workspaces`' seam is a file

ADR-0034 built `CompositorLink` for `keyboard.active_layout` and put `CompositorKind` and the
`$HYPRLAND_INSTANCE_SIGNATURE`/`$NIRI_SOCKET` probe beside it, in
`supervisor/src/hardware/keyboard/layout.rs`. ADR-0056 then gave `workspaces` no trait at all,
reusing "the part that is verified and has no per-capability shape: `detect_compositor()` and
`CompositorKind`". Both calls were right. Together they left `workspaces/controller.rs` importing
from `crate::hardware::keyboard::layout`, and left `niri_ipc`'s types as the input of the one pure
function in the capability.

Nothing here reverses either ADR. No compositor is being added, and no trait is being extracted.
This is the preparation ADR-0056 named -- "extract the trait then, taking the two implementors'
actual shapes as the input instead of one implementor's shape plus a prediction" -- done while
there is still one implementor, because every piece of it gets *more* expensive once a second one
exists to be dragged along.

## Decision 1: the probe moves to `supervisor/src/compositor.rs`

Which compositor is running is a session-level fact. `hardware::keyboard` does not own it, and
`CONTEXT.md`'s **Compositor link** entry said as much already by scoping that module to "what
keyboard layout needs today". A second consumer arriving and reaching sideways into a sibling
capability is the signal that the thing is in the wrong place, not that the second consumer is
doing something unusual.

So `CompositorKind` and `detect_compositor` move to a new top-level `compositor` module, and
`hardware/keyboard/layout.rs` keeps `CompositorLink` and its two implementors. Detection there,
adaptors in the capability that needs them. A pure move: behaviour is unchanged.

The `CompositorLink` trait deliberately does **not** move with it. ADR-0056 settled that the trait
is keyboard-layout-shaped and does not grow; putting it next to the probe would suggest it is the
compositor abstraction, which is the thing this codebase has now twice decided not to have.

## Decision 2: the probe is a table, and an unsupported session gets named

`detect_compositor` was an `if`/`else` chain whose comment had to explain that Hyprland-first was
"an arbitrary but harmless tie-break". It is now a `PROBES` table of `(kind, env var)` in probe
order, so a third compositor is a line of data and the precedence is read rather than inferred. A
test asserts every `CompositorKind` has an entry and vice versa, with an exhaustive `match` that
fails the build when a variant is added without one -- a variant with no probe is a compositor
nothing can ever detect, which is a silent failure with no other guard.

`$XDG_CURRENT_DESKTOP` is deliberately **not** in that table. It is a name, written by whatever
launched the session, and it is still set for a session whose compositor never came up; every
`PROBES` entry is a var its compositor sets *because it is running*. Dispatching on the name would
be guessing.

It is good enough to say out loud, which is the actual gap being closed. `keyboard` used to print
"neither HYPRLAND_INSTANCE_SIGNATURE nor NIRI_SOCKET is set" and `workspaces` "no supported
compositor detected", and neither told a user on sway what Oblisk thought it was looking at. One
shared `unsupported_session_report()` now says "this session is sway, which has no implementor".
That is not a second detection path -- an unrecognised name still yields no implementor -- it is
the error path saying which session it gave up on.

## Decision 3: `derive_state` takes rows, not `niri_ipc` types

This is the decision with the cost, and it is the one ADR-0056 did not separate out.

"Do not write a trait for one implementor" is sound and stands. "Let `niri_ipc::Workspace` and
`niri_ipc::Window` be the input type of the reduction" came along with it and is a different call.
`derive_state` is the best code in the capability -- pure, ten tests, and it owns every judgement
ADR-0056 made: the output grouping and ordering, decision 3's workspace list, decision 4's optional
`focused_workspace`, decision 5's `app_id`-into-`class`. None of that is niri-specific *logic*. All
of it was niri-specific *types*, so a second compositor would have re-derived § 2.9's shape from
scratch and inherited none of those tests -- which is precisely the "plausible looking answer" risk
ADR-0056 wrote decision 1 to avoid.

`derive_state` now takes `&[WorkspaceRow]` and `Option<&FocusedWindow>`, declared in
`controller.rs`. `workspaces/niri.rs` is the only file in the capability that names `niri_ipc`: it
maps niri's two state parts onto those rows, and drives the loop.

The split follows what actually varies. *Which* window holds focus is the adaptor's question --
niri flags it per window, another compositor may answer it with a separate query -- so the search
lives in `niri.rs`, and only the winner is cloned per event. *What that window becomes in § 2.9* is
neutral, so it stays in `derive_state`. The same line puts "a workspace with no output is dropped"
in the reduction and "niri reports `output: null` with no monitors connected" in the adaptor.

`StatePublisher` is the write half of the same seam: reduce, drop an update that changes nothing,
store, wake `main.rs`. Eight lines that every adaptor would otherwise copy, and the eight where two
copies silently disagreeing about debouncing or about the mutex/channel contract would be a real
bug rather than a cosmetic one.

## Decision 4: the seam is a module boundary, and stays one until a second implementor is live

ADR-0056 decision 1's reasoning is untouched: a trait with one implementor is Speculative
Generality, and Hyprland's per-monitor-active versus niri's global-focus models still do not map by
renaming fields, and writing that mapping without a machine to run it on is still how a plausible
answer ships. `WorkspacesController` still matches a `CompositorKind` rather than holding a
`Box<dyn>`.

What changed is that the line the trait would sit on is now a file boundary instead of nothing. A
second compositor is a sibling module plus two arms, and it inherits the reduction, the publish
contract and their tests rather than re-deriving them. Both of those arms are exhaustive `match`es
on `CompositorKind`, so adding a variant fails the build at exactly the two places that need an
answer.

The tests moved with their subject, which is the clearest evidence the boundary is in the right
place. `derive_state`'s ten shaping tests build rows directly and name no compositor.
`niri.rs`'s tests keep ADR-0056's wire-JSON fixtures, deserialized from a live `niri msg -j` rather
than written as struct literals so a niri field rename breaks them -- and that guarantee now sits
with the adaptor, which is the only thing a niri upgrade can break.

## What this does not decide

Whether the eventual trait is one trait or two. ADR-0056 argued `CompositorLink` should not grow
workspace methods, and nothing here tests that either way; it is still a question for the commit
that adds a second live-tested compositor, which is still the only thing that should answer it.
