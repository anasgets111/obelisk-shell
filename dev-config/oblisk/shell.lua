-- Development bar. Exercises the layout engine and every rostered capability signal against a
-- real session, which the unit fixtures cannot do: `XDG_CONFIG_HOME=dev-config target/debug/
-- supervisor` boots the whole stack against this file, and editing it while that runs drives the
-- Watcher's in-place reload (Phase 13). Changing a surface's `id`/`layer`/`anchor`/`monitor`
-- instead drives a full PBA generation swap (§ 15.2).
--
-- Every field below now decides what the compositor sees (Phase 20, ADR-0038): editing a `layer`
-- restacks the surface, a `namespace` is what a Hyprland `layerrule` matches on, and `exclusive`
-- reserves screen area. Changing `id`/`layer`/`anchor`/`monitor`/`namespace` is a topology change
-- and drives a full PBA generation swap; changing `margin`/`keyboard_interactivity`/`exclusive`/
-- a size is a value change and reloads in place.

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

-- Phase 21 item 1's live proof: a `button` whose `on_click` changes what a `text` paints, which is
-- the whole point of routing pointer events at all.
--
-- The counter is a `state` signal (ADR-0044 decision 5), so the handler's `:set()` is what marks
-- the scene dirty and drives the repaint -- nothing outside the config guesses that a click
-- changed something. The name is also what survives an in-place reload: edit a colour below while
-- this is running and the count keeps going instead of resetting to 0, because `state("clicks", 0)`
-- finds the signal it built last time and ignores the new initial.
--
-- `rect` is the button's own rect in this surface's logical coordinates (ADR-0050 decision 3) --
-- Phase 22's `popup` is what really wants it; this prints it so a live session can check it against
-- where the button is actually drawn.
local clicks = state("clicks", 0)

-- The anchor rect the `popup` below hangs from. ADR-0049's amendment settles where it comes from:
-- not off the input-dispatch stack (the re-resolve runs after `dispatch_pending` has returned) but
-- through the config, because `on_click` receives the button's own rect (ADR-0050 decision 3) and
-- writes it to a named `state` signal the `popup` reads back. Both halves are wired here so the
-- commit that actually creates the `xdg_popup` has nothing left to connect.
--
-- The initial is the button's declared size, so the popup is well-formed before anything has ever
-- been clicked: `anchor_rect` must be non-zero or the whole evaluation fails (§ 6.3).
local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 86, height = 24 })

local click_button = button {
    width = 86,
    height = 24,
    background = "#313244ff",
    radius = 4,
    on_click = function(rect)
        local n = clicks:get() + 1
        clicks:set(n)
        popup_anchor:set(rect)
        print(string.format("[shell.lua] click %d, button rect %.0f,%.0f %.0fx%.0f", n, rect.x, rect.y, rect.width, rect.height))
    end,
    children = { cell(clicks:map(function(n)
        return string.format("clicks %d", n)
    end), ACCENT) },
}

-- Only rendered when the config itself has failed, so `rescue` is the one signal whose absence is
-- the healthy case (§ 2.10).
local rescue_cell = cell(label(rescue, function(r)
    if r.error_log == nil or r.error_log == "" then
        return nil
    end
    return "config error"
end), "#f38ba8ff")

-- Two surfaces, because one surface cannot catch a whole class of bug: the applied-scene log
-- named every surface by its kind (the literal string "panel") until a second one made that
-- visible, and a candidate that fails to build a scene still gets promoted, which only shows up
-- when a config is big enough to get wrong. Both get a real `zwlr_layer_surface_v1` now, one per
-- monitor each, addressed as `"bar@{output}"` and `"notification_area@{output}"`. The `window` and
-- `popup` after them are declarations only -- see their own comment.
return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        -- Reserves 32px of screen area along the anchored (top) edge. The zone is derived from
        -- the height the compositor actually configures, so it stays right if this changes.
        exclusive = true,
        -- Not what a real bar wants, and deliberate: this file is a fixture that exercises the
        -- engine, so it opts into Phase 21 item 2's focus path rather than leaving it dark. The
        -- cost is that clicking the bar takes keyboard focus off the window behind it.
        keyboard_interactivity = "OnDemand",
        width = "Fill",
        height = 32,
        child = row {
            width = "Fill",
            height = "Fill",
            background = BG,
            padding = { left = 12, right = 12, top = 4, bottom = 4 },
            spacing = 8,
            align_v = "Center",
            children = { clock, net, bt, kbd, media, sound, tray_cell, notifs, click_button, rescue_cell },
        },
    },
    panel {
        id = "notification_area",
        layer = "Overlay",
        anchor = { top = true, right = true },
        -- Anchored to one corner, so it reserves nothing and floats over whatever is behind it.
        margin = { top = 12, right = 12 },
        -- Both axes explicit, and they have to be: layer-shell only lets the compositor pick an
        -- axis whose two edges are both anchored, and this surface anchors one corner.
        width = 380,
        height = 60,
        child = column {
            spacing = 6,
            padding = { top = 8, right = 10, bottom = 8, left = 10 },
            background = "#181825ee",
            radius = 8,
            border_width = 1,
            border_color = "#313244ff",
            children = { notification_feed },
        },
    },
    -- Two more of § 6's roles, declared but never mapped: nothing creates an `xdg_toplevel` or an
    -- `xdg_popup` yet, so neither gets a surface instance and neither reaches the scene at all.
    -- What they exercise is the half that does exist -- `window_spec`/`popup_spec` run over both on
    -- every evaluation, so a typo in either lands in `rescue.error_log` (§ 2.10) instead of
    -- surfacing as a protocol error the first time something tries to open one. `visible = false`
    -- on both says the same thing the config's way, and is what stays true once they do map.
    window {
        id = "settings",
        title = "Oblisk settings",
        app_id = "oblisk.settings",
        -- Advisory in the spec's own words, and never clamped against the resolved tree (§ 6.2).
        min_size = { width = 320, height = 240 },
        max_size = { width = 1280, height = 800 },
        visible = false,
        child = column {
            padding = { top = 12, right = 12, bottom = 12, left = 12 },
            background = BG,
            children = { cell("settings", FG) },
        },
    },
    popup {
        id = "click_menu",
        -- The `id` of the surface this anchors to, not a node -- the protocol roots a popup under a
        -- parent *surface* at creation (§ 6.3).
        parent = "bar",
        -- Read now rather than bound as a signal: a top-level spec is parsed from the *unresolved*
        -- properties, so `:get()` is what turns the signal into the table § 6.3 wants. The ceiling
        -- that follows -- this is what the last evaluation saw, not what the last click wrote -- is
        -- named in `renderer/src/socket.rs`'s `panel_specs`.
        anchor_rect = popup_anchor:get(),
        -- Required and non-zero on both axes: a popup has no "Fill" (§ 6.3), because there is
        -- nothing for it to fill.
        width = 200,
        height = 120,
        anchor = "BottomLeft",
        gravity = "BottomRight",
        -- Dropdown behaviour, which is also the default -- spelled out because this fixture exists
        -- to exercise the parser, and the protocol's own default is no adjustment at all.
        constraint_adjustment = { "FlipY", "SlideX" },
        offset = { x = 0, y = 4 },
        visible = false,
        child = column {
            padding = { top = 8, right = 10, bottom = 8, left = 10 },
            background = "#181825ee",
            radius = 8,
            border_width = 1,
            border_color = "#313244ff",
            children = { cell(clicks:map(function(n)
                return string.format("opened after %d clicks", n)
            end), ACCENT) },
        },
    },
}
