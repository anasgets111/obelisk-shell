-- BrightnessService.qml is a service, not a bar file; this mirrors no single Quickshell file.
--
-- Brightness, and the first § 3.2 command with an argument in it. `oblisk.lock:invoke("lock")`
-- in `session.lua` proves the envelope; this proves the rest of the write path: the arguments
-- array, the round trip back through udev, and the revision the envelope is stamped with (Phase 25
-- item 2).
-- The click reads `percent` off the last snapshot, steps it, and the number that comes back is
-- whatever logind actually wrote, not what this config asked for.
--
-- It wraps to 10 rather than to 0. A demo that can black the panel out with one stray click is a
-- demo nobody clicks twice, and `brightness:set(0)` on this machine's `intel_backlight` does
-- exactly that.
--
-- Left steps up, right steps down, which is `on_click`'s second argument doing the only job it has
-- (docs/adr/0050's second amendment). A wheel would be the obvious control and there is no
-- `on_scroll`; this is what the pointer can express today.
--
-- Reads "--" forever on a machine with no backlight, deliberately: § 2.3 specifies no absence
-- sentinel, so the capability pushes nothing at all rather than fabricating a `0` that a config
-- could not tell from a screen turned all the way down (docs/adr/0053).
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")
local meter = require("components.meter")

local BRIGHTNESS_STEP = 10

local brightness_module = pill({
    button {
        width = 58,
        height = 18,
        align_v = "Center",
        on_click = function(_, button)
            local b = oblisk.brightness:get()
            if b == nil then
                return
            end
            local stepped = b.percent + (button == "right" and -BRIGHTNESS_STEP or BRIGHTNESS_STEP)
            if stepped > 100 then
                stepped = BRIGHTNESS_STEP
            elseif stepped < BRIGHTNESS_STEP then
                stepped = 100
            end
            oblisk.brightness:invoke("set", stepped)
        end,
        children = { cell(util.label(oblisk.brightness, function(b)
            return string.format("sun %d%%", b.percent)
        end), theme.FG) },
    },
    meter(oblisk.brightness, function(b)
        return b.percent
    end, theme.YELLOW),
})

return brightness_module
