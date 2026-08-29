# Five specified capabilities were never given a phase, and a bar is what found them

A request to make `dev-config/oblisk/shell.lua` look like a bar people actually run turned into an
audit, because the first three modules any such bar has (a clock, a battery, a volume readout) could
not be written against this codebase at all. Not because the Lua vocabulary is missing. Because
nothing feeds them.

## What the audit found

`docs/oblisk-idl-api-specs.md` § 2 specifies fifteen capabilities. `shared::CAPABILITIES` holds
eleven, and the two sets do not line up:

| § 2 | Built |
| --- | --- |
| 2.2 `battery` (`present`, `percent`, `charging`) | nothing |
| 2.3 `brightness` (`percent`) | nothing |
| 2.4 `audio` (`volume`, `muted`, `sinks`, `sources`, `apps`) | `apps` only, under different field names |
| 2.9 `workspaces` | nothing |
| 2.11 `system` (`state`, `time`) | nothing |
| 2.13 `power` (profiles, `on_battery`, `energy_rate`) | nothing |

The other nine are built. Three capabilities exist that § 2 never specified (`privacy`, `updates`,
`lock`), each traceable to an ADR that added it deliberately.

**No phase in `build-steps.md` owns any of the six rows above.** Phase 16 built "the capability
roster" and was scoped by `oblisk-supervisor-services-dbus.md`'s section numbers, not by the IDL's,
so the IDL-only entries fell between the two documents. The single mention anywhere is
`build-steps.md` line 98, justifying the `udev` dependency by "§ 1.1's battery netlink monitor" for a
battery monitor no phase then wrote. This is the same failure as the one recorded before Phase 18:
ADR-0023 item 2 deferred signal-driven re-evaluation and named Phase 13 as the place to resume it,
Phase 13 shipped without it, and no later phase claimed it back.

Worth stating plainly because it is the useful part: the gap was invisible for as long as nobody
tried to write a realistic config against the engine. Fifteen unit-tested capability payloads and a
fixture config that exercises the layout engine did not surface it. Asking for a clock did, in about
a minute.

## Decision 1: build `battery`, `system` and audio's missing fields now; give the rest a phase

`battery` (§ 2.2), `system` (§ 2.11) and `audio`'s `volume`/`muted` (§ 2.4) are what the bar needs
and are each small. They land now.

`brightness` (§ 2.3), `workspaces` (§ 2.9) and `power` (§ 2.13) get a phase of their own rather than
riding along. `workspaces` in particular is not small and not obviously ours: it is compositor
specific, `niri-ipc` is already a dependency, and deciding whether the capability speaks one
compositor's IPC or an abstraction over several is a design question, not an implementation one.
Doing it badly here to finish a bar would be the wrong trade.

## Decision 2: `system.time` pushes on change, and the repaint cost is named rather than absorbed

§ 2.11 asks for a time value "updated at 1-second intervals". Taken literally that is a
`StateSnapshot` per second across the control socket, each one marking the scene dirty (ADR-0044)
and driving a full re-resolve and repaint of every surface, forever, on a laptop.

So the controller emits only when the epoch second it would report actually differs from the last
one emitted. That is the same once-per-second in the steady state and nothing at all if the tick
ever runs fast, and it costs one comparison.

That is a floor, not a fix, and the honest ceiling is worth writing down: a bar drawing `HH:MM` is
repainted sixty times a minute to change once. The upgrade path is a configurable interval, which
`sysinfo` already has the shape for (a `watch` channel per task, reconfigured through
`sysinfo:configure`), and which needs the Lua write path from Phase 25 before anything can call it.
Not built now, because nothing can call it now.

## Decision 3: `audio`'s payload takes § 2.4's shape, and its existing fields are renamed

The built `audio` payload is a bare JSON array of `{node_id, pid, app_name, process_name}`. § 2.4
specifies an object with `volume`, `muted`, `sinks`, `sources` and `apps`, where an `apps` entry is
`{id, name, volume, muted}`. Every name and the outer shape differ.

Adding master volume forces the outer shape to change regardless, since a bare array has nowhere to
put a scalar. Given that, the field names go to § 2.4's spelling in the same move rather than
leaving a payload that matches neither the spec nor its replacement. The `pid` field is kept
alongside § 2.4's fields rather than dropped: ADR-0016 exists because finding the owning process was
genuinely hard, and discarding the answer to make a table narrower would be throwing away the one
part of this payload that took real work.

