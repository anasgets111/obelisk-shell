# Desktop entries are an enumerated capability, not a lookup call

ADR-0054 decision 5 declined to build the `app_id` half of § 3.2's
`system:find_icon(app_id, name, fallback)`, on the grounds that the theme-name half had moved into
the Renderer and left the function with nothing to be asked for. It closed with the condition for
reopening: "It goes in the day something needs an icon for a window that is not already telling us
its icon."

Three callers arrived at once, and the config had already worked around their absence.

- An application launcher needs every entry's display name, icon and command.
- A focused-window readout has `workspaces.active_client.class`, an `app_id`, and no icon.
- A tray item can register with neither an `IconName` nor an `IconPixmap`, leaving § 2.5's
  `icon_name`/`icon_path` both null and the bar drawing a truncated string.

The workaround is what forced this. `dev-config/oblisk/modules/launcher/apps.lua` shelled out to
`process.run("sh", {"-c", "grep -H -E '^(Name|Exec|Icon|NoDisplay|Hidden|Type)=' ..."})` and parsed
`Exec=` in Lua. It worked, and every way it was wrong was invisible: it read `[Desktop Action ...]`
groups as if they were the application, so an entry with a "New Window" action could launch the
wrong command; it split `Exec=foo "a b"` into three arguments; and it had no path to a
`Terminal=true` entry at all.

## Decision 1: a snapshot capability, not the `find_icon` signature

ADR-0054's objection to putting the resolver in the Supervisor was specific and it still stands: the
control socket carries one-way commands one way and one-way `StateSnapshot`s the other, with no
correlation id and no reply, so a *synchronous* `find_icon` returning a path has no transport.

That objection does not reach this data. Desktop entries are not a per-call lookup; they are a set
that changes when packages are installed and at no other time. That is snapshot-shaped, exactly like
`tray`'s items or `updates`' package list, and it needs no protocol that does not already exist.
`oblisk-idl-api-specs.md`'s own `system:find_icon` row reached this conclusion before this ADR did:
the launcher "needs `app_id` to display name and icon for every entry, which is desktop-entry
enumeration rather than a per-`app_id` lookup, so it does not want this row's signature."

So `applications` joins `shared::CAPABILITIES` and pushes `{ entries, by_app_id }`. The Renderer
needs no change at all: `json::to_lua` already converts any payload, and `socket.rs` seeds every
roster name onto `oblisk`.

This is an amendment to ADR-0054 decision 5, not a reversal. That decision was right that nothing
wanted a synchronous per-`app_id` call, and it is still right: nothing does. What it did not
anticipate is that the same underlying data wanted a different shape.

## Decision 2: `by_app_id` repeats the entries rather than indexing into them

The payload carries a sorted `entries` array for the launcher and a `by_app_id` map for the other
two callers. The map's values are whole entries, duplicated, not indices into the array.

An index would have to be an array index, and the JSON array counts from zero while the Lua table it
becomes counts from one. Every config reading it would carry an off-by-one that is invisible in the
payload and wrong by exactly one row. Three small fields repeated across a few hundred entries is
cheaper than that, and the alternative that avoids both (an id-keyed object plus a separate order
array) costs the launcher its sort order for a saving nothing has measured.

`app_id` matching runs in two passes: exact `StartupWMClass` and exact desktop file id first, then
case-folded spellings and the last dot-segment of a reverse-DNS id. Two passes rather than one is the
whole reason it is a function: built entry by entry, one entry's case-folded guess could displace
another entry's exact filename, and which one won would depend on the order the filesystem handed
back its directory entries.

## Decision 3: the argv never crosses into Lua

`entries` carries `id`, `name` and `icon`. It does not carry `Exec`. Launching is
`applications:launch(id)`, and the parsed command line stays in a map on the Supervisor's side.

Two reasons, and the second is the one that decided it. A config that could read an argv is a config
that could assemble a different one before handing it back to be run, which turns a launcher into a
way to run anything with the Supervisor's privileges through a `shell.lua` that only had to be
tricked once. And `process.run` is the wrong lifecycle regardless: it pipes stdout and stderr and
holds the `Child` for its exit code (ADR-0026), so a launched GUI application would keep two pipes
and a handle alive for its whole life, and a generation swap would reap it. Opening an editor and
then editing the config would close the editor.

## Decision 4: rescan on demand, not on a watch

The scan runs at startup and whenever `applications:refresh()` is called. Nothing watches
`/usr/share/applications`.

An inotify watch is affordable (the crate is already a dependency) and it is still the wrong trade.
`watcher.rs` is built for the config tree, with recursive descent, content hashing and a `.lua`
filter, so reuse means generalizing an ADR-0047-governed file for a second consumer with different
semantics. A second watcher means a second copy of that machinery held open all session for an event
that fires a handful of times a month. Meanwhile the config already knows the one moment the list is
about to be read, and `dev-config`'s launcher button calls `refresh` as it opens.

The scan pushes only when the result differs from the last one, so a config that refreshes on every
open does not repaint the shell every open. Without that filter it would: every `StateSnapshot` marks
the scene dirty and drives a full re-resolve and repaint (ADR-0044).

## What this costs, named rather than absorbed

**No locale.** `Name[de]` is skipped and `Name` is always the unlocalized value, so the launcher
reads English on a localized system. Half-doing it would be worse than not doing it: matching `ll`
but not `ll_CC` silently prefers the wrong regional variant. About a dozen lines when someone runs
this in a locale that has translations.

**No `OnlyShowIn`/`NotShowIn`.** Entries meant for one desktop environment are listed everywhere.
Honouring them needs `$XDG_CURRENT_DESKTOP`, and the failure mode is a handful of extra rows rather
than a wrong one.

**`Terminal=true` needs `$TERMINAL` and refuses without it.** There is no specified way to find a
terminal emulator. Probing `PATH` against a list of known ones is the upgrade; it is not the starting
point because a guess that picks the wrong emulator is worse than a refusal naming the variable to
set.

**A token mixing a field code with other text keeps the text.** `--file=%f` becomes `--file=`.
Dropping the whole token is right for that shape and wrong for a code inside a longer string, and the
launcher passes no files, so neither spelling is reachable today.

**The scan is not incremental.** Every `refresh` reads every entry. On this machine that is 290
matching lines across two directories, and it runs on a `spawn_blocking` thread, so the cost lands
off the async runtime and off the render thread both. Caching by directory mtime is the upgrade if a
system with thousands of entries ever makes it visible.
