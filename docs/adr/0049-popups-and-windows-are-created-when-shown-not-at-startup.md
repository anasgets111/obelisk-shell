# Popups and windows are created when shown, not at generation startup

ADR-0038 decision 2 and ADR-0040 decision 2 contradict each other, and both are mine.

ADR-0038: "A generation creates exactly the surfaces its own evaluation declared, once, at startup."
Inside a live generation only two things move, `visible` mapping and unmapping an existing surface,
and the fields layer-shell permits changing in place.

ADR-0040, quoting the protocol: a grab "must answer a real input event (button press, key press,
touch down) and must be requested before the popup is mapped; after mapping it raises
`invalid_grab`." The anchor rect comes from the rect `on_click` returns.

A popup created at startup and unmapped cannot satisfy that. Its grab would have to be requested
before its first mapping, using a serial from an input event that has not happened yet. Worse,
`xdg_positioner` is consumed by `get_popup`, so a popup created once is anchored once: a dropdown
cannot follow whichever button opened it without `xdg_popup.reposition`, which ADR-0040 deferred.

The protocol decides this, not preference. Popups are per-open objects.

## Decision 1: the role decides the lifetime

| Role | Wayland object exists |
| :--- | :--- |
| `panel` | for the generation's whole life, created at startup |
| `lock` | while the session is locked (ADR-0042) |
| `popup` | only while shown |
| `window` | only while shown |

ADR-0038 decision 2 is correct for `panel`, which is the only role that existed when it was written.
It is amended rather than replaced: what a config *declares* is still fixed for a generation's life,
and adding or removing a declaration is still the topology change ADR-0001 routes to a swap. What
changes is that for two roles, the declaration and the Wayland object no longer have the same
lifetime.

## Decision 2: `visible` creates and destroys, rather than mapping and unmapping

Same Lua-facing property, same declarative model, different mechanics underneath. When a `popup` or
`window` node's `visible` resolves true, the engine creates the Wayland object; when it resolves
false, it destroys it.

Nothing new is needed to drive it. `on_click` writes named state (ADR-0044 decision 5), the write
marks the scene dirty, the re-resolve reads `visible` as true, and creation happens inside that
re-resolve. That re-resolve is still running inside input dispatch, so the engine has the serial of
the event that caused it, which is exactly what `grab` requires. The anchor rect is read at the same
moment, so it reflects the button actually clicked.

This is why the IDL's existing rule holds without new machinery: "A popup may only be opened in
response to real user input, so `grab = true` outside an input callback is rejected." A dirty
re-resolve outside input dispatch has no serial to offer, and the rejection is the engine noticing
that rather than a rule bolted on.

## Decision 3: opening a popup is a value change, never a topology change

It has to be, or every dropdown would spawn a candidate process and wait for presentation evidence
before it could appear.

The declared set is what ADR-0001 keys on, and opening a popup does not change it: the `popup` node
was declared and stays declared whether or not it is currently shown. Only adding or removing the
declaration itself is a topology change. This keeps ADR-0001's split intact and keeps the reason
ADR-0038 gave for refusing in-place surface creation intact too, because that reason was about the
declared set changing underneath a generation, which this does not do.

## Consequences

This delivers what Quickshell's `LazyLoader` delivers, without a `LazyLoader`. A config with twenty
popups that are never opened holds twenty nodes in the retained scene and zero Wayland surfaces,
zero buffers, and zero EGL surfaces. Against ADR-0043's 50 MB per monitor that is the difference
between paying for what is visible and paying for what is declared, and it falls out of the protocol
constraint rather than being bought with a new primitive.

Destruction order is the engine's to own, and ADR-0040 already named the rule: nested popups are
destroyed in reverse creation order. A parent popup whose `visible` goes false destroys its children
first.

`build-steps.md` Phase 20 item 1's "Do not add in-place surface creation or destruction" applies to
panels, which is all that phase builds. Phase 22 is where creation-on-show lands, alongside the roles
that need it.

## Deliberately not built: `xdg_popup.reposition`

A popup that follows a moving anchor while open. ADR-0040 deferred it and this ADR does not change
that. Decision 2 makes the common case work by creating a fresh popup per open, which covers a
dropdown that opens under different buttons. Only an anchor that moves *during* an open popup's life
needs `reposition`, and nothing needs that yet.
