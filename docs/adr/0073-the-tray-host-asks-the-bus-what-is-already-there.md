# The tray host asks the bus what is already there

A tray item registers with the watcher once, when the application starts. Restart the shell and the
watcher is new and empty, and the spec's answer is `StatusNotifierHostRegistered`: the watcher emits
it, and an application is supposed to hear it and register again.

Slack does not. Restarting Oblisk lost its icon until Slack itself was restarted, which is not a
thing a shell gets to ask for.

## Decision: scan `ListNames` at startup and register what is found

`TrayController::new` calls `ListNames`, keeps the names matching
`org.{kde,freedesktop}.StatusNotifierItem-`, and puts each through the same
`resolve_registration` plus `register_item` path a live `RegisterStatusNotifierItem` call takes.
Both spellings, because KDE's is the de-facto name and Chromium claims the `org.freedesktop` one.

The signal stays. This is the belt to its braces, not a replacement: an application that does
re-register still does, and lands on the same registry key and overwrites its own entry.

Serial, not joined. Each registration is a handful of property reads and an optional `GetLayout`,
and a session has a handful of tray items, against a Supervisor that has already made several round
trips by this point.

## What this cannot find

Only items that claimed a well-known name. An application that called
`RegisterStatusNotifierItem("/some/object/path")` and owns no `StatusNotifierItem-*` name is
invisible to the scan, because nothing on the bus says which connections export the interface
without asking each one in turn. Vesktop is exactly that shape, and does not need this: it listens
for the watcher and re-registers on its own.

So the scan covers the applications that need covering and misses the ones that do not, which is
luck rather than design, and worth saying out loud. The upgrade path is introspecting every
connection on the session bus, which is dozens of round trips at startup to look for something
usually not there.

## The race is the one that was already there

Adoption runs before `spawn_name_owner_changed_forwarder`, and reordering them would not help: that
forwarder subscribes inside its own spawned task, so nothing here can guarantee the match rule is
installed before the first adoption finishes. An item that disconnects mid-adoption is caught by
`register_item`'s existing pre-insert liveness check, the same guard that already covers the
identical window for a live registration. It fired during the very run that verified this change.

## Verified

With Slack running and started before the shell:

```
tray: RequestName(org.kde.StatusNotifierWatcher) -> PrimaryOwner
tray: adopted org.freedesktop.StatusNotifierItem-1273902-1, registered before this host started
```

and its 22x22 pixmap spooled alongside Vesktop's, with no application restarted.

`is_item_bus_name` is the pure half and carries the tests: both spellings match, the watcher's own
name and `StatusNotifierHost-1234` do not, and neither does a name that merely starts the same way.
The trailing `-` is the whole guard, and without it every one of those matches.
