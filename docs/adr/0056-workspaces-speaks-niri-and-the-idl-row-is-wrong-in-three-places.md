# `workspaces` speaks niri, and § 2.9 is wrong in three places

ADR-0053 decision 1 held `workspaces` back from the batch that shipped `battery`, `system` and
audio's master volume, on the grounds that "deciding whether the capability speaks one compositor's
IPC or an abstraction over several is a design question, not an implementation one".
`docs/build-steps.md` Phase 28 item 3 carried the same warning forward and added one of its own: "Do
not let a bar's need for a workspace strip decide it."

This settles that question, and three others that only appear once you hold § 2.9 next to what a
compositor actually reports.

## Decision 1: one compositor, no trait, and the trait that already exists does not grow

ADR-0034 built `CompositorLink` for `keyboard.active_layout` with two implementors on day one
(Hyprland and Niri), and scoped it deliberately: "not widened to guess a future workspace adaptor's
eventual method surface". `CONTEXT.md`'s **Compositor link** entry ends "Whether that trait extends
this one or defines its own is an open question, not settled here." It is settled here, and the
answer is neither.

`workspaces` has one implementor: niri. There is no trait, because a trait with one implementor is
Speculative Generality by this repo's own review checklist, and ADR-0034 already made exactly that
call one level down ("Forcing a trait before any caller needs `Vec<Box<dyn Adapter>>` polymorphism
is Speculative Generality"). What is shared with `keyboard` is the part that is verified and has no
per-capability shape: `detect_compositor()` and `CompositorKind`, the `$HYPRLAND_INSTANCE_SIGNATURE`
/ `$NIRI_SOCKET` probe. Those are reused directly. Nothing else is.

Extending `CompositorLink` was the other option and is worse for a specific reason. Its Hyprland
implementor is, by ADR-0034's own admission, "built to its documented real IPC protocol but **not**
independently live-verified". Adding workspace methods to that trait forces a second unverified
Hyprland implementation to be written in the same commit, and this one is much larger than layout's:
Hyprland models one active workspace per monitor plus a globally focused monitor, niri models a
per-output `is_active` and a single global `is_focused`, and those two models do not map onto each
other by renaming fields. Writing that mapping without a machine to run it on is how a plausible
looking answer gets shipped, which is the failure this codebase has now hit three times (audio's
cubed `channelVolumes`, `freedesktop_icons::default_theme_gtk`'s directory-versus-display name, and
`inotify` on sysfs attributes).

A session with no `$NIRI_SOCKET` gets no `workspaces` push, ever, and the signal stays `nil`. That
is `brightness`'s missing-backlight posture applied unchanged (ADR-0053's amendment): § 2.9 has no
absence sentinel, an empty `outputs` array would read as "this compositor has no workspaces" rather
than "nobody asked this compositor", and ADR-0037's nil-until-hydrated contract already covers the
difference. The upgrade point is written down rather than guessed at: when a second compositor is
implemented and live-tested, extract the trait then, taking the two implementors' actual shapes as
the input instead of one implementor's shape plus a prediction.

## Decision 2: a second niri event-stream socket, not a shared one

`keyboard` already holds a `Request::EventStream` connection for `KeyboardLayoutsChanged`. This
opens a second one for `WorkspacesChanged`/`WindowsChanged`. Two connections to the same compositor
from the same process, each replaying niri's full startup state to a reader that discards most of
it.

That cost is real and it is the smaller one. Sharing the stream means one owner fanning events out
to two capabilities, which is a lifetime and ordering coupling between `keyboard` and `workspaces`
that neither has today, in a codebase whose every controller "owns its connection/thread, pushes
into a channel, `main.rs` `select!`s the receiver" (ADR-0034). The duplicated replay is a few
kilobytes once per boot.

ponytail: the ceiling is the third consumer. Two niri sockets is a duplicate, not a pattern; three
is a leak. The upgrade path is one niri link owning the stream and `EventStreamState` whole, with
per-capability subscribers, which is also the point at which `niri_ipc::state::EventStreamState`
gets used in full instead of the two parts each capability needs.

