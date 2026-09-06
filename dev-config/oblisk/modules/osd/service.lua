-- As `OSDService.qml`: system changes show a card for two seconds. Push-driven (ADR-0115), so a
-- volume key, terminal `wpctl`, and bar button use the same card; before `on_change`, only the bar
-- button could trigger it.
--
-- `level` selects the shape: volume/brightness/keyboard backlight use glyph, track, percentage;
-- toggles/devices/layout/charger use a glyph tile and text. `modules/osd/popup.lua` only draws.
--
-- No mirror queue. A less important card arriving during a higher-priority one is dropped; an equal
-- or higher one replaces it. This covers the charger edge's brightness step while "charger
-- connected" is visible, which is not useful to read.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")

local osd = {}

-- Mirror `prio`, lower first. Unlisted kinds are least important.
local PRIORITY = { battery = 0, audio_device = 1, networking = 2, bluetooth = 2, volume = 3, brightness = 3 }

osd.entry = state("osd_entry", { kind = "", glyph = "", text = "" })
osd.visible = state("osd_visible", false)

-- `process.run("sleep", ...)` is the only timer. `ProcessHandle:kill()` does not cancel queued
-- `exit_cb`, so a request counter distinguishes a newer card from the older sleep; killing alone
-- would let the old callback hide the new card.
local SECONDS = "2"
local request = 0

-- `entry` is `{ glyph, text, level?, color? }`; `level` in 0..100 selects the track layout.
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

-- `showToggle`: "<what> on" or "<what> off".
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

-- Every handler skips the first push (`previous == nil`): like the mirror's `initialized` timer, it
-- reports learned state, not a change.

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
