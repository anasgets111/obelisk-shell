# The Wayland client addresses surfaces by position, the retained scene by identity

The 2026-08-31 architecture review proposed rekeying `wayland::App::surfaces` from a `Vec` index to
the surface instance id, on the grounds that `layout::scene` already keys its top-level surfaces by
id and ADR-0038 records why ("surfaces are identified, not ordered", citing ADR-0023). Measured
against the code, the two halves are both right and the difference is not drift. Recording that,
because 26 methods taking `index: usize` sitting next to ADR-0038 will read as an inconsistency to
the next reader, exactly as it did to this one.

## What ADR-0038 actually governs

"Identified, not ordered" is about how a config declares surfaces and how the retained scene matches
a fresh evaluation against the one already applied. The scene keys by id because `shell.lua` names
its surfaces and node identity has to survive a reload. None of that reaches how the Wayland client
stores the protocol handles for one generation's live surface instances.

## Where the indices come from

Every index in `wayland/` has one of three origins, and none of them is a caller holding an id.

| origin | sites | what rekeying costs |
|---|---|---|
| A protocol event carrying a `wl_surface` | `xdg_shell.rs:715, 763, 798, 830`, `input.rs:785`, `layer.rs:348`, `lock.rs:499` | `index_of_surface` becomes `surface_id_for`, and the callee scans again for the index it needs anyway |
| A bulk loop that iterates and mutates | `surface.rs:514, 974, 1031`, `lock.rs:217, 250`, `xdg_shell.rs:686` | a `Vec<String>` allocation per loop, then a scan per element, on the Wayland dispatch thread in the paint and activate-draw paths |
| A cross-module call | `layer.rs:352`, `lock.rs:254, 503`, `xdg_shell.rs:447, 776, 805`, `surface.rs:452, 555, 561, 671, 673, 679` | the caller already resolved the index two lines above |

The decision rests on the exception rather than the rule. `App::destroy_surface_by_id` already takes
`&str`, and its two callers (`layer.rs:337`, `output.rs:171`) have an id and no index, because they
come from output removal and instance reconciliation rather than from a protocol object. The code
already uses identity exactly where identity is what the caller holds, and position everywhere the
caller holds a position. That is the same answer arrived at twice, not one answer applied
inconsistently.

The constraint the index actually serves is stated at `surface.rs:1053`: these bodies need
`&mut self` for EGL state and `self.text_painter` at several points, which a held
`&mut TrackedSurface` would conflict with. `paint_surface` alone touches `client`, `egl`, `gl`,
`image_cache`, `shaping`, `text_painter`, `exit` and `surfaces`.

## Rejected alongside it: folding the `MapState` writes into named transitions

Nine sites assign `map_state` across four files, which looks like a state machine written by hand.
The fold is worth doing only if a transition carries an invariant, and the candidate invariant was
that `null_buffered` must move with it. It must not. `null_buffered` is PBA-candidate staging, set
only inside the `is_pba_candidate` branch of `bind_and_clear` and cleared at the two sites that
destroy a `window` or `popup` object and so need it restaged. `App::unmap` leaves it alone because a
panel's object survives the unmap. With no invariant to enforce,
`self.surfaces[index].unmapped()` says less than the assignment it would replace.

## Consequences

`wayland/` keeps 26 methods taking `index: usize` and roughly 123 `self.surfaces[index]` accesses.
That is the intended shape, not a backlog item.

This ADR does not defend `App`'s size. It holds around 30 fields and 169 methods across six
`impl App` blocks, nothing constructs one in a test, and `renderer/src/wayland` measures 4,755
production lines against 1,359 test lines, the lowest ratio in the repo against 32 commits in six
weeks. Addressing was the wrong explanation for that. The reachable version of the problem is that
decisions and protocol calls sit in the same methods: `hit_path` and `hover_writes` carry nineteen
tests between them while their only callers, `App::hit_under` and `App::sync_hover`, have none.
