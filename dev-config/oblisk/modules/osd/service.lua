-- The OSD as OSDService.qml has it: a change somewhere on the system puts a card on screen for two
-- seconds saying what changed. Driven by pushes (ADR-0115), not by bar clicks, so a volume key, a
-- `wpctl` in a terminal and the bar button all show the same card. Before `on_change` only the bar
-- button could, and the OSD was mostly a way to watch yourself click.
--
-- Two shapes, decided by `level`: a level (volume, brightness, keyboard backlight) draws a glyph,
-- a track and a percentage; a fact (a toggle, a device, a layout, a charger) draws the glyph in a
-- tile and a line of text. `modules/osd/popup.lua` draws them and knows nothing else.
--
-- Not the mirror's queue. It holds a pending entry to show after the current one, and a suppress
-- list per kind; here a card that arrives while a more important one is up is dropped, and one
-- that is as important or more replaces it. That is enough for the case the list existed for: the
-- brightness step the charger edge triggers arrives while "charger connected" is up, and is not
-- what you want to read.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")

local osd = {}

-- The mirror's `prio`, lower first. Kinds not listed are the least important.
local PRIORITY = { battery = 0, audio_device = 1, networking = 2, bluetooth = 2, volume = 3, brightness = 3 }

osd.entry = state("osd_entry", { kind = "", glyph = "", text = "" })
osd.visible = state("osd_visible", false)

-- `process.run("sleep", ...)` is this engine's only timer. `ProcessHandle:kill()` does not cancel the
-- queued `exit_cb`, so a newer card while the older sleep is still running is told apart by a
-- request counter rather than a kill, or the older timer would hide the newer card.
local SECONDS = "2"
local request = 0

-- `entry`: `{ glyph, text, level?, color? }`. `level` in 0..100 selects the track layout.
function osd.show(kind, entry)
    local current = osd.entry:get()
    if osd.visible:get() and current.kind ~= kind and (PRIORITY[kind] or 9) > (PRIORITY[current.kind] or 9) then
        return
    end
    entry.kind = kind
    osd.entry:set(entry)
    osd.visible:set(true)
    request = request + 1
    local this_request = request
    process.run("sleep", { SECONDS }, function() end, function()
        if this_request == request then
            osd.visible:set(false)
        end
    end)
end

-- `showToggle`: "<what> on" / "<what> off".
local function toggle(kind, on, glyph_on, glyph_off, what)
    osd.show(kind, { glyph = on and glyph_on or glyph_off, text = what .. (on and " on" or " off") })
end

local function active_name(devices)
    for _, device in ipairs(devices or {}) do
        if device.active then
            return device.name
        end
    end
end

-- Every handler below skips `previous == nil`, the first push after start: the mirror's
-- `initialized` timer, for the same reason -- nothing changed, the shell just learned the state.

oblisk.audio:on_change(function(a, previous)
    if previous == nil then
        return
    end
    local percent = math.floor((a.volume or 0) * 100 + 0.5)
    if a.muted ~= previous.muted or percent ~= math.floor((previous.volume or 0) * 100 + 0.5) then
        osd.show("volume", {
            glyph = util.volume_glyph(a),
            text = a.muted and "muted" or string.format("%d%%", percent),
            level = a.muted and 0 or percent,
            color = theme.ACCENT,
        })
    end
    local sink = active_name(a.sinks)
    if sink and sink ~= active_name(previous.sinks) then
        osd.show("audio_device", { glyph = icons.speaker, text = sink })
    end
end)

oblisk.brightness:on_change(function(b, previous)
    if previous and b.percent ~= previous.percent then
        osd.show("brightness", {
            glyph = icons.brightness,
            text = string.format("%d%%", b.percent),
            level = b.percent,
            color = theme.YELLOW,
        })
    end
end)

oblisk.network:on_change(function(n, previous)
    if previous and n.networking_enabled ~= previous.networking_enabled then
        toggle("networking", n.networking_enabled, icons.lan, icons.lan_off, "networking")
    end
end)

oblisk.bluetooth:on_change(function(b, previous)
    if previous and b.enabled ~= previous.enabled then
        toggle("bluetooth", b.enabled, icons.bt_on, icons.bt_off, "bluetooth")
    end
end)

oblisk.notifications:on_change(function(n, previous)
    if previous and n.dnd ~= previous.dnd then
        toggle("dnd", n.dnd, icons.bell_off, icons.bell, "do not disturb")
    end
end)

oblisk.keyboard:on_change(function(k, previous)
    if previous == nil then
        return
    end
    if k.active_layout ~= previous.active_layout and k.active_layout ~= "" then
        osd.show("layout", { glyph = icons.keyboard, text = "layout: " .. k.active_layout })
    end
    if k.caps_lock ~= previous.caps_lock then
        toggle("locks", k.caps_lock, icons.caps_lock, icons.caps_lock, "caps lock")
    end
    if k.num_lock ~= previous.num_lock then
        toggle("locks", k.num_lock, icons.num_lock, icons.num_lock, "num lock")
    end
    if k.scroll_lock ~= previous.scroll_lock then
        toggle("locks", k.scroll_lock, icons.keyboard, icons.keyboard, "scroll lock")
    end
    if k.backlight_pct >= 0 and k.backlight_pct ~= previous.backlight_pct then
        osd.show("backlight", {
            glyph = icons.keyboard,
            text = string.format("%d%%", k.backlight_pct),
            level = k.backlight_pct,
            color = theme.YELLOW,
        })
    end
end)

return osd
