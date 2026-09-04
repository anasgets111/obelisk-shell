-- Development bar. Two jobs, and they pull in opposite directions often enough to be worth naming:
-- it is the fixture that exercises the engine against a real session (`cargo build --workspace &&
-- XDG_CONFIG_HOME=dev-config target/debug/supervisor`), and it is the worked example of what a
-- config for this shell looks like. Where those conflict the fixture wins, and the comment says so.
--
-- `cargo build --workspace` first, and it is not optional. The Supervisor finds the Renderer as a
-- filesystem sibling of its own binary (`supervisor/src/generation.rs`'s `renderer_binary_path`),
-- not as a Cargo dependency, so `cargo run -p supervisor` rebuilds one half of the stack and
-- launches whatever `target/debug/renderer` happens to be. That fails as a *config* error: a
-- Renderer older than the `fonts` global reports `attempt to call a nil value (global 'fonts')` at
-- the declaration below and points at this file, which is the last place the fault actually is.
--
-- Laid out like a bar people actually run, because it is copied from one: the zones and the order
-- of the modules in them are `~/.config/quickshell`'s, down to the clock sitting last on the right
-- rather than centred. That shape is not decoration. It is what found ADR-0053: writing it
-- required a clock, a battery and a volume readout, and none of the three had a data source until
-- that ADR.
--
-- Editing this file while the stack runs drives a reload. Changing `id`/`layer`/`anchor`/`monitor`/
-- `namespace` on a surface is a topology change and drives a full PBA generation swap (§ 15.2);
-- anything else reloads in place on the same Lua VM.

-- What this file imports, and where each thing lives. The tree mirrors the Quickshell config this
-- shell is written to replace, directory for directory: `config/` holds design tokens,
-- `components/` holds dumb reusable widgets, `lib/` holds functions with no node in them, and
-- `modules/` holds feature assemblies -- `bar/` with its `indicators/` and `panels/`, `global/`
-- for the surfaces that are not the bar, `notification/`, `osd/`, and `shell/` for the one host
-- that puts a panel on screen.
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
-- `return { require(...), require(...) }` puts one more element in this list than it has surfaces,
-- a string like "/path/to/lock.lua", and the engine then reports `error converting Lua string to
-- table` with no clue which entry is wrong. `local x = require(...)` takes the first value and nothing else.
-- The font chain, in fallback order, and it has to be declared before anything measures text.
-- `femtovg` and `cosmic-text` both fall back across it per glyph, so one chain covers body text and
-- the Nerd Font private-use glyphs the Quickshell config draws its whole chrome with: the codepoint
-- picks the face, not the node. Without this the engine resolves `sans-serif` and those glyphs
-- render as tofu, which is what they did until the `fonts` declaration existed (ADR-0043
-- decision 2).
--
-- Read once, at startup. Editing this list re-evaluates and changes nothing until the shell is
-- restarted; `renderer/src/lua/fonts.rs` says why.
fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans CJK JP",
    "Noto Color Emoji",
}

local wallpaper = require("modules.global.wallpaper")
local bar = require("modules.bar")
local notifications = require("modules.notification.popup")
local osd = require("modules.osd.popup")
local settings = require("modules.bar.panels.settings")
local panel_host = require("modules.shell.panel_host")
local launcher = require("modules.global.launcher")
-- A tooltip is a surface of its own, so each is listed here rather than nested in the bar: a
-- `popup` is an `xdg_popup` rooted under the bar, not a node inside it (§ 6.3, ADR-0062).
-- They cost nothing until hovered -- a popup with `visible = false` creates no Wayland object.
local battery_tooltip = require("modules.bar.indicators.battery").tooltip
local clock_tooltip = require("modules.bar.indicators.date_time").tooltip
local launcher_tooltip = require("modules.bar.indicators.launcher_button").tooltip
-- New with the icon-only bar, and not decoration. A circle with a wifi glyph in it says how strong
-- the signal is and nothing about which network, which is fine on the bar and useless without
-- somewhere to read the rest -- so the two indicators that lost their labels grew a tooltip each.
local network_tooltip = require("modules.bar.indicators.network").tooltip
local bluetooth_tooltip = require("modules.bar.indicators.bluetooth").tooltip
local lock_screen = require("modules.global.lock")
local polkit_dialog = require("modules.global.polkit")
-- Not a surface: the battery's side effects (OSD lines, low-battery notifications, suspend), which
-- only need to be registered once. Required for that, and returns nothing to list below.
require("modules.global.power_events")

return {
    wallpaper,
    bar,
    notifications,
    osd,
    settings,
    panel_host,
    battery_tooltip,
    clock_tooltip,
    launcher_tooltip,
    network_tooltip,
    bluetooth_tooltip,
    launcher,
    lock_screen,
    polkit_dialog,
}
