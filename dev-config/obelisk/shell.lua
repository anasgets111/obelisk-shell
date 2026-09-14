-- Development bar, real-session fixture, and worked example. If they conflict, the fixture wins.
-- `cargo build --workspace && XDG_CONFIG_HOME=dev-config target/debug/supervisor`.
-- Supervisor finds Renderer beside its binary via `supervisor/src/generation.rs`'s
-- `renderer_binary_path`, not through Cargo; `cargo run -p supervisor` can rebuild one half and
-- launch stale `target/debug/renderer`. A Renderer older than `fonts` then reports `attempt to call
-- a nil value (global 'fonts')` at this file.
--
-- Zones and module order copy `~/.config/quickshell`, including the rightmost clock. ADR-0053
-- exposed the need for clock, battery and volume data sources; none had a source until that ADR.
--
-- Editing reloads the stack. Changing surface `id`/`layer`/`anchor`/`monitor`/`namespace` changes
-- topology and triggers a full generation swap; other edits reload in place on the same Lua VM.

-- Imports mirror the Quickshell tree. `config/` holds tokens, `components/` dumb reusable widgets,
-- `lib/` node-free functions, and `modules/` assembles `bar/indicators/`, `bar/panels/`,
-- `global/`, `notification/`, `osd/`, and `shell/`'s panel host. There is no `services/`:
-- Quickshell's 25 singleton `*Service.qml` files each own their D-Bus connection, poll loop or
-- socket.
-- Supervisor-owned capabilities push signals on `obelisk`; the config reads `obelisk.audio`, and the
-- data layer is not this config's job.
--
-- `require` resolves only inside this directory and its cache clears before each re-evaluation
-- (ADR-0047), so any file below can reload the bar in place.

-- Bind modules before the return. Lua 5.4 `require` returns the module and loader data, unlike 5.3;
-- a final `require` in a table expands both, adds a path such as "/path/to/lock.lua", and produces
-- `error converting Lua string to table`, with no clue which entry is wrong. Bind it as
-- `local x = require(...)` to keep only the module.
-- Declare the font chain before text measurement. `femtovg` and `cosmic-text` fall back per glyph,
-- so body text and Nerd Font private-use glyphs choose their faces independently; without it, the
-- engine resolves `sans-serif` and the glyphs become tofu (ADR-0043 decision 2).
--
-- Read once at startup. Editing the list changes nothing until restart; see
-- `renderer/src/lua/fonts.rs`.
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
local modal_host = require("modules.global.modal_host")
-- Each tooltip is its own surface, not a bar child: `popup` is an `xdg_popup` rooted under the bar
-- (ADR-0062). `visible = false` creates no Wayland object until hover.
local battery_tooltip = require("modules.bar.indicators.battery").tooltip
local clock_tooltip = require("modules.bar.indicators.date_time").tooltip
local launcher_tooltip = require("modules.bar.indicators.launcher_button").tooltip
local wallpaper_tooltip = require("modules.bar.indicators.wallpaper_button").tooltip
-- Icon-only wifi/Bluetooth indicators show strength; tooltips restore network/device labels.
local network_tooltip = require("modules.bar.indicators.network").tooltip
local bluetooth_tooltip = require("modules.bar.indicators.bluetooth").tooltip
local screen_recorder_tooltip = require("modules.bar.indicators.screen_recorder").tooltip
-- The idle tooltip counts down to the next stage, which the bar has no room to show.
local idle_tooltip = require("modules.bar.indicators.idle_inhibitor").tooltip
-- One glyph carries five update states; the tooltip names the one it is in.
local updates_tooltip = require("modules.bar.indicators.updates").tooltip
local audio_panel = require("modules.bar.panels.audio_panel")
local lock_screen = require("modules.global.lock")
local polkit_dialog = require("modules.global.polkit")
local bluetooth_pairing = require("modules.global.bluetooth_pairing")
-- Not a surface. Registers the battery's OSD, low-battery notification and suspend effects once;
-- it returns nothing to the surface list.
require("modules.global.power_events")
-- Not a surface. Registers the idle clock: one `register_threshold`, one `obelisk.system` handler,
-- and the three actions for an unattended seat. `lib/idle.lua` owns the actions; the bar reads the
-- same facts without requiring this module.
require("modules.global.idle")

return {
    wallpaper.desktop,
    wallpaper.overview,
    bar,
    notifications,
    osd,
    settings,
    panel_host,
    battery_tooltip,
    clock_tooltip,
    launcher_tooltip,
    wallpaper_tooltip,
    network_tooltip,
    bluetooth_tooltip,
    screen_recorder_tooltip,
    idle_tooltip,
    updates_tooltip,
    audio_panel.output_tooltip,
    audio_panel.input_tooltip,
    modal_host,
    lock_screen,
    polkit_dialog,
    bluetooth_pairing,
}