This breaks the one consumer, `dev-config/oblisk/shell.lua`, which is rewritten in the same commit.
No other config exists.

Master `volume` and `muted` are real. Per-app `volume` and `muted` ship as `1.0` and `false`, marked
as placeholders in the code, because each stream node needs its own `SPA_PARAM_Props` subscription
and that is a second slice rather than a wider match arm. A field that is present and wrong is worse
than one that is absent, so this is the trade being taken knowingly: the names exist because § 2.4
names them and a config can already lay out against them, and the numbers are marked at their one
source rather than left to be discovered.

The parse itself was worth the trouble twice over. A sink node advertises two objects under
`ParamType::Props`, not one: the mixer object, and an unrelated ALSA device-settings object carrying
none of the mixer keys. Reading the second one as a mixer object with no channels yields a volume of
zero, which then overwrites the correct value the first one had just produced. Presence of
`channelVolumes` is what tells them apart. The symptom was a volume that was right exactly once per
boot and zero afterwards, which no unit test over a captured mixer pod would have caught, because the
captured pod is the object that parses correctly.

## Amendment: `brightness` is built, and three of its four decisions were not in the original

Decision 1 deferred `brightness` on the grounds that it had "no consumer pressing for it". It has
one now. The original decision stands (it is a phase item of its own, not bundled with the three
above) and this records what building it settled.

**The read watch is udev, not inotify.** `build-steps.md` line 98 justified the `inotify` dependency
partly with "§ 1.2's backlight watch". Inotify does not fire on a sysfs attribute write. `keyboard`
had already found this for the LED-state files and said so in its own module doc; the same was
confirmed here with `udevadm monitor --udev --subsystem-match=backlight`, which shows a `change`
uevent on the `backlight` subsystem for every brightness change. So this uses the `AsyncFd` udev
monitor `battery` built, with the same 30s poll fallback. `inotify` keeps its other justification,
the config-directory watch, and loses this one.

**Device selection ranks by `type`.** `firmware`, then `platform`, then `raw`, tie-broken by sorted
directory name, and any device whose `max_brightness` is not positive is skipped. This is the
preference the kernel exposes the `type` attribute for
(`Documentation/ABI/stable/sysfs-class-backlight`). The alternative, first-in-readdir-order, is what
`brightnessctl` does and is fine on a machine with one device, which is most machines since the
kernel started registering only the backlight it believes works. It is wrong on a machine with both
`acpi_video0` and a native device, and the fix costs one comparator.

**The write is `login1.Session.SetBrightness`, not a sysfs write.** `/sys/class/backlight/*/
brightness` is root-owned `0644`, and the Supervisor runs as the user and is deliberately not
privileged. Writing it directly means shipping a udev rule that grants a group write access to a
device node, which is an install-time dependency and a permission grant to every process the user
runs. `SetBrightness(subsystem, name, brightness)` on the `session/auto` path needs neither and was
confirmed to succeed as this user. The cost is that logind refuses it from a session that is not the
seat's active session: a `set` issued from a background VT logs and does nothing. That is logind
protecting a display the caller does not own, and it is the correct behaviour to inherit rather than
work around.

**No backlight device means the capability never pushes at all.** § 2.3 specifies `percent: integer
[0, 100]` and gives no absence sentinel, unlike `battery.present` or `sysinfo.temp_gpu`'s `-1`. A
fabricated `0` reads to a config as "the screen is off", not "there is no backlight hardware", so
the signal stays `nil` forever and ADR-0037's nil-until-hydrated contract carries it. Inventing a
sentinel the spec does not have was the other option and was rejected: a config author reading § 2.3
would have no way to know about it.

This is the same hole decision 3 left open in `audio`, resolved the other way. `audio` has no way to
say "unknown", every unavailable path there collapses into a plausible number, and that is how a
failed sink bind hid as a 0% volume. `brightness` does not repeat it.

## What this does not decide

Whether `oblisk.system`'s `state` should ever be writable. § 2.11 calls it "read-only" and this
implementation loads `state.json` once at construction, so a config can read persisted state and
cannot create it. Something has to write that file for the feature to mean anything, and nothing
does. That is a real hole, it is not this ADR's to fill, and it is named here so the next person
does not read `system.state` as finished.
