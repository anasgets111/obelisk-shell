-- Development bar. Exercises the layout engine and every rostered capability signal against a
-- real session, which the unit fixtures cannot do: `XDG_CONFIG_HOME=dev-config target/debug/
-- supervisor` boots the whole stack against this file, and editing it while that runs drives the
-- Watcher's in-place reload (Phase 13). Changing a surface's `id`/`layer`/`anchor`/`monitor`
-- instead drives a full PBA generation swap (§ 15.2).
--
-- Paint properties (`background`, `radius`, `border_color`, `border_width`, `foreground`) parse
-- but do not draw yet: Phase 19 item 6's third commit is what walks the resolved tree into
-- FemtoVG. They are written here now so that commit has a real config to prove itself against.

local BG = "#1e1e2eff"
local FG = "#cdd6f4ff"
local DIM = "#6c7086ff"
local ACCENT = "#89b4faff"

-- `content` takes a string, and a capability signal's value is nil until the Supervisor's first
-- snapshot for it arrives. Every reader below goes through this: it fixes the nil case once, and
-- keeps the per-capability closures to the one line that actually reads the payload.
local function label(signal, read)
    return signal:map(function(value)
        if value == nil then
            return "--"
        end
        local ok, text = pcall(read, value)
        if not ok then
            return "!"
        end
        return text or "--"
    end)
end

local function cell(content, color)
    return text {
        content = content,
        foreground = color or FG,
        font_size = 13,
        margin = { left = 6, right = 6 },
    }
end

local function count(list)
    return list and #list or 0
end

local clock = cell(label(sysinfo, function(s)
    -- sysinfo stays dormant until a poll interval is configured, which needs the Lua write path
    -- (Phase 25). Until then this cell reads "--" on a live session, by design, not by breakage.
    return string.format("cpu %d%%  ram %d%%", s.cpu_pct or 0, s.ram_pct or 0)
end), DIM)

local net = cell(label(network, function(n)
    for _, ap in ipairs(n.available_networks or {}) do
        if ap.active then
            return string.format("%s %d%%", ap.ssid, ap.strength or 0)
        end
    end
    return "offline"
end), ACCENT)

local bt = cell(label(bluetooth, function(b)
    if not b.enabled then
        return "bt off"
    end
    return string.format("bt %d", count(b.connected_devices))
end))

local kbd = cell(label(keyboard, function(k)
    local name = k.active_layout or "?"
    if k.caps_lock then
        name = name .. " CAPS"
    end
    return name
end))

local media = cell(label(mpris, function(m)
    local player = (m.players or {})[1]
    if not player then
        return "no media"
    end
    local mark = player.play_state == "Playing" and ">" or "||"
    return string.format("%s %s", mark, player.title or player.identity or "?")
end), ACCENT)

local sound = cell(label(audio, function(a)
    return string.format("%d streams", count(a))
end), DIM)

local tray_cell = cell(label(tray, function(t)
    return string.format("tray %d", count(t.items))
end), DIM)

local function notification_summary(n)
    if n.dnd then
        return "dnd"
    end
    return string.format("notif %d", count(n.feed))
end

-- Two cells, not one node table shared by both surfaces: a node is identified by its position in
-- one parent's child list (docs/adr/0045), so handing the same table to two trees is asking two
-- parents to reconcile against one identity.
local notifs = cell(label(notifications, notification_summary))
local notification_feed = cell(label(notifications, notification_summary))

-- Only rendered when the config itself has failed, so `rescue` is the one signal whose absence is
-- the healthy case (§ 2.10).
local rescue_cell = cell(label(rescue, function(r)
    if r.error_log == nil or r.error_log == "" then
        return nil
    end
    return "config error"
end), "#f38ba8ff")

-- Two surfaces, because one surface cannot catch a whole class of bug: the applied-scene log
-- named every surface by its kind (the literal string "surface") until a second one made that
-- visible, and a candidate that fails to build a scene still gets promoted, which only shows up
-- when a config is big enough to get wrong. `notification_area` has no `wl_surface` behind it
-- yet, so it resolves and reconciles without appearing on screen -- Phase 20 item 4 is what
-- deletes `SurfaceRole` and makes the config's ids the same id space the compositor sees.
return {
    surface {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        width = "Fill",
        height = "Fill",
        child = row {
            width = "Fill",
            height = "Fill",
            background = BG,
            padding = { left = 12, right = 12, top = 4, bottom = 4 },
            spacing = 8,
            align_v = "Center",
            children = { clock, net, bt, kbd, media, sound, tray_cell, notifs, rescue_cell },
        },
    },
    surface {
        id = "notification_area",
        layer = "Overlay",
        anchor = { top = true, right = true },
        width = 380,
        child = column {
            spacing = 6,
            -- A container's own padding does not contribute to its content size today, so this
            -- column reports the height of its children alone and the padding pushes them past
            -- its own edge. Left as written rather than worked around: the config is right and
            -- the engine is not.
            padding = { top = 8, right = 10, bottom = 8, left = 10 },
            background = "#181825ee",
            radius = 8,
            border_width = 1,
            border_color = "#313244ff",
            children = { notification_feed },
        },
    },
}
