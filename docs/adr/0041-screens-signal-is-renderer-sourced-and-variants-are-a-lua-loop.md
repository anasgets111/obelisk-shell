# `oblisk.screens` is Renderer-sourced; variants are a Lua loop, not a primitive

Quickshell's per-monitor idiom is `Variants { model: Quickshell.screens; PanelWindow { screen:
modelData } }`: a declarative repeater that instantiates one window per model entry and re-runs when
the model changes. Building a Quickshell-equivalent means answering what Oblisk's version of that
is. Two separate questions hide inside it, and only one of them needs anything built.

## Decision 1: no `variants` primitive. Lua already has `for`

QML has no loops. `Variants`, `Repeater`, and `ObjectRepeater` exist because a declarative markup
language cannot express "one of these per element" any other way. Lua can:

```lua
local panels = {}
for _, screen in ipairs(oblisk.screens:get()) do
    panels[#panels + 1] = panel { id = "bar@" .. screen.name, monitor = screen.name, ... }
end
return panels
```

Adding a `variants` constructor would wrap a language feature the config language already has, which
is the first rung of AGENTS.md's ladder ("does this need to be built at all"). This is one of the
concrete wins of choosing Lua over a declarative markup, and it should be spent rather than
neutralized. Do not re-propose a repeater primitive; if a future need appears, it will be for
reload identity (decision 3), not for iteration.

## Decision 2: `oblisk.screens` is a Renderer-local signal, not a Supervisor capability

The loop above needs a screen list, and none exists today. Lua can see no output information at all.

`oblisk.screens` carries what `wl_output` reports, per connected output: `name` (connector, `"DP-1"`),
`width`, `height`, `scale`, `refresh`. It is a reactive signal: outputs appearing and disappearing
update it.

It is sourced in the Renderer from `smithay_client_toolkit`'s `OutputState`, which the Renderer
already maintains and already needs (`create_wallpaper_layers` reads it today, and layout cannot
resolve without real output dimensions). The Supervisor is not involved.

This is a deliberate exception to the shape ADR-0037 established, and worth stating plainly because
it is the first one: every Lua signal until now is a capability the Supervisor owns, pushes as a
`StateSnapshot`, and lists in `shared::CAPABILITIES`. `screens` is none of those. The roster stays
Supervisor-only; `screens` is a Renderer-local global seeded at VM construction and updated from
output events on the same thread.

Routing it through the Supervisor was considered and rejected. The Supervisor's own Wayland
connection exists for idle-notify and lock authority (ADR-0010) and binds no outputs. Making it bind
`wl_output` and push snapshots would add a process hop and a second source of truth for geometry
the Renderer must hold anyway to lay out against. ADR-0039 removes the only reason this was ever
awkward: once the Lua VM shares the Wayland thread, `OutputState` is a local read.

## Decision 3: identity is the `id` set, and it decides swap versus in-place

Two ways to put a panel on every monitor, with different reload costs, and the difference is
visible enough that a config author should be able to pick deliberately:

`monitor = "All"` declares **one** surface. The engine expands it to one surface instance per
output (ADR-0038 decision 3). Hotplug changes the instance set, not the declared `id` set, so it is
handled in place with no generation swap.

An explicit loop declares **N** surfaces with N distinct ids. Hotplug changes the id set, which is a
topology change, which is a generation swap (ADR-0001). This is the correct classification, not a
regression: the config genuinely declares different surfaces before and after, and PBA exists to
make that swap glitch-free. Docking a laptop is not a high-frequency event.

The rule to state in the docs: use `monitor = "All"` when every screen gets the same panel, and a
loop when screens get genuinely different content and a swap on hotplug is acceptable.

## Decision 4: a hotplug reload reuses the file-edit reload path exactly

A config that loops over `oblisk.screens` must be re-evaluated when that list changes, or its
per-screen panels go stale. That re-evaluation reuses ADR-0024's existing machinery rather than
adding a second reload path: on an output change the Renderer re-evaluates, diffs its own topology,
and reports `Unchanged` / `TopologyChanged` / `Failed` to the Supervisor exactly as it does for a
`Reevaluate` frame. The Supervisor stays the one authority that decides in-place versus swap and
dispatches it.

The only new thing is the trigger. `inotify` on `~/.config/oblisk/` is one; a `wl_output` change is
now another. Rollback, rescue, and the topology diff are unchanged.

## Consequences

`oblisk-idl-api-specs.md` § 2.9 currently gives `workspaces.outputs` a per-output structure carrying
`name`, `width`, `height`, `scale`, `active_workspace`, and `focused_workspace`. The first four now
duplicate `oblisk.screens` from a worse source: they would come from the still-undesigned compositor
workspace adaptor (services § 10) rather than from `wl_output`, and the Renderer would be laying out
against one copy while Lua reads another.

Split them by what actually knows the answer. `oblisk.screens` owns geometry, scale, and connector
names, available now with no adaptor. `oblisk.workspaces` keeps workspace state and refers to
screens by `name` instead of restating their geometry. § 2.9 is edited accordingly.

Popups need this too, though for a different reason: an `xdg_positioner`'s constraint adjustment is
resolved by the compositor against the output the popup lands on, so a config that positions its own
popups needs to know which screen it is on. That dependency is recorded here rather than in the
popup ADR because the signal is the same one.
