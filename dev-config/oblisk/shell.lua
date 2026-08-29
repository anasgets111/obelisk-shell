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

-- The palette, and the reason it is a second file rather than ten more locals here: ADR-0047
-- points `package.path` at this directory and nothing else, and clears `package.loaded` before
-- every re-evaluation, so editing `theme.lua` recolours the bar in place. Before Phase 26 a
-- `require` searched `/usr/local/share/lua/5.4/` and then the process's working directory, and an
-- edit to a required file reached a cached copy and changed nothing.
--
-- Unpacked into locals rather than read as `theme.BG` throughout, so that every module below stays
-- exactly as it was and this split stays a demonstration of `require` rather than a rewrite.
local theme = require("theme")
local BG      = theme.BG
local SURFACE = theme.SURFACE
local FG      = theme.FG
local DIM     = theme.DIM
local ACCENT  = theme.ACCENT
local GREEN   = theme.GREEN
local YELLOW  = theme.YELLOW
local PEACH   = theme.PEACH
local RED     = theme.RED
local MAUVE   = theme.MAUVE

-- Every capability signal reads `nil` until the Supervisor's first snapshot for it arrives, and a
-- payload can be malformed in ways this file should not crash the whole evaluation over. This fixes
-- both once: `nil` renders as "--", a raising reader renders as "!", and each module below is left
-- as the one line that reads its own payload.
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

local function cell(content, color, size)
    return text { content = content, foreground = color or FG, font_size = size or 13 }
end

-- A module's visual grouping, the thing every bar calls a pill. Worth having as a function rather
-- than repeated inline: it is eleven modules, and the padding and radius being identical across
-- them is the entire visual effect.
--
-- Cross-axis alignment is filled in here because `align_v` defaults to `"Start"` and a pill is
-- taller than everything in it. A child that forgets the property sits on the top edge while its
-- neighbours that remembered sit centred, which is a per-child bug that reads as an engine one: the
-- volume pill drew its `icon` a few pixels above the readout for exactly this reason, since the
-- `button` and the `meter` beside it both set the property and the bare `icon` did not. Setting the
-- row's own `align_v` does not help, because that places the pill inside the bar rather than placing
-- the pill's children inside the pill. A child wanting `"Start"` or `"Stretch"` still says so and
-- keeps it.
local function pill(children, background)
    for _, child in ipairs(children) do
        if child.align_v == nil then
            child.align_v = "Center"
        end
    end
    return row {
        height = "Fill",
        align_v = "Center",
        spacing = 6,
        padding = { left = 10, right = 10 },
        background = background or SURFACE,
        radius = 6,
        children = children,
    }
end

local function count(list)
    return list and #list or 0
end

-- A module that has nothing to say should not be a pill containing "--". `visible` is an ordinary
-- base property (§ 5.1) and takes a signal like any other, so a module can hide itself on the same
-- pass that resolves its text, and a hidden child is skipped by the row's own positioning rather
-- than laid out at zero width.
local function shown_when(signal, predicate)
    return signal:map(function(value)
        if value == nil then
            return false
        end
        local ok, shown = pcall(predicate, value)
        return ok and shown or false
    end)
end

-- `text` has no truncation, no ellipsis and no max width (it wraps to its parent and that is all),
-- so a 200-character track title would push every module to its right off the bar. Truncating in
-- Lua is the only lever a config has today.
--
-- `utf8.offset` rather than `string.sub`, because `string.sub` counts bytes: cutting a track title
-- at byte 28 lands mid-scalar on any non-ASCII text and produces a string the shaper cannot render.
local function truncate(s, limit)
    if utf8.len(s) == nil or utf8.len(s) <= limit then
        return s
    end
    return string.sub(s, 1, utf8.offset(s, limit + 1) - 1) .. "..."
end

-- A bar graphic, which is what a percentage actually wants to look like. The trick is that `width`
-- accepts a "NN%" string and a signal resolves before the property is parsed (ADR-0044), so a
-- signal that maps to "45%" is a live-width rect with no engine support for progress bars at all.
local function meter(signal, read, color, width)
    return row {
        width = width or 40,
        height = 6,
        align_v = "Center",
        background = SURFACE,
        radius = 3,
        children = { rect {
            width = signal:map(function(value)
                if value == nil then
                    return "0%"
                end
                local ok, pct = pcall(read, value)
                if not ok or pct == nil then
                    return "0%"
                end
                -- `math.floor`, and it is load-bearing rather than tidy. Lua 5.4's `%d` raises on a
                -- float with no integer representation, and `audio.volume * 100` is 45.00027 on a
                -- real session. A raise inside a signal getter fails the whole re-resolve, and the
                -- engine then rolls the scene back and keeps the last good frame (ADR-0044), so one
                -- bad number here freezes every module on the bar, not just this meter.
                return string.format("%d%%", math.floor(math.max(0, math.min(100, pct)) + 0.5))
            end),
            height = "Fill",
            background = color,
            radius = 3,
        } },
    }
