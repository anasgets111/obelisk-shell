-- Value-filled track matching `Components/Slider.qml`: drag sets value anywhere; wheel steps it.
-- `button`'s `on_drag`/`on_wheel` provide track-local coordinates and wheel notches (ADR-0116).
--
-- During a drag, `state()` `pending` keeps fill local and avoids a Supervisor round trip per pixel.
-- Release calls `on_commit` once at the final position, matching `Slider.qml`'s `committed`.
--
-- Keep `pending` until the capability snapshot carries the value.
-- Clear it with one `on_change` per slider name. Clearing on release showed the old value during a
-- PipeWire round trip showed old value, then flashed new, old, new.
-- A one-second timer releases it if the snapshot never matches: another writer or a clamped write.
-- Plain `state` without `on_change` clears on release.
-- `pending` is numeric because `state()` fixes its type at creation; `nil` has none.
-- `-1` means "nothing held", outside the value range. `children` stack over the fill;
-- `Volume.qml` fills the whole control.
local theme = require("config.theme")

---@class SliderOpts
---@field name string The `state()` name for the held drag. Unique per slider.
---@field signal Signal|Capability<any> The capability or state the value is read from.
---@field read fun(payload: any): number? The value, `0` to `max`, off one payload; `nil` ignores drag and wheel.
---@field on_commit fun(value: number) Called once per drag, on release, and once per wheel step.
---@field max? number The full track's value. Default `1`.
---@field steps? number How many positions the track has across `0` to `max`, `Slider.qml`'s `steps`: a drag lands on the nearest one and a wheel notch moves one. Default `20 * max`, 5% steps; `0` is continuous.
---@field split_at? number The value past which the fill takes `headroom_color`. Default `max`.
---@field headroom_color? Color|Bound The fill past `split_at`. Default `theme.RED`.
---@field marker? boolean A 1px line at `split_at`.
---@field color? Color|Bound The fill. Default accent.
---@field track? Color|Bound The ground under the fill. Default `theme.SURFACE`.
---@field fill_visible? boolean|Bound Hides the fill and keeps the track and the input.
---@field width? Length|Bound
---@field height? integer|Bound
---@field radius? integer|Bound
---@field align_v? Align
---@field background? Color|Bound Overrides `track`.
---@field border_width? integer
---@field border_color? Color|Bound
---@field animate? Animations|Bound Eases the track's own properties; the fill follows the value and is not eased.
---@field hover? Signal
---@field pending? StateSignal<number> The held value, `-1` when none. Default `state(name)`.
---@field dragging? StateSignal<boolean> True while a drag is held. Default `state(name .. "_dragging")`.
---@field visible? boolean|Bound
---@field on_click? fun(rect: Rect, button: "left"|"right"|"middle") The left click still lands after the drag ends.
---@field children? Node[] Drawn over the fill.

local function clamp(value, max)
    return math.max(0, math.min(max, value))
end

-- Snap to the nearest `steps` position: a 79% drag commits 80%, as in the mirror.
local function quantize(value, steps, max)
    if steps <= 0 then
        return clamp(value, max)
    end
    return clamp(math.floor(value / max * steps + 0.5) * max / steps, max)
end

local function value_of(read, payload, max)
    if payload == nil then
        return nil
    end
    local ok, value = pcall(read, payload)
    if not ok or type(value) ~= "number" then
        return nil
    end
    return clamp(value, max)
end

-- Per name, as `list` rebuilds rows: `on_change` registers once; `timer` and wheel `rest` persist.
local per_name = {}

