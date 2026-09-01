# The battery poll is a floor, not a fallback

Amends build-steps.md line 98's `power_supply` udev watch. That watch is still primary and still
correct; what changes is that `run_battery_task` no longer treats it as sufficient.

Reported from the bar: the battery pill's percentage and its fill both sat still while the machine
discharged, and the tooltip's charging/discharging line sat still with them.

## Decision 1: the timer runs alongside the udev watch, not instead of it on failure

`run_battery_watch_loop` and `run_battery_poll_loop` are one `run_battery_loop`. It selects over the
udev fd and a `POLL_INTERVAL` ticker, and both wakeups funnel into the same read-and-compare, so the
push-on-change filter that kept the watch quiet keeps the timer quiet too. A watch that fails to
build, or whose fd errors mid-run, drops the loop to the timer alone rather than switching to a
different function.

**The assumption that broke.** The old shape read the state only when udev said something happened,
and fell back to a 30s poll only when `build_power_supply_watch` failed to stand up. That assumed a
watch which builds is a watch which fires. Measured on this dev machine while discharging, with
`udevadm monitor --udev --subsystem-match=power_supply` running alongside a sampler:

| | |
| --- | --- |
| `capacity` over the window | 69, then 65 |
| `power_supply` uevents delivered | 0 |
| UPower's own view, same battery | tracked every point, "updated: 13 seconds ago" |

The kernel's ACPI battery driver emits a uevent on a plug or an unplug and, on this hardware, on
nothing else. So `charging` was live and `percent` was frozen between plug events, which is exactly
the report: the pill only moved when the cable did.

30s is UPower's own cadence for a battery that needs polling, which makes it the number to match
rather than a number to pick.

## Decision 2: this stays a sysfs reader; UPower is the open question, not the fix

Quickshell does not read sysfs at all. `src/services/upower/core.cpp` calls
`org.freedesktop.UPower.GetDisplayDevice()` and `device.cpp` binds that path's properties through
`DBusPropertyGroup`, which does one `GetAll` and then follows `PropertiesChanged`. There is no timer
anywhere in it. It works because UPower is the one doing the polling, and the client inherits its
cadence for free.

Taking that route here is a real option and this ADR does not take it. It would add what UPower
carries and sysfs does not: a seven-value `state` where § 2.2 has a `charging` boolean (this machine
reports `pending-charge` right now, which the boolean flattens to "not charging"), `TimeToEmpty` and
`TimeToFull`, the aggregate across two batteries, and the vendor quirks UPower already handles. The
precedent is in the tree: `power/controller.rs` already proxies `org.freedesktop.UPower` for
`on_battery` and `energy_rate`, and already degrades with a printed warning when the daemon is
absent.

What holds it back is that it changes § 2.2's payload and makes `oblisk.battery` need a daemon that
`/sys/class/power_supply` does not. That is a spec decision. This ADR is the smaller one: with the
timer as a floor, the reported bug is fixed whether or not UPower is running, and the UPower move
becomes a question about what the payload should carry rather than a question about whether the
number updates.