## Decision 3: § 2.9 cannot be rendered, so the payload adds the workspace list

§ 2.9 specifies, per output, `active_workspace: integer` and `focused_workspace: integer`, and
nothing else. Both are workspace ids. A config that reads them has two opaque numbers and no way to
turn either into a strip of buttons, a label, or anything else on a screen: it does not know which
workspaces exist, what they are called, or what order they sit in. The capability as specified
cannot draw its own subject.

So each output entry also carries `workspaces`, an array of `{ id, idx, name }` ordered by `idx`.
`id` is niri's stable, monitor-independent identity and is what `active_workspace`/`focused_workspace
` refer to and what `workspaces:focus(id)` takes. `idx` is the 1-based position on that output,
which is what a user sees on their keyboard and what a bar labels a button with, and it is
explicitly not stable across a reorder. `name` is niri's optional named workspace, `nil` when unset.

This is the same call ADR-0053 decision 3 made for `audio`, in the same direction: keep the field
the spec did not think to ask for, because dropping it makes the payload narrower and useless. The
difference is that this one is proved by a consumer in the same commit rather than argued for. § 2.9
gets an annotation pointing here.

The rest of § 2.9's shape is built verbatim. `is_urgent` is not carried, though niri reports it,
because nothing draws it yet and one unused field is how the next one gets added.

## Decision 4: `focused_workspace` is optional, because focus is global and § 2.9 says it is not

niri's `Workspace.is_active` is per output ("every output has one active workspace"), and
`Workspace.is_focused` is global ("there's only one focused workspace across all outputs"). § 2.9
puts both inside the per-output structure, which models focus as a per-output fact. It is not one.

Three ways to reconcile that. Repeat the single global focused id on every output entry, which tells
a two-monitor config that both its monitors have keyboard focus. Hoist the field out of the array,
which contradicts the spec's own structure for a config author reading it. Or make it optional:
present, and equal to a real workspace id, on the one output that actually holds focus; absent
everywhere else.

Optional wins, and it costs nothing on the common single-output machine, where it is always present
and always equal to `active_workspace`. It preserves § 2.9's field name and its placement, it never
states something false, and a config's `out.focused_workspace ~= nil` is the exact test for "is this
the focused monitor", which is a question a bar asks and § 2.9 gave it no other way to answer.

## Decision 5: `active_client.is_fullscreen` is omitted, and `class` is `app_id` renamed

§ 2.9's `active_client` asks for four fields. niri's `Window` answers three of them.

`title` is `Window.title`. `is_floating` is `Window.is_floating`. `class` is `Window.app_id`, which
is a rename rather than a match: `class` is X11's `WM_CLASS`, and a Wayland toplevel has an `app_id`
instead. Every shell doing this makes the same substitution, and the IDL's own example values
(`"Alacritty"`, `"firefox"`) are app ids in practice.

`is_fullscreen` has no source. niri-ipc 26.4.0's `Window` struct has no fullscreen field, its event
stream never reports one, and `Request::FocusedWindow` returns the same struct. Fullscreen appears
in that crate only as actions (`Action::FullscreenWindow`, `Action::ToggleWindowedFullscreen`),
which is a thing you can do, not a thing you can read.

So the key is omitted from the payload and reads `nil` in Lua, rather than being reported as
`false`. `false` is a specific claim, it is the claim a config will branch on, and it would be wrong
for exactly the windows a fullscreen check exists to find: a video player, a game, a presentation.
This is `brightness`'s "no absence sentinel, so stay absent" applied to one field instead of a whole
capability. A later compositor that can answer it fills the key in; nothing else changes.

## What this does not decide

Whether `workspaces` should ever report windows beyond the focused one. § 2.9 specifies exactly one
`active_client` and this builds that. A window list is what a taskbar needs and no phase owns one;
`niri_ipc::state::WindowsState` already holds the whole map this controller reduces, so the data is
one field away whenever a taskbar is actually specified.
