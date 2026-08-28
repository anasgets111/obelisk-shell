# Popups and windows are created when shown, not at generation startup

> **Amended before Phase 22 was built.** Decision 2's mechanism is wrong about *where* the
> re-resolve happens, which matters because the whole `grab` argument rests on it. The decision
> stands and so does the rule it protects; the serial reaches `grab` by a different route than this
> predicted. See the amendment at the end.

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

## Amendment: the re-resolve does not run inside input dispatch

Decision 2 claims the re-resolve that creates a popup "is still running inside input dispatch, so
the engine has the serial of the event that caused it." Phase 21 built the input path and it does
not work that way, and could not have: `re_resolve_if_dirty` runs in `wayland::run`'s poll loop,
after `dispatch_pending` has returned. A click's chain is `PointerHandler::pointer_frame` ->
`on_click` -> `Signal::set` marks the dirty flag -> the callback returns -> the poll loop
re-resolves. Same turn, microseconds later, but the dispatch callback's stack is gone and with it
any serial sitting on it.

Restructuring the loop to resolve inside dispatch is the wrong fix. It would put a full
`Scene::apply`, arbitrary Lua, and Wayland object creation inside a `Dispatch` callback, reentering
the queue it is being dispatched from.

**The serial is armed by input dispatch and disarmed at the end of the poll turn.** A pointer press
records its serial; the poll loop clears it after the re-resolve and apply have run. So a popup
created by a click finds a serial, and a popup created by anything else does not, because nothing
else ever armed one.

That keeps the IDL's rule exactly as decision 2 states it, and keeps the reason: "A popup may only
be opened in response to real user input, so `grab = true` outside an input callback is rejected."
A notification arriving over D-Bus marks the scene dirty and re-resolves with no armed serial, so a
popup it tried to open with `grab = true` is refused. The engine is still noticing the absence
rather than enforcing a bolted-on rule; the absence is just recorded in a field for the length of
one poll turn instead of living on the stack.

Refused means the popup is not created at all, not created without its grab. A dropdown that cannot
be dismissed by clicking outside it is worse than one that did not open: the click-outside dismissal
is the whole reason ADR-0040 reached for a real `xdg_popup` instead of a second `panel`.

One consequence decision 2 got right for the wrong reason: the anchor rect still reflects the button
actually clicked, because `on_click` receives that rect (ADR-0050 decision 3) and writes it to a
`state` signal the `popup` reads. It arrives through the config rather than off the dispatch stack.

## Amendment: a popup's spec is derived from resolved properties, not from the evaluation

A second thing the serial mistake was hiding. `socket.rs`'s `panel_specs` parses a top-level node's
**raw** `VirtualNode::properties`, before `node::resolve_properties` has run over them. That is
correct for a `panel`, whose topology fields reject a `Signal` on purpose: `layer` and `namespace`
are fixed when the layer surface is created, so a signal in one of them is a config error worth
naming.

A `popup` is the opposite case. Its `anchor_rect` is *supposed* to change, and decision 2's whole
point is that it changes to wherever the last click landed. Parsing it at evaluation time freezes it
at whatever the config saw when the file was last read, which for a dropdown means it opens over the
button that was clicked before the last reload.

So the authoritative `PopupSpec` is built from the **resolved** tree, at the point the surface is
reconciled, where `resolve_properties` has already run exactly once for that pass (ADR-0044
decision 1). The positioner is fed from that, not from the evaluation's raw node.

Evaluation-time parsing stays, narrowed to what it can honestly check: a property holding a literal
is validated there, so a typo fails fast and lands in ADR-0046's `rescue` log rather than surfacing
as an `xdg_positioner` protocol error at first open. A property holding a `Signal` is skipped there
and checked when it resolves. Two passes over the same properties, each checking what it is in a
position to know.

Resolving inside `panel_specs` instead is the wrong shortcut. It would run Lua getters during
topology parsing, and that function also runs on every monitor hotplug, so each getter would be
bought twice per pass against ADR-0021's budget and `resolve_properties`'s one-read-per-pass
guarantee would stop being true.
