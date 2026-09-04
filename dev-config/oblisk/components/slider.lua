-- A track that fills to a fraction and takes the pointer, `Components/Slider.qml`: drag anywhere
-- along it to set the value, roll the wheel over it to step. Built on `button`'s `on_drag` and
-- `on_wheel` (ADR-0116), which hand this the pointer in the track's own coordinates and the wheel
-- in notches; everything else here is arithmetic.
--
-- Two values are drawn from. While a drag is held the fill follows `pending`, a `state()` this
-- component owns, so the track tracks the finger without a round trip through the Supervisor per
-- pixel. On release `on_commit` is called once with where the drag ended, which is `Slider.qml`'s
-- `committed` and not a stream of writes. `pending` is kept until the capability's next snapshot
-- carries the value, cleared from a single `on_change` per slider name: clearing it on release
-- showed the old value for the frames the round trip through PipeWire takes, and a click flashed
-- new, old, new. A signal without `on_change` (a plain `state`) clears on release instead.
--
-- `pending` is a number because `state()` fixes a signal's type from its initial value and `nil`
-- has none; `-1` is "nothing held", a fraction never is.
--
-- `children` stack on top of the fill, so the volume pill is this component with its glyph and
-- percentage laid over the track, the way `Volume.qml` fills the whole control.
local theme = require("config.theme")

---@class SliderOpts
---@field name string The `state()` name for the held drag. Unique per slider.
---@field signal Signal|Capability<any> The capability or state the value is read from.
---@field read fun(payload: any): number The fraction, `0` to `1`, off one payload.
---@field on_commit fun(fraction: number) Called once per drag, on release, and once per wheel step.
---@field steps? integer How many positions the track has, `Slider.qml`'s `steps`: a drag lands on the nearest one and a wheel notch moves one. Default `20`, which is 5% steps over a `0` to `1` range; `0` is continuous.
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
---@field hover? Signal
---@field visible? boolean|Bound
---@field on_click? fun(rect: Rect, button: "left"|"right"|"middle") The left click still lands after the drag ends.
---@field children? Node[] Drawn over the fill.

local function clamp(fraction)
    return math.max(0, math.min(1, fraction))
end

-- The nearest of `steps` positions, so a drag that stopped at 79% commits 80% the way the mirror's
-- does, and a config reading the value back sees round numbers.
local function quantize(fraction, steps)
    if steps == nil or steps <= 0 then
        return clamp(fraction)
    end
    return clamp(math.floor(fraction * steps + 0.5) / steps)
end

local function fraction_of(read, payload)
    if payload == nil then
        return 0
    end
    local ok, value = pcall(read, payload)
    if not ok or type(value) ~= "number" then
        return 0
    end
    return clamp(value)
end

-- Slider names whose signal already has this component's `on_change`. A `list` rebuilds its rows,
-- and each rebuild constructs the slider again; the `state()` behind a name is one signal, so one
-- handler per name is enough and a second would only clear it twice.
local watched = {}

---@param opts SliderOpts
return function(opts)
    local pending = state(opts.name, -1)
    local steps = opts.steps or 20
    local dragging = false

    local on_change = opts.signal.on_change
    if on_change and not watched[opts.name] then
        watched[opts.name] = true
        on_change(opts.signal, function(current, previous)
            local held = pending:get()
            if dragging or held < 0 then
                return
            end
            local now = fraction_of(opts.read, current)
            -- The commit landed, or someone else moved it; either way the snapshot is the truth again.
            if quantize(now, steps) == held or now ~= fraction_of(opts.read, previous) then
                pending:set(-1)
            end
        end)
    end

    local fill = computed({ opts.signal, pending }, function(payload, held)
        if held >= 0 then
            return held
        end
        return fraction_of(opts.read, payload)
    end)

    local children = {
        rect {
            width = fill:map(function(fraction)
                -- `%d` raises on a float in Lua 5.4; see `components/meter.lua`.
                return string.format("%d%%", math.floor(fraction * 100 + 0.5))
            end),
            height = "Fill",
            radius = opts.radius or theme.radius.sm,
            background = opts.color or theme.ACCENT,
            visible = opts.fill_visible,
        },
    }
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
        hover = opts.hover,
        visible = opts.visible,
        on_click = opts.on_click,
        on_drag = function(rect, pointer, phase)
            local fraction = quantize(pointer.x / rect.width, steps)
            dragging = phase ~= "end"
            pending:set(on_change and fraction or (dragging and fraction or -1))
            if not dragging then
                opts.on_commit(fraction)
            end
        end,
        on_wheel = function(_, notches)
            if steps <= 0 then
                return
            end
            local held = pending:get()
            local current = held >= 0 and held or fraction_of(opts.read, opts.signal:get())
            -- Onto the grid first, so a 79% set by another mixer steps to 80% and 85%, not 84%.
            local next_fraction = quantize(quantize(current, steps) + notches / steps, steps)
            if on_change then
                pending:set(next_fraction)
            end
            opts.on_commit(next_fraction)
        end,
        children = children,
    }
end
