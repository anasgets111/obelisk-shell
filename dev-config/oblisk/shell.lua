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
-- writes it to a named `state` signal the `popup` reads back.
--
-- The initial is the button's declared size, so the popup is well-formed before anything has ever
-- been clicked: `anchor_rect` must be non-zero or the whole evaluation fails (§ 6.3).
local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 86, height = 24 })

-- Whether the `window` below is open. ADR-0049 decision 2: for a `window` (and, next commit, a
-- `popup`) `visible` creates and destroys the Wayland object rather than mapping and unmapping it,
-- and nothing outside this file drives that. The handler's `:set()` marks the scene dirty
-- (ADR-0044 decision 5), the poll loop re-resolves, and the reconcile reads the new value -- so one
-- click builds an `xdg_toplevel` and the next destroys it, with no engine-side toggle anywhere.
local settings_open = state("settings_open", false)

-- Whether the `popup` below is open. Set true by the click, and back to false by the popup's own
-- `on_dismiss`, which is the shape a dropdown wants: the compositor is what closes it, on a click
-- anywhere outside it, and that is the whole reason ADR-0040 reached for a real popup instead of a
-- second panel.
--
-- Deliberately not toggled here the way `settings_open` above is, and the reason is measured rather
-- than assumed. Under an `xdg_popup` grab, niri still delivers a click on *this* button to us,
-- because the bar is the popup's own parent surface and so inside the grab's tree -- so a toggle
-- would close it, on this compositor. What a toggle would also do is fight `on_dismiss` on every
-- click that lands anywhere else, since that path already writes `false`. One writer per edge.
local menu_open = state("menu_open", false)

local click_button = button {
    width = 86,
    height = 24,
    background = "#313244ff",
    radius = 4,
    on_click = function(rect)
        local n = clicks:get() + 1
        clicks:set(n)
        popup_anchor:set(rect)
        settings_open:set(not settings_open:get())
        menu_open:set(true)
        print(string.format("[shell.lua] click %d, button rect %.0f,%.0f %.0fx%.0f, settings %s", n, rect.x, rect.y, rect.width, rect.height,
            settings_open:get() and "open" or "closed"))
    end,
    children = { cell(clicks:map(function(n)
        return string.format("clicks %d", n)
    end), ACCENT) },
}

-- Arms the lock screen below. A bar button is a strange place for one, and it is the only place
-- available: ADR-0052 decision 1 makes locking an ordinary capability command, and an input
-- callback is the only thing that can issue one today. `oblisk.idle` cannot -- a
-- `SupervisorFrame::IdleEvent` reaches the Renderer and stops there, because no Lua-side
-- `register_threshold` callback registry exists to dispatch it to, so the idle threshold a real
-- config would lock on has nowhere to land yet.
--
-- `invoke` is the one generic write path (build-steps.md Phase 25 item 1): it builds § 7.2's
-- envelope from the capability, the action and the arguments, and knows nothing about locking. The
-- day `sysinfo:configure` and `audio:set_volume` land, they land on this same call rather than on
-- twenty-nine more bindings.
--
-- Nothing here refuses to lock without a lock screen; that is the engine's job and it does it
-- (ADR-0052 decision 3), reporting the refusal through `rescue` because a refused lock leaves the
-- ordinary scene on the glass and `rescue` is what a bar can render.
local lock_button = button {
    width = 44,
    height = 24,
    background = "#313244ff",
    radius = 4,
    on_click = function()
        oblisk.lock:invoke("lock")
        print("[shell.lua] lock requested")
    end,
    children = { cell("lock", ACCENT) },
}

-- The password field. § 6.4 routes authentication through a `textfield` with `secure_submit`, and
-- that pair is what keeps the password out of this VM entirely: with both `mask_character` and
-- `secure_submit` set, the keystrokes go straight into a native buffer on the Renderer's Wayland
-- thread and leave as a `("lock", "authenticate")` envelope, never as a Lua value at all
-- (§ 5.2 item 8, ADR-0005/ADR-0027). So there is deliberately no `on_change`/`on_submit` handler
-- here: one would be the exact hole the design exists to close.
--
-- One byte is the cap on `mask_character`, so an ASCII `*` rather than a bullet.
--
-- It is the *only* `secure_submit` field on this lock surface, and that is load-bearing rather than
-- incidental: the engine focuses a surface's sole `secure_submit` field the moment the compositor
-- gives that surface keyboard focus, so this one is typable with no click. Adding a second such
-- field here would put the lock screen back to needing a mouse before a password could be typed,
-- because with two destinations the engine refuses to guess which one a keystroke belongs to.
--
-- Measured, so it is not mistaken for breakage on a live lock: a `textfield` paints nothing today
-- (`renderer/src/layout/paint.rs` skips the kind), so this reserves its 28px and swallows the
-- keystrokes while showing no masked characters at all -- `lock_status` below is what shows that
-- typing is landing.
local password_field = textfield {
    width = "Fill",
    height = 28,
    placeholder = "password",
    mask_character = "*",
    secure_submit = { capability = "lock", action = "authenticate" },
}

