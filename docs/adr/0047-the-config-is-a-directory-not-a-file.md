# The config is a directory, not a file

`Loader::evaluate_file` reads one `shell.lua`. `supervisor/src/watcher.rs` adds a single non-recursive
`inotify` watch on `~/.config/oblisk/` and fires on that one filename. Nothing else is read and
nothing else is watched.

`require` is available, because mlua's `Lua::new()` loads `StdLib::ALL_SAFE`, which includes the
`package` library. So a config can already split itself across files today, and doing so breaks
reloading silently: `package.path` points at the system Lua tree rather than the config directory, and
editing a required file triggers no reload because the watcher only looks at `shell.lua`.

One file is not the shape for a config language whose stated goal is any app the user wants.
Quickshell's `QmlScanner` walks the whole config tree, hashes every file, and watches all of them.

## Decision 1: `package.path` points at the config directory and nothing else

Set it to the config directory's `?.lua` and `?/init.lua`, replacing the default rather than
prepending to it. A config's `require "widgets.clock"` resolves inside the config; it never picks up
a same-named module from `/usr/share/lua/5.4/`.

Replacing rather than prepending means installed Lua libraries are unreachable. That is the right
default for a shell config, and the upgrade path if someone genuinely wants luarocks is to append a
user-declared path list, not to restore the system default silently.

C modules need no decision: mlua's safe mode already replaces the C searchers and makes
`package.loadlib` raise. Record it so nobody reaches for `Lua::unsafe_new` to fix an unrelated
problem, since loading a `.so` into the Renderer would discard the memory-safety argument the whole
process boundary exists for.

## Decision 2: clear `package.loaded` before every re-evaluation

ADR-0044 decision 4 keeps one Lua VM per generation and does not reset it on an in-place reload.
`require` caches by module name in `package.loaded`. Together those two mean editing a required
module would re-run `shell.lua` against the *old* cached module and change nothing on screen, which
looks exactly like a reload that silently did not happen.

Clear the non-standard entries from `package.loaded` immediately before each re-evaluation. This is
the one place the two ADRs interact, and neither is wrong on its own, which is why it is written down
rather than left to be discovered.

## Decision 3: watch the directory tree, and gate on a content hash

Watch the config directory recursively instead of one filename. Keep a `path -> hash` map, refreshed
on every evaluation, and drop any `inotify` event whose file hashes the same as last time.

Hashing is not a duplicate of the existing debounce. Debouncing collapses the burst of events one
save produces; hashing rejects saves that changed no bytes at all, which editors produce routinely
through write-truncate-rewrite cycles and swap-file churn. Quickshell carries both for the same
reason (`QmlScanner::hasFileContentChanged`).

Tracking which files `require` actually loaded was the tighter alternative and is rejected. That set
is only known after a *successful* evaluation, so the first broken config leaves nothing to watch and
no way to recover by editing. Watching the tree works before the first successful load, which is
exactly when a config author needs it.

Only `.lua` files trigger a reload. A README, a shell script, or an editor swap file in the config
directory is not a config change.

## Consequences

Lua's own module cache gives per-module singletons for free: a module that returns a table is
evaluated once per VM and every `require` of it gets the same table. Quickshell needed a `Singleton`
type and a `SingletonRegistry` because QML has no equivalent. Oblisk needs neither, and decision 2 is
what keeps that free behavior correct across reloads.

`shared::shell_lua_path` stays the entry point. `supervisor/src/watcher.rs`'s single `add()` becomes a
recursive walk, and its debounce stays as is.

ADR-0045's per-parent `id` scoping is what makes a required module reusable: a widget module
instantiated twice carries the same internal ids in both copies without colliding.
