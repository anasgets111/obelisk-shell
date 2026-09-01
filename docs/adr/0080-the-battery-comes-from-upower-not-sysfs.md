# The battery comes from UPower, not sysfs

Supersedes build-steps.md line 98's `power_supply` udev watch, and the `charging: boolean` in
§ 2.2. `oblisk.battery` now reads UPower's `DisplayDevice` over D-Bus and follows its
`PropertiesChanged`. No sysfs, no udev, no timer.

Two faults, reported one after the other from the same bar, and one source fixes both.

## The reading was stale

The pill's percentage and its fill sat still while the machine discharged, and the tooltip's
charging line sat still with them. `run_battery_task` read `/sys/class/power_supply` when udev said
something had happened, and fell back to a 30s poll only when the watch failed to *build*. That
assumed a watch which builds is a watch which fires.

Measured on this dev machine, `udevadm monitor --udev --subsystem-match=power_supply` alongside a
sampler:

| | |
| --- | --- |
| `capacity` over the window | 69, then 65 |
| `power_supply` uevents delivered | 0 |
| UPower's own view, same battery | tracked every point, "updated: 13 seconds ago" |

The ACPI battery driver emits a uevent on a plug or an unplug and, on this hardware, on nothing
else. So `charging` was live and `percent` was frozen between cable moves.

## A boolean could not say what was happening

The second report: the pill and its tooltip read the same on mains at the charge limit as they did
on battery. They had to. `charging` was `status == "Charging" || status == "Full"`, and sysfs has
five status words where the states a laptop with a charge threshold actually visits are:

| what is happening | sysfs `status` | old `charging` | now |
| --- | --- | --- | --- |
| taking current | `Charging` | `true` | `Charging` |
| on battery | `Discharging` | `false` | `Discharging` |
| at the limit, on mains | `Not charging` | `false` | `PendingCharge` |
| above the limit, draining to it, on mains | `Discharging` | `false` | `PendingDischarge` |
| at the top of the battery | `Full` | `true` | `FullyCharged` |

Three rows collapsed into `false` and one said `true` for a battery that is not charging. This
machine's `charge_control_end_threshold` is 70, so the third row is most of every day.

Rows three and four are the ones sysfs cannot fix on its own. `Not charging` and `Discharging`
would each need the mains adapter's `online` bit read separately and combined, which is UPower's
`up-device-supply` logic reimplemented from the same files it reads.

## Decision 1: `DisplayDevice`, and the seven states by name

`BatteryStatus` is UPower's `Device.State` numbering mapped to its own names, serialized as a
string, so a config compares `b.state == "PendingCharge"` -- the shape `mpris`'s `play_state`
already uses at this boundary. An eighth state in some future UPower reads as `"Unknown"` rather
than failing the capability.

`present` is `Type == Battery && IsPresent`, both halves, which is the pair Quickshell's
`isLaptopBattery` tests before it believes a percentage. `IsPresent` alone is true on hardware that
is not a battery.

`time_to_empty` and `time_to_full` come along in the same `GetAll` and are `nil` when UPower reports
`0`, which it does while charging and before it has estimated. Neither is a duration.

The path is well-known and fixed, so the proxy addresses `/org/freedesktop/UPower/devices/
DisplayDevice` directly rather than calling `GetDisplayDevice()` for an address that is documented
to be exactly that. One `PropertiesChanged` subscription for the whole object, not one per property,
because a percentage that moves while the state flips arrives as one message.

**This is Quickshell's shape, checked against its source.** `src/services/upower/core.cpp` calls
`GetDisplayDevice()` and `device.cpp` binds that path through `DBusPropertyGroup`, which does one
`GetAll` and then follows property changes. There is no timer anywhere in it. It works because
UPower is the one polling, and every client on the system inherits that.

## Decision 2: no sysfs fallback

A host without UPower prints one line and reports nothing, which is what § 2.13 already does for a
missing power-profiles-daemon and what ADR-0053 established for a capability with no implementor.

Keeping the sysfs reader as a fallback was considered and dropped. It cannot fill the payload it
would be falling back for: no `PendingDischarge` without reading the mains adapter too, no
`time_to_*` at all, and it is the path measured above as unable to see a change the kernel does not
announce. A fallback that reports a worse answer under the same field names is harder to diagnose
than no answer.

## What this does not do

**No 0% glitch suppression.** The reference config holds the last percentage when UPower reports a
spurious 0% on AC, and exposes the flag so its derived `isPendingCharge` can suppress its own
flicker. That is roughly six lines here, keyed on the state not being `Discharging`, and it is not
written because it has not been reproduced on this machine. Writing an unverifiable workaround is
the thing this session has spent two ADRs arguing against. If the pill is seen snapping to 0% while
plugged in, that is the fix and it is known.

**No charge threshold in the payload.** `charge_control_end_threshold` is a sysfs file UPower does
not expose, so a config can say "charge limit reached" but not "charge limit 70%". The states cover
every case that was reported; the number is one `read_attr` away if a config asks for it.

**`brightness` has the same watch shape and keeps it.** Its `backlight` udev watch was confirmed
firing when it was written (`udevadm monitor --udev --subsystem-match=backlight` while changing the
level), and logind is the write path, so nothing about it rests on the assumption that broke here.