-- The failure state, off `oblisk.lock` rather than off `rescue`, and the split is ADR-0052
-- decision 4: with the lock surfaces mapped the compositor shows only these, so the bar's
-- `rescue_cell` below is unreachable and the capability's own state is the only channel left.
-- `attempts` is printed because a config cannot rebuild it -- capability state is sampled at layout
-- time (ADR-0044), so two identical failures in a row are one unchanged `error` string and a
-- counter written here would miss the second.
-- `type your password, then Enter` rather than `enter your password`, because with a `textfield`
-- painting nothing the only feedback a user gets that the keyboard is reaching the field at all is
-- knowing which key ends the entry. Backspace corrects; the engine keeps the buffer, not this VM.
local lock_status = cell(label(oblisk.lock, function(l)
    if l.error == nil or l.error == "" then
        return l.active and "type your password, then Enter" or "locking..."
    end
    return string.format("%s (%d)", l.error, l.attempts or 0)
end), "#f38ba8ff")

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
-- monitor each, addressed as `"bar@{output}"` and `"notification_area@{output}"`. The `window`
-- after them is a real `xdg_toplevel`, addressed as `"settings"` with no `@output` because the
-- compositor places it; the `popup` last is a real `xdg_popup`, addressed as `"click_menu"` and for
-- the same reason plus one of its own -- it belongs to the click that opened it, not to the output
-- set (ADR-0051 decision 1).
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
            children = { clock, net, bt, kbd, media, sound, tray_cell, notifs, click_button, lock_button, rescue_cell },
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
    -- A real `xdg_toplevel`, opened and closed by the button above. It is the compositor that
    -- places and sizes this, not the config: § 6.2 gives a `window` no `monitor`, no `anchor` and
    -- no size, so `niri msg windows` is where you check that the title and app_id below arrived.
    window {
        id = "settings",
        title = "Oblisk settings",
        app_id = "oblisk.settings",
        -- Advisory in the spec's own words, and never clamped against the resolved tree (§ 6.2).
        -- They do bound the size this client picks when a compositor leaves an axis to it.
        min_size = { width = 320, height = 240 },
        max_size = { width = 1280, height = 800 },
        -- Not a literal: the whole point of Phase 22 item 5. False until the first click.
        visible = settings_open,
        child = column {
            -- Fills whatever the compositor configured, so the window is opaque and takes clicks
            -- across its whole area -- a `Content`-sized child under a tiling compositor would
            -- leave most of the surface transparent and, since the input region is the visible
            -- content (ADR-0038 decision 5), click-through.
            width = "Fill",
            height = "Fill",
            padding = { top = 12, right = 12, bottom = 12, left = 12 },
            background = BG,
            children = { cell(clicks:map(function(n)
                return string.format("settings, opened after %d clicks", n)
            end), FG) },
        },
    },
    popup {
        id = "click_menu",
        -- The `id` of the surface this anchors to, not a node -- the protocol roots a popup under a
        -- parent *surface* at creation (§ 6.3).
        parent = "bar",
        -- Bound as a signal, which is the spelling § 6.3 and docs/adr/0050 decision 3 prescribe:
        -- `on_click` receives the button's own rect and writes it here, and the popup opens over
        -- whichever button was actually clicked. It used to read `:get()` because the evaluation
        -- pass rejected a raw signal outright, which froze the rect at whatever the file last saw;
        -- that pass now skips a signal-bound property and leaves it to the resolved tree
        -- (docs/adr/0049's second amendment, `renderer/src/layout/node.rs`'s `is_deferred_signal`).
        anchor_rect = popup_anchor,
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
        -- Not a literal, for the `window` above's reason and one more: `visible` going true is what
        -- creates the `xdg_popup`, and it may only do so from inside a click, because that is the
        -- only turn a grab serial is armed for (docs/adr/0049's amendment).
        visible = menu_open,
        -- Fired when the compositor dismisses this popup, which for a grabbing popup is what a
        -- click anywhere outside it means (§ 6.3). Writing the flag back is the config's half of
        -- docs/adr/0051 decision 2: the engine has already destroyed the object and latched the
        -- declaration shut, and this `false` is what unlatches it so the next click can reopen it.
        -- A config that omits this is not a config error -- the latch is what keeps that from being
        -- a livelock -- it just cannot reopen until something else writes `visible = false`.
        on_dismiss = function()
            menu_open:set(false)
            print("[shell.lua] popup dismissed")
        end,
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
    -- Declared, not open. § 6.4 gives a `lock` an `id` and a `child` and nothing else -- no
    -- `visible`, no `monitor`, no size -- because the compositor decides when these surfaces exist
    -- and the protocol requires one on every output while they do. Returning this costs one
    -- retained node and zero Wayland objects until `oblisk.lock:invoke("lock")` above is clicked,
    -- the same declaration/lifetime split ADR-0049 already made for `window` and `popup`.
    lock {
        id = "lock_screen",
        child = column {
            -- Opaque and full-bleed: this is what covers the session, so a `Content`-sized child
            -- would leave the desktop showing through everything it did not paint.
            width = "Fill",
            height = "Fill",
            background = "#11111bff",
            align_h = "Center",
            align_v = "Center",
            children = { column {
                width = 360,
                padding = { top = 20, right = 20, bottom = 20, left = 20 },
                spacing = 10,
                background = BG,
                radius = 10,
                border_width = 1,
                border_color = "#313244ff",
                children = { cell("locked", FG), password_field, lock_status },
            } },
        },
    },
}
