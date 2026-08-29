-- Development bar. Two jobs, and they pull in opposite directions often enough to be worth naming:
-- it is the fixture that exercises the engine against a real session (`XDG_CONFIG_HOME=dev-config
-- target/debug/supervisor`), and it is the worked example of what a config for this shell looks
-- like. Where those conflict the fixture wins, and the comment says so.
--
-- Laid out like a bar people actually run: three zones, modules grouped into pills, the clock
-- centred. That shape is not decoration. It is what found docs/adr/0053: writing it required a
-- clock, a battery and a volume readout, and none of the three had a data source until that ADR.
--
-- Editing this file while the stack runs drives a reload. Changing `id`/`layer`/`anchor`/`monitor`/
-- `namespace` on a surface is a topology change and drives a full PBA generation swap (§ 15.2);
-- anything else reloads in place on the same Lua VM.

-- What this file imports, and where each thing lives. The layout mirrors the Quickshell config
-- this shell is written to replace: `config/` holds design tokens, `components/` holds dumb
-- reusable widgets, `lib/` holds functions with no node in them, and `modules/` holds feature
-- assemblies grouped by the surface they appear on.
--
-- There is no `services/` directory, and that is the one place the mirror deliberately breaks.
-- Quickshell needs 25 singleton `*Service.qml` files because each one has to own its own D-Bus
-- connection, poll loop or socket. Here every one of those is a capability the Supervisor owns and
-- pushes as a signal on `oblisk`, so the config reads `oblisk.audio` instead of constructing an
-- `AudioService`. The data layer is not missing from this tree; it is not the config's job.
--
-- `require` resolves inside this directory only and the module cache is cleared before every
-- re-evaluation (ADR-0047), so editing any file below reloads the bar in place.

-- Bound to locals first, and that is load-bearing rather than style. Lua 5.4's `require` returns
-- *two* values, the module and the loader data (the file path), where 5.3 returned one. A call in
-- the last position of a table constructor expands to all of its values, so the obvious
-- `return { require(...), require(...) }` puts a seventh element in this list that is the string
-- "/path/to/lock.lua", and the engine then reports `error converting Lua string to table` with no
-- clue which of the six is wrong. `local x = require(...)` takes the first value and nothing else.
local wallpaper = require("modules.global.wallpaper")
local bar = require("modules.bar")
local notifications = require("modules.notification.popup")
local settings = require("modules.bar.panels.settings")
local menu = require("modules.bar.panels.menu")
local lock_screen = require("modules.global.lock")

return { wallpaper, bar, notifications, settings, menu.surface, lock_screen }