---@param opts SliderOpts
return function(opts)
    local pending = opts.pending or state(opts.name, -1)
    local dragging = opts.dragging or state(opts.name .. "_dragging", false)
    local max = opts.max or 1
    local steps = opts.steps or (20 * max)
    -- Continuous: half a displayed percent.
    local tolerance = (steps > 0 and 0.5 / steps or 0.005) * max
    local on_change = opts.signal.on_change

    local entry = per_name[opts.name]
    if not entry then
        entry = { rest = 0 }
        per_name[opts.name] = entry
        if on_change then
            on_change(opts.signal, function(current, previous)
                local held = pending:get()
                if dragging:get() or held < 0 then
                    return
                end
                -- Landed, or another writer moved away from `held`; ours move toward it.
                local now, before = value_of(opts.read, current, max), value_of(opts.read, previous, max)
                if now == nil or before == nil then
                    return
                end
                local off = math.abs(now - held)
                local was = math.abs(before - held)
                if off <= tolerance or off > was + tolerance then
                    pending:set(-1)
                end
            end)
        end
    end

    local function hold(value)
        if not on_change then
            pending:set(-1)
            return
        end
        pending:set(value)
        if entry.timer then
            entry.timer:cancel()
        end
        entry.timer = timer(1000, function()
            if not dragging:get() then
                pending:set(-1)
            end
        end)
    end

    local fill = computed({ opts.signal, pending }, function(payload, held)
        if held >= 0 then
            return held
        end
        return value_of(opts.read, payload, max) or 0
    end)

    -- `%d` raises on a float in Lua 5.4; see `components/meter.lua`.
    local function percent(value)
        return string.format("%d%%", math.floor(value / max * 100 + 0.5))
    end
    local function bar(width, color)
        return rect {
            width = width,
            height = "Fill",
            radius = opts.radius or theme.radius.sm,
            background = color,
            visible = opts.fill_visible,
        }
    end
    local split = opts.split_at or max
    local children = { row {
        width = "Fill",
        height = "Fill",
        children = {
            bar(fill:map(function(value)
                return percent(math.min(value, split))
            end), opts.color or theme.ACCENT),
            bar(fill:map(function(value)
                return percent(math.max(0, value - split))
            end), opts.headroom_color or theme.RED),
        },
    } }
    if opts.marker then
        children[2] = row {
            width = "Fill",
            height = "Fill",
            children = {
                rect { width = percent(split) },
                rect { width = 1, height = "Fill", background = theme.with_opacity(theme.FG, theme.opacity.medium) },
            },
        }
    end
    for _, child in ipairs(opts.children or {}) do
        children[#children + 1] = child
    end

    return button {
        width = opts.width or "Fill",
        height = opts.height or theme.s(16, 12),
        align_v = opts.align_v,
        radius = opts.radius or theme.radius.sm,
        background = opts.background or opts.track or theme.SURFACE,
        border_width = opts.border_width,
        border_color = opts.border_color,
        animate = opts.animate,
        hover = opts.hover,
        visible = opts.visible,
        on_click = opts.on_click,
        on_drag = function(rect, pointer, phase)
            if value_of(opts.read, opts.signal:get(), max) == nil then
                dragging:set(false)
                pending:set(-1)
                return
            end
            local value = quantize(pointer.x / rect.width * max, steps, max)
            if phase ~= "end" then
                dragging:set(true)
                pending:set(value)
                return
            end
            dragging:set(false)
            hold(value)
            opts.on_commit(value)
        end,
        on_wheel = function(_, notches)
            local current = value_of(opts.read, opts.signal:get(), max)
            if steps <= 0 or current == nil then
                return
            end
            -- Touchpad fractions accumulate, reset on reversal; 1e-4 rounds f32 0.9999... up.
            local total = (entry.rest * notches < 0 and 0 or entry.rest) + notches
            local whole = math.modf(total + (total < 0 and -1e-4 or 1e-4))
            entry.rest = total - whole
            if whole == 0 then
                return
            end
            local held = pending:get()
            current = held >= 0 and held or current
            -- Snap first: a 79% value from another mixer steps to 80% then 85%, not 84%.
            local next_value = quantize(quantize(current, steps, max) + whole * max / steps, steps, max)
            hold(next_value)
            opts.on_commit(next_value)
        end,
        children = children,
    }
end