end

-- Left zone -----------------------------------------------------------------------------------

-- Workspaces, and the only module here reading a compositor's IPC rather than a device or a D-Bus
-- service (docs/adr/0056). The strip is one `text` cell, not a `list` of buttons, because `list`
-- lays out vertically only (the no-horizontal-list ponytail in scene.rs) -- this is that ponytail's
-- second consumer, the tray being the first.
--
-- It reads `outputs[1]` rather than looping, which is the fixture winning over the worked example:
-- this machine has one output, and a config for a real multi-monitor setup would match
-- `out.focused_workspace ~= nil` to find the monitor with keyboard focus (docs/adr/0056 decision 4)
-- or loop and draw a strip per monitor.
--
-- `name` before `idx`: niri lets a workspace be named, and a named one is what its user calls it.
-- `idx` is the position on the output, which is what an unnamed workspace has instead.
--
-- The click cycles to the next workspace and is what proves § 3.2's `workspaces:focus(id)`. It
-- sends `id`, never `idx`: `idx` shifts when workspaces are reordered, so the id is the only
-- argument that still names the workspace the user just saw.
local workspaces_module = pill({
    button {
        height = 18,
        align_v = "Center",
        -- Right cycles backwards, the same reading of the second argument the brightness pill makes.
        on_click = function(_, button)
            local w = oblisk.workspaces:get()
            local out = w and (w.outputs or {})[1]
            if out == nil then
                return
            end
            local entries = out.workspaces or {}
            if #entries == 0 then
                return
            end
            local at = 1
            for i, ws in ipairs(entries) do
                if ws.id == out.active_workspace then
                    at = i
                end
            end
            local step = button == "right" and -1 or 1
            oblisk.workspaces:invoke("focus", entries[(at - 1 + step) % #entries + 1].id)
        end,
        children = { cell(label(oblisk.workspaces, function(w)
            local out = (w.outputs or {})[1]
            if out == nil then
                return "no workspaces"
            end
            local marks = {}
            for _, ws in ipairs(out.workspaces or {}) do
                local name = ws.name or tostring(ws.idx)
                marks[#marks + 1] = ws.id == out.active_workspace and ("[" .. name .. "]") or name
            end
            return table.concat(marks, " ")
        end), FG) },
    },
    -- § 2.9's `active_client`, minus the `is_fullscreen` niri cannot answer (docs/adr/0056
    -- decision 5). `class` is the app id: on Wayland there is no WM_CLASS to read.
    cell(label(oblisk.workspaces, function(w)
        local client = w.active_client
        if client == nil then
            return "no window"
        end
        return truncate(client.class ~= "" and client.class or "?", 14) .. (client.is_floating and " (float)" or "")
    end), DIM, 11),
})

-- The one module that reads two capabilities at once, and the reason `computed` exists: a media
-- widget wants the player from `mpris` and the output volume from `audio`, and neither signal can
-- see the other.
local media = pill({ cell(computed({ oblisk.mpris, oblisk.audio }, function(m, a)
    local player = m and (m.players or {})[1]
    if not player then
        return "no media"
    end
    local mark = player.play_state == "Playing" and ">" or "||"
    local title = truncate(player.title or player.identity or "?", 16)
    if player.artist and player.artist ~= "" then
        title = title .. " -- " .. truncate(player.artist, 10)
    end
    if a and a.muted then
        return mark .. " " .. title .. " (muted)"
    end
    return mark .. " " .. title
end), ACCENT) })

-- Notifications shows the newest summary rather than a count, because a count is what a test
-- fixture shows and a summary is what a bar shows. `body` is an array of spans (§ 2.7), not a
-- string, so this deliberately reads `summary` and leaves span rendering to a real notification
-- surface: flattening spans into one line here would throw away the bold/italic/href structure the
-- Supervisor went to the trouble of parsing.
local notifications_module = pill({ cell(label(oblisk.notifications, function(n)
    if n.dnd then
        return "dnd"
    end
    local newest = (n.feed or {})[1]
    if not newest then
        return "no notifications"
    end
    return truncate(newest.summary or newest.app_name or "?", 16)
end), FG) })

-- Centre zone ---------------------------------------------------------------------------------

-- The clock, and the whole reason `system` exists (docs/adr/0053). `system.time` is a unix epoch in
-- seconds pushed once per wall-clock second, so `os.date` formats it the same way it would format
-- `os.time()`. The difference is that this one moves: `os.date(os.time())` freezes at whatever
-- instant the config was evaluated, because nothing re-evaluates it.
local clock = cell(label(oblisk.system, function(s)
    return os.date("%H:%M:%S", s.time)
end), FG, 15)

local date = cell(label(oblisk.system, function(s)
    return os.date("%a %d %b", s.time)
end), DIM, 11)

-- Right zone ----------------------------------------------------------------------------------

local updates_module = pill({ cell(label(oblisk.updates, function(u)
    if u.check_error and u.check_error ~= "" then
        return "updates?"
    end
    if u.installing then
        return string.format("installing %d/%d", u.install_current_step or 0, u.install_total_steps or 0)
    end
    if (u.count or 0) == 0 then
        return "up to date"
    end
    return string.format("%d updates", u.count)
end), YELLOW) })

-- Only rendered when something is actually using the camera, which is the whole point: a privacy
-- indicator that is always visible is not an indicator.
local privacy_module = row {
    height = "Fill",
    align_v = "Center",
    visible = shown_when(oblisk.privacy, function(p)
        return #(p.camera_users or {}) > 0
    end),
    children = { pill({ cell(label(oblisk.privacy, function(p)
        local users = p.camera_users or {}
        return "cam: " .. ((users[1] or {}).app_name or "?")
    end), RED) }, "#45253aff") },
}

-- Truncated, and the number is a budget rather than a taste: a zone with a fixed width is a fixed
-- number of characters, and every text module in it spends from the same total. See the surfaces
-- section for what adding `power` cost.
local keyboard_module = pill({ cell(label(oblisk.keyboard, function(k)
    local name = truncate(k.active_layout or "?", 10)
    if k.caps_lock then
        name = name .. " CAPS"
    end
    return name
end), DIM) })

local network_module = pill({ cell(label(oblisk.network, function(n)
    for _, ap in ipairs(n.available_networks or {}) do
        if ap.active then
            return string.format("%s %d%%", truncate(ap.ssid, 12), ap.strength or 0)
        end
    end
    return n.scanning and "scanning" or "offline"
end), ACCENT) })

local bluetooth_module = pill({ cell(label(oblisk.bluetooth, function(b)
    if not b.enabled then
        return "bt off"
    end
    local connected = b.connected_devices or {}
    if #connected == 0 then
        return "bt"
    end
    local first = connected[1]
    -- `battery` is -1 when BlueZ exposes no `Battery1` for the device, which is not 0 percent and
    -- must not render as it.
    if first.battery and first.battery >= 0 then
        return string.format("%s %d%%", truncate(first.name, 12), first.battery)
    end
    return truncate(first.name, 12)
end), ACCENT) })

-- Volume, real as of docs/adr/0053 decision 3. `audio.volume` is the cube root of PipeWire's
-- `channelVolumes`, which is the number `wpctl` and `pactl` show and the one a user recognises as
-- "the volume"; the raw linear value would read 3% where this reads 30%.
-- § 5.2 item 5's own worked example (`"audio-volume-high"`), which makes this the natural place to
-- prove `icon` resolves a theme name and re-resolves it when the signal pushes. `:map` rather than
-- `label`: a nil `audio` should resolve to no icon at all, and `label`'s "--" placeholder would go
-- to the theme lookup as if it were a name.
local volume_icon = oblisk.audio:map(function(a)
    if a == nil then
        return ""
    end
    if a.muted then
        return "audio-volume-muted"
    end
    local percent = (a.volume or 0) * 100
    if percent < 34 then
        return "audio-volume-low"
    elseif percent < 67 then
        return "audio-volume-medium"
    end
    return "audio-volume-high"
end)

-- The icon and the readout are one `button`: clicking mutes, clicking again unmutes. This is the
-- § 3.2 audio write path's live proof, and `toggle_mute` is the one action of the seven that takes
-- no arguments, so it is also the one a click can express without inventing a gesture this engine
-- does not have. Volume steps and a device picker want a scroll wheel and a popup list, and the
-- config vocabulary for both is a separate slice.
--
-- Nothing here updates the pill optimistically. The new state arrives back through PipeWire's own
-- `Props` event, which is what makes a mute from this bar and a mute from `wpctl` look identical.
local volume_module = pill({
    icon { name = volume_icon, size = 14 },
    -- The readout is the button and the icon beside it is not, which is the shape
    -- `brightness_module` already uses: a `button` is a stacking container (`scene.rs` positions
    -- its children independently rather than in a line), so it holds one child. Wrapping a `row`
    -- in it to get both drew the icon's box and none of its pixels.
    button {
        height = 18,
        align_v = "Center",
        on_click = function()
            oblisk.audio:invoke("toggle_mute")
        end,
        children = { cell(label(oblisk.audio, function(a)
            if a.muted then
                return "muted"
            end
            return string.format("vol %d%%", math.floor(a.volume * 100 + 0.5))
        end), FG) },
    },
    meter(oblisk.audio, function(a)
        return a.muted and 0 or a.volume * 100
    end, MAUVE),
})

-- Brightness, and the first § 3.2 command with an argument in it. `oblisk.lock:invoke("lock")`
-- below proves the envelope; this proves the rest of the write path: the arguments array, the
-- round trip back through udev, and the revision the envelope is stamped with (Phase 25 item 2).
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
        children = { cell(label(oblisk.brightness, function(b)
            return string.format("sun %d%%", b.percent)
        end), FG) },
    },
    meter(oblisk.brightness, function(b)
        return b.percent
    end, YELLOW),
})

