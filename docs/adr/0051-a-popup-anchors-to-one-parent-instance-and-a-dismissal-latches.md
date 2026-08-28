# A popup anchors to one parent instance, and a compositor dismissal latches

ADR-0049 settled when a popup's `xdg_popup` exists: only while `visible` resolves true, created per
open, with the grab serial armed by input dispatch and disarmed at the end of the poll turn. Two
things it did not settle turn out to block the implementation, and both are the kind that a config
depends on once it works.

## Decision 1: a popup anchors to the parent instance the arming click landed on

§ 6.3's `parent` names a declared `id`. A `panel` is not one surface: `monitor = "All"` expands it
per output (ADR-0038 decision 3), so `parent = "bar"` on a two-monitor session names two
`zwlr_layer_surface_v1`s, and `xdg_surface.get_popup` takes exactly one parent.

A popup does not expand per output the way a panel does. It is opened by one click, on one monitor,
and it belongs there. So its instance id is the bare declared `id`, like a `window`'s, and the
parent it roots under is the instance whose surface the arming click was delivered to.

That is the same event this ADR's other half already tracks. The press that arms the grab serial
names a surface, so the parent instance costs one more field on the same record rather than a second
mechanism. When no click armed it (a `grab = false` popup opened by a D-Bus notification, say) the
first instance of the named parent is used, with a `ponytail:` naming the ceiling.

The alternative was expanding a popup per parent instance, giving `click_menu@eDP-1` and
`click_menu@DP-1`. It is wrong for the reason that makes it tempting: one `visible` signal drives
both, so a single click would open a dropdown on every monitor. A dropdown belongs to the click, not
to the output set.

## Decision 2: a compositor dismissal latches until `visible` cycles

`xdg_popup.popup_done` is the compositor saying the object is gone. Click-outside is the ordinary
cause, and it is the entire reason ADR-0040 reached for a real `xdg_popup` instead of a second
`panel`.

The engine destroys the object and fires § 6.3's `on_dismiss`, which is where a config writes
`visible = false` back. What it must not do is trust the config to. A dismissal leaves the resolved
tree still saying `visible = true`, so the next re-resolve would create a second popup, which the
same click-outside would dismiss, forever. A config with no `on_dismiss` at all is not a config
error, and it must not be a livelock.

So a dismissed popup is latched: the engine records that this declaration was dismissed by the
compositor and refuses to create it again until `visible` resolves false and then true. The latch
clears on the false, so the config's own `on_dismiss` reopens the path immediately and a config
without one reopens it on the next deliberate close.

This is the same shape as `WindowHandler::request_close`, and deliberately not the same rule.
`close` is a request the client may ignore, so the engine destroys nothing and lets the config
decide. `popup_done` is not a request: the object is already gone, and the only question left is
whether a replacement appears unasked.

## Decision 3: a refused grab means no popup, not a popup without one

Restating ADR-0049's amendment because this is where it becomes code. `grab = true` and no armed
serial means the popup is not created at all, logged once.

There is a second, protocol-side refusal that reads the same way from the config: the compositor may
deny a grab it was asked for, and § 6.3 says so ("A compositor may deny the grab, in which case the
popup is dismissed immediately and `on_dismiss` fires; treat that as a normal outcome, not an
error"). That arrives as an immediate `popup_done` and goes through decision 2 unchanged. Nothing
distinguishes it from a click-outside on this side of the wire, and nothing needs to.

## Consequences

Nested popups still close in reverse creation order (ADR-0040), and a parent's dismissal cascades:
`popup_done` on a parent means the compositor already destroyed the children, so the engine's job is
to drop its handles in that order, not to send anything.

A popup's surface instance exists from generation startup even though its `xdg_popup` does not,
exactly as a `window`'s does and for exactly the same reason: the instance is what makes the scene
resolve the popup's tree at all, and `visible` is read off that resolved tree. Twenty declared
popups still cost twenty retained nodes and zero Wayland objects, which is ADR-0049's memory claim
intact.

The latch is per declaration and lives on the tracked surface, so it dies with the generation. A PBA
swap starts every popup unlatched, which is correct: a new generation has shown nothing yet.

## Deliberately not built: a popup on more than one monitor at once

Decision 1 gives a popup one instance and one parent. A config that genuinely wants a dropdown on
every bar at once declares one popup per monitor and drives them separately, which is more typing
and is also what it actually means. The upgrade path, if anything ever wants it, is per-parent-instance
expansion plus a per-instance `visible`, and that second half is the part that does not exist.

## Amendment: the latch clears on the next click, not on `visible` going false

Decision 2 above says a dismissed popup stays latched "until `visible` resolves false and then true".
That edge is unobservable in the one case that matters, so as written the latch is permanent.

The engine samples `visible` once per poll turn, after `dispatch_pending` has drained the whole
event batch. Under a grab, niri still delivers the click that closes a popup to the parent bar as
well, because the bar is the popup's own parent surface and so inside the grab's tree. That is
measured, not assumed: `dev-config/oblisk/shell.lua` records it. So `popup_done` and the button's
`on_click` land in the same batch. `on_dismiss` writes `false`, `on_click` writes `true`, and the
single end-of-turn sample reads `true`. The false edge never existed as far as the engine can see,
the popup stays latched with `visible = true`, and nothing is left that writes `false`. The dropdown
opens once per generation and is then dead.

The fact that separates the two cases is not the value of `visible`. It is whether the user asked
again. A dismissal followed by no input is exactly the livelock decision 2 was written to stop. A
dismissal followed by a click is a person reaching for the dropdown a second time.

So the latch records *when* it was set, counted on the same pointer input that arms the grab serial,
and a popup is latched only while no pointer input has arrived since its dismissal. A config with no
`on_dismiss` still cannot livelock: nothing new arrives, the counter does not move, the latch holds
for the life of the generation. A config that reopens on the next click gets it on the next click.

## Amendment: a grabbing popup cannot be opened from `on_click`

Decision 3 treats a refused grab as an outcome the config can see and reason about. There is a
second refusal it did not anticipate, and the config has nothing to do with it.

wlroots validates a pointer grab against two conditions, in
`wlr_seat_validate_pointer_grab_serial`: `button_count != 1 || grab_serial != serial` fails it. A
popup opened from `on_click` fails both. ADR-0050 decision 2 defines a click as a press and a
release on the same node, so `on_click` fires on release, and by the time the config writes
`visible = true` the button count is 0 and the press serial is no longer current. Handing it the
press serial instead of the release's fails the button-count half just the same. There is no serial
the engine can pass that validates.

niri accepts the grab because smithay does not run that check. That is the whole reason this works
on the development machine and would flash open and shut on sway or Hyprland.

The engine is not the thing that is wrong. A grabbing popup has to be created while the button is
still down, which is what GTK and Qt menus have always done: a menu opens on press, not on release.
That is an IDL change rather than an engine one, and § 6.1 has no press-time hook to hang it on.

ponytail: shipped as it stands, which works on smithay compositors and is refused on wlroots ones.
The upgrade path is § 6.1 gaining an `on_press` beside `on_click`, at which point ADR-0049's serial
amendment narrows to arming on press alone, since a release serial can never validate anywhere.