-- Battery, real as of docs/adr/0053. Colour carries the state, which is what a bar is for, and it
-- is a signal rather than a constant because a property resolves from a signal like any other
-- (ADR-0044). Charging is green whatever the level, because a charging battery at 8% is not the
-- emergency an 8% discharging one is.
local function battery_color(b)
    if b == nil or not b.present then
        return DIM
    end
    if b.charging then
        return GREEN
    end
    if b.percent < 15 then
        return RED
    end
    if b.percent < 30 then
        return PEACH
    end
    return GREEN
end

local battery_module = pill({
    cell(label(oblisk.battery, function(b)
        if not b.present then
            return "ac"
        end
        return string.format("%d%%%s", b.percent, b.charging and " +" or "")
    end), oblisk.battery:map(battery_color)),
    meter(oblisk.battery, function(b)
        return b.present and b.percent or 0
    end, oblisk.battery:map(battery_color), 32),
    -- `power` sits beside the battery it describes rather than in a pill of its own: § 2.13's two
    -- UPower fields read the same hardware § 2.2 reports the charge of, and the right zone has no
    -- room for a twelfth pill.
    --
    -- Each of § 2.13's four fields can be absent on its own, so each is read behind its own check
    -- rather than through one `nil` guard. This machine has no power-profiles-daemon, so
    -- `active_profile` is `nil` forever and this draws the rate and the source alone. That is the
    -- absence being reported correctly, not the module failing, and it is the difference between
    -- `nil` and a fabricated `"balanced"` that made every field optional.
    cell(label(oblisk.power, function(p)
        local parts = {}
        if p.on_battery ~= nil then
            parts[#parts + 1] = p.on_battery and "bat" or "ac"
        end
        if p.energy_rate ~= nil then
            parts[#parts + 1] = string.format("%.1fW", p.energy_rate)
        end
        if p.active_profile ~= nil then
            parts[#parts + 1] = p.active_profile
        end
        return #parts > 0 and table.concat(parts, " ") or "--"
    end), DIM, 11),
})

-- The one `list` in this file, and the only node kind whose children do not exist as a literal Lua
-- table: they are generated one per `source` element (ADR-0045 decision 3). A tray is exactly that
-- shape, so this is where it belongs rather than in a synthetic fixture.
--
-- `key` is what makes reconciliation stable across pushes: without it a tray item appearing at the
-- front would renumber every sibling and reconcile each one against the wrong previous node.
-- § 2.5 populates exactly one of `icon_name` and `icon_path` per item and never both, which is
-- why one `icon` node handles both: docs/adr/0054 decision 2 makes an absolute `name` its own path,
-- so the `or` below is the whole branch. Falling back to the app's name keeps an item visible when
-- a theme has nothing under the name it reported, rather than leaving a 16px hole.
local tray_module = pill({ list {
    source = oblisk.tray:map(function(t)
        return (t and t.items) or {}
    end),
    itemfn = function(item)
        local art = item.icon_name or item.icon_path
        if art then
            return icon { name = art, size = 16 }
        end
        return cell(truncate(item.name or item.id or "?", 10), DIM, 11)
    end,
    key = function(item)
        return tostring(item.id)
    end,
} })

-- sysinfo has no data and will read "--" forever on a live session. Left in rather than deleted,
-- because the reason is worth seeing: its pollers start dormant (`watch::channel(Duration::ZERO)`)
-- and only `sysinfo:configure({cpu_interval = ...})` wakes them, which needs the Lua write path
-- from Phase 25. The field names below are the real ones (`cpu_percent`, not `cpu_pct`); the
-- previous version of this file read `cpu_pct` and would have silently shown 0% forever the day
-- Phase 25 landed, which nobody would have caught because dormant and wrong look identical here.
local sysinfo_module = pill({ cell(label(oblisk.sysinfo, function(s)
    return string.format("cpu %d%% ram %d%%", s.cpu_percent or 0, s.ram_percent or 0)
end), DIM, 11) })

-- Interaction ---------------------------------------------------------------------------------

-- Phase 21 item 1's live proof: a `button` whose `on_click` changes what a `text` paints. The
-- counter is a `state` signal (ADR-0044 decision 5), so the handler's `:set()` is what marks the
-- scene dirty. The name is what survives an in-place reload: edit a colour above while this runs
-- and the count keeps going instead of resetting, because `state("clicks", 0)` finds the signal it
-- built last time and ignores the new initial.
local clicks = state("clicks", 0)

-- The anchor rect the `popup` hangs from. ADR-0049's amendment settles where it comes from: not off
-- the input-dispatch stack, but through the config, because `on_click` receives the button's own
-- rect (ADR-0050 decision 3) and writes it to a named `state` signal the popup reads back. The
-- initial is the button's declared size, because `anchor_rect` must be non-zero before anything has
-- ever been clicked or the whole evaluation fails (§ 6.3).
local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 90, height = 24 })
local settings_open = state("settings_open", false)

-- Not toggled the way `settings_open` is, and the reason is measured rather than assumed. Under an
-- `xdg_popup` grab, niri still delivers a click on this button to us, because the bar is the
-- popup's own parent surface and so inside the grab's tree, so a toggle would close it. What a
-- toggle would also do is fight `on_dismiss` on every click landing elsewhere, since that path
-- already writes false. One writer per edge.
local menu_open = state("menu_open", false)

local menu_button = button {
    width = 90,
    height = 24,
    background = SURFACE,
    radius = 6,
    on_click = function(rect)
        clicks:set(clicks:get() + 1)
        popup_anchor:set(rect)
        settings_open:set(not settings_open:get())
        menu_open:set(true)
    end,
    children = { cell(clicks:map(function(n)
        return string.format("menu %d", n)
    end), ACCENT) },
}

-- A bar button is a strange place to arm a lock and it is the only place available: ADR-0052
-- decision 1 makes locking an ordinary capability command, and an input callback is the only thing
-- that can issue one today. `oblisk.idle` cannot, because a `SupervisorFrame::IdleEvent` reaches the
-- Renderer and stops there, so the idle threshold a real config would lock on has nowhere to land.
--
-- `invoke` is the one generic write path (Phase 25 item 1): it builds § 7.2's envelope from the
-- capability, action and arguments and knows nothing about locking. The day `sysinfo:configure` and
-- `audio:set_volume` land they land on this same call, not on twenty-nine more bindings.
local lock_button = button {
    width = 46,
    height = 24,
    background = SURFACE,
    radius = 6,
    -- The one handler in this tree where the widened button set is not cosmetic: a right-click
    -- landing here would take over the session (docs/adr/0050's second amendment names this).
    on_click = function(_, button)
        if button ~= "left" then
            return
        end
        oblisk.lock:invoke("lock")
    end,
    children = { cell("lock", MAUVE) },
}

-- The first `process.run` in this config, and the reason `json.decode` exists (docs/adr/0057,
-- build-steps.md section 6 item 2). `niri msg -j focused-window` reports something no capability
-- carries: ADR-0056 keeps window lists out of `workspaces`, so before the decoder this data was
-- unreachable from Lua no matter how many subprocesses a config spawned.
--
-- The accumulate-then-decode shape is the one every JSON-emitting subprocess needs, not a
-- flourish. `out_cb` fires once per line with the newline stripped, because the Supervisor reads
-- the child through `BufReader::lines()`, so a pretty-printed document arrives in pieces and only
-- `exit_cb` knows the buffer is whole.
local window_title = state("window_title", "click for the focused window")

-- Every answer arrives after the click that asked for it, and nothing cancels a request the config
-- has moved on from: `exit_cb` fires whenever the child exits, however stale its question has
-- become. Without this counter a second click, or the reset below, is silently overwritten when the
-- first click's reply lands late, and a rapid double click shows whichever answer the scheduler
-- happened to finish last rather than the newer one. Every subprocess-backed module has this
-- problem, so the demo carries the guard rather than pretending it does not.
local title_request = 0

-- Bumping the counter is what cancels the outstanding request, so the fetch and the reset both go
-- through here instead of each remembering to do it.
local function claim_title_request()
    title_request = title_request + 1
    return title_request
end

local function refresh_window_title()
    local request = claim_title_request()
    local buffer = {}
    process.run("niri", { "msg", "-j", "focused-window" }, function(line, stream)
        if stream == "stdout" then
            table.insert(buffer, line)
        end
    end, function(code)
        if request ~= title_request then
            return
        end
        if code ~= 0 then
            window_title:set(string.format("niri msg exited %s", tostring(code)))
            return
        end
        local window, err = json.decode(table.concat(buffer))
        if not window then
            -- The ambiguous case `json.decode`'s doc comment names, met in the first config to
            -- call it: a bare top-level `null` decodes cleanly to `nil`, so `if not window` cannot
            -- tell success from a parse error. `err` is the only thing that can, which is why it is
            -- read rather than dropped.
            window_title:set(err and "undecodable" or "nothing focused")
            return
        end
        window_title:set(truncate(window.title or "untitled", 28))
    end)
end

local window_title_module = pill({
    button {
        width = 210,
        height = 24,
        on_click = function(_, button)
            if button == "left" then
                refresh_window_title()
            else
                claim_title_request()
                window_title:set("click for the focused window")
            end
        end,
        -- `text.content` takes a signal directly (ADR-0044), so this needs no `label` wrapper: the
        -- signal already holds a string on every path above, including both failure paths.
        children = { cell(window_title, DIM) },
    },
})

-- Only rendered when the config itself has failed, so `rescue` is the one signal whose absence is
-- the healthy case (§ 2.10).
local rescue_cell = row {
    height = "Fill",
    align_v = "Center",
    visible = shown_when(oblisk.rescue, function(r)
        return r.error_log ~= nil and r.error_log ~= ""
    end),
    children = { pill({ cell("config error", RED) }, "#45253aff") },
}

-- Lock screen ---------------------------------------------------------------------------------

-- § 6.4 routes authentication through a `textfield` with `secure_submit`, and that pair is what
-- keeps the password out of this VM entirely: with both `mask_character` and `secure_submit` set,
-- keystrokes go into a native buffer on the Renderer's Wayland thread and leave as a
-- `("lock", "authenticate")` envelope, never as a Lua value (§ 5.2 item 8, ADR-0005/ADR-0027). So
-- there is deliberately no `on_change`/`on_submit` here: one would be the exact hole the design
-- exists to close.
--
-- It is the only `secure_submit` field on this surface, and that is load-bearing: the engine
-- focuses a surface's sole `secure_submit` field the moment the compositor gives that surface
-- keyboard focus, so this is typable with no click. A second such field would put the lock screen
-- back to needing a mouse, because with two destinations the engine refuses to guess.
--
-- Measured, so it is not mistaken for breakage on a live lock: a `textfield` paints nothing today
-- (`paint.rs` skips the kind), so this reserves 28px and swallows keystrokes while showing no
-- masked characters at all. `lock_status` below is what shows that typing is landing.
local password_field = textfield {
    width = "Fill",
    height = 28,
    placeholder = "password",
    mask_character = "*",
    secure_submit = { capability = "lock", action = "authenticate" },
}

-- Off `oblisk.lock` rather than off `rescue`, and the split is ADR-0052 decision 4: with the lock
-- surfaces mapped the compositor shows only these, so the bar's `rescue_cell` is unreachable and the
-- capability's own state is the only channel left. `attempts` is printed because a config cannot
-- rebuild it: capability state is sampled at layout time (ADR-0044), so two identical failures in a
-- row are one unchanged `error` string and a counter written here would miss the second.
local lock_status = cell(label(oblisk.lock, function(l)
    if l.error == nil or l.error == "" then
        return l.active and "type your password, then Enter" or "locking..."
    end
    return string.format("%s (%d)", l.error, l.attempts or 0)
end), RED)

-- The lock screen gets the clock too, because every lock screen has one and because it is the
-- cheapest possible proof that `system` keeps pushing while the session is locked.
local lock_clock = cell(label(oblisk.system, function(s)
    return os.date("%H:%M", s.time)
end), FG, 48)

-- Surfaces ------------------------------------------------------------------------------------

-- Three zones at 40/20/40. Each is a fixed-width row distributing its own spare space by its own
-- `align_h`, which is the only way to centre anything here: a `Fill` child takes the parent's whole
-- budget rather than the remainder (`resolve_non_content` in scene.rs), so the flexbox trick of two
-- `Fill` spacers does not work, they both take the full width.
--
-- The sides are equal because that is what makes the middle a centre. A 30/40/30 split with the
-- modules this bar carries put the right zone over its 576px and ran the battery off the edge of a
-- 1920px output; widening only the right zone would have fixed the overflow and moved the clock off
-- centre, since a centre zone is only centred while it is symmetric about the middle.
--
-- The tray is in the left zone and most status-shaped modules are on the right, which reads
-- backwards until you notice the tray is the one module with no width of its own: it grows with
-- however many `StatusNotifierItem`s happen to be registered.
--
-- Adding `power` to the battery pill put the right zone over its 768px again and clipped `lock`
-- off the edge, and trimming two text modules did not buy back enough. Two things moved instead.
-- `notifications` left the bar entirely: the `notification_area` surface below already draws the
-- same newest notification, so the bar copy was the one place that information appeared twice.
-- `brightness` moved to the left zone, which has the room and no reason to prefer the right.
--
-- Worth naming rather than fixing again by shaving characters: neither side can grow. The sides
-- are equal because that is what makes the middle a centre, and a 20% centre holding a clock and a
-- date has slack that a 40% side cannot borrow. Every module added from here costs another module
-- its place until this engine has a real space-between.
-- The wallpaper, which is not a capability and never was (docs/adr/0055). Everything
-- `wallpaper:set(mon, path, fit, anim, dur)` was going to carry already had a home once ADR-0038
-- moved surface declaration here and Phase 21 built `state`: the monitor is `panel.monitor`, the
-- path is `image.source`, the fit is `image.fit`, and the two animation arguments need an animation
-- model this engine does not have. Changing it at runtime is `wallpaper:set(path)` on the signal
-- below, with no IPC anywhere in the path.
--
-- `oblisk.config_dir` is what lets this name a file it ships beside itself. It stays a `state`
-- signal rather than a constant so the runtime path is the one being exercised, not a literal that
-- happens to work at boot.
local wallpaper = state("wallpaper", oblisk.config_dir .. "/wallpaper.svg")

return {
    -- All four edges anchored, so the compositor sizes both axes and this covers the output. Not
    -- exclusive: a wallpaper that reserved screen area would push every other surface off the
    -- screen it is behind.
    panel {
        id = "wallpaper",
        layer = "Background",
        anchor = { top = true, bottom = true, left = true, right = true },
        exclusive = false,
        width = "Fill",
        height = "Fill",
        -- Painted under the image, so a source that does not decode leaves the desktop dark rather
        -- than transparent, and the failure is visible instead of looking like a surface that never
        -- mapped.
        background = "#11111bff",
        child = image {
            source = wallpaper,
            fit = "cover",
            width = "Fill",
            height = "Fill",
        },
    },
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        -- Reserves screen area along the anchored edge, derived from the height the compositor
        -- actually configures, so it stays right if this changes.
        exclusive = true,
        -- Not what a real bar wants, and deliberate: this file exercises the engine, so it opts
        -- into Phase 21 item 2's focus path rather than leaving it dark. The cost is that clicking
        -- the bar takes keyboard focus off the window behind it.
        keyboard_interactivity = "OnDemand",
        width = "Fill",
        height = 34,
        child = row {
            width = "Fill",
            height = "Fill",
            background = BG,
            padding = { left = 8, right = 8, top = 4, bottom = 4 },
            children = {
                row {
                    width = "40%",
                    height = "Fill",
                    align_h = "Start",
                    align_v = "Center",
                    spacing = 6,
                    children = { workspaces_module, window_title_module, media, tray_module, brightness_module },
                },
                row {
                    width = "20%",
                    height = "Fill",
                    align_h = "Center",
                    align_v = "Center",
                    spacing = 8,
                    children = { date, clock },
                },
                row {
                    width = "40%",
                    height = "Fill",
                    align_h = "End",
                    align_v = "Center",
                    spacing = 6,
                    children = {
                        rescue_cell,
                        privacy_module,
                        updates_module,
                        keyboard_module,
                        network_module,
                        bluetooth_module,
                        volume_module,
                        battery_module,
                        menu_button,
                        lock_button,
                    },
                },
            },
        },
    },
    -- A second surface, because one cannot catch a whole class of bug: the applied-scene log named
    -- every surface by its kind (the literal string "panel") until a second one made that visible.
    panel {
        id = "notification_area",
        layer = "Overlay",
        anchor = { top = true, right = true },
        -- Anchored to one corner, so it reserves nothing and floats over whatever is behind it.
        margin = { top = 44, right = 12 },
        -- Both axes explicit, and they have to be: layer-shell only lets the compositor pick an
        -- axis whose two edges are both anchored, and this anchors one corner.
        width = 380,
        height = 96,
        child = column {
            spacing = 6,
            padding = { top = 10, right = 12, bottom = 10, left = 12 },
            background = "#181825ee",
            radius = 10,
            border_width = 1,
            border_color = SURFACE,
            children = {
                cell(label(oblisk.notifications, function(n)
                    local newest = (n.feed or {})[1]
                    if not newest then
                        return "no notifications"
                    end
                    return truncate(newest.app_name or "?", 30)
                end), DIM, 11),
                cell(label(oblisk.notifications, function(n)
                    local newest = (n.feed or {})[1]
                    if not newest then
                        return "nothing to show"
                    end
                    return truncate(newest.summary or "?", 34)
                end), FG),
            },
        },
    },
    -- A real `xdg_toplevel`, opened and closed by the button above. The compositor places and sizes
    -- this, not the config: § 6.2 gives a `window` no `monitor`, no `anchor` and no size, so
    -- `niri msg windows` is where you check that the title and app_id arrived.
    window {
        id = "settings",
        title = "Oblisk settings",
        app_id = "oblisk.settings",
        min_size = { width = 320, height = 240 },
        max_size = { width = 1280, height = 800 },
        visible = settings_open,
        child = column {
            -- Fills whatever the compositor configured, so the window is opaque and takes clicks
            -- across its whole area. A `Content`-sized child under a tiling compositor would leave
            -- most of the surface transparent and, since the input region is the visible content
            -- (ADR-0038 decision 5), click-through.
            width = "Fill",
            height = "Fill",
            padding = { top = 14, right = 14, bottom = 14, left = 14 },
            spacing = 8,
            background = BG,
            children = {
                cell("oblisk settings", FG, 16),
                cell(label(oblisk.system, function(s)
                    return "up since " .. os.date("%H:%M:%S", s.time)
                end), DIM, 11),
                cell(label(oblisk.audio, function(a)
                    return string.format("%d playback stream(s)", count(a.apps))
                end), DIM, 11),
                cell(label(oblisk.screens, function(s)
                    return string.format("%d output(s)", #s)
                end), DIM, 11),
                sysinfo_module,
            },
        },
    },
    popup {
        id = "click_menu",
        -- The `id` of the surface this anchors to, not a node: the protocol roots a popup under a
        -- parent surface at creation (§ 6.3).
        parent = "bar",
        -- Bound as a signal, which is the spelling § 6.3 and ADR-0050 decision 3 prescribe, so the
        -- popup opens over whichever button was actually clicked.
        anchor_rect = popup_anchor,
        -- Required and non-zero on both axes: a popup has no "Fill" (§ 6.3), because there is
        -- nothing for it to fill.
        width = 220,
        height = 130,
        anchor = "BottomLeft",
        gravity = "BottomRight",
        constraint_adjustment = { "FlipY", "SlideX" },
        offset = { x = 0, y = 4 },
        -- `visible` going true is what creates the `xdg_popup`, and it may only do so from inside a
        -- click, because that is the only turn a grab serial is armed for (ADR-0049's amendment).
        visible = menu_open,
        -- Fired when the compositor dismisses this, which for a grabbing popup is a click anywhere
        -- outside it (§ 6.3). Writing the flag back is the config's half of ADR-0051 decision 2: the
        -- engine has already destroyed the object and latched the declaration shut, and this false
        -- is what unlatches it so the next click can reopen it.
        on_dismiss = function()
            menu_open:set(false)
        end,
        child = column {
            padding = { top = 10, right = 12, bottom = 10, left = 12 },
            spacing = 6,
            background = "#181825ee",
            radius = 10,
            border_width = 1,
            border_color = SURFACE,
            children = {
                cell(label(oblisk.battery, function(b)
                    if not b.present then
                        return "on ac power"
                    end
                    return string.format("battery %d%% %s", b.percent, b.charging and "charging" or "discharging")
                end), FG, 12),
                cell(label(oblisk.network, function(n)
                    return string.format("%d network(s) in range", count(n.available_networks))
                end), DIM, 12),
                cell(label(oblisk.bluetooth, function(b)
                    return string.format("%d bluetooth device(s)", count(b.connected_devices))
                end), DIM, 12),
                cell(label(oblisk.keyboard, function(k)
                    -- `or 1` would not help here: `layout_count` is 0 until the compositor's first
                    -- layout resync, and 0 is truthy in Lua, so the fallback never fires and the
                    -- line reads "layout 1 of 0". An absent count is absent, not one.
                    local total = k.layout_count or 0
                    if total == 0 then
                        return "layout unknown"
                    end
                    return string.format("layout %d of %d", (k.active_layout_index or 0) + 1, total)
                end), DIM, 12),
            },
        },
    },
    -- Declared, not open. § 6.4 gives a `lock` an `id` and a `child` and nothing else: no `visible`,
    -- no `monitor`, no size, because the compositor decides when these surfaces exist and the
    -- protocol requires one on every output while they do. Returning this costs one retained node
    -- and zero Wayland objects until `oblisk.lock:invoke("lock")` is clicked, the same
    -- declaration/lifetime split ADR-0049 made for `window` and `popup`.
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
            spacing = 18,
            children = {
                lock_clock,
                column {
                    width = 380,
                    padding = { top = 20, right = 20, bottom = 20, left = 20 },
                    spacing = 10,
                    background = BG,
                    radius = 12,
                    border_width = 1,
                    border_color = SURFACE,
                    children = { cell("locked", FG), password_field, lock_status },
                },
                cell(label(oblisk.battery, function(b)
                    if not b.present then
                        return ""
                    end
                    return string.format("battery %d%%%s", b.percent, b.charging and " charging" or "")
                end), DIM, 11),
            },
        },
    },
}
