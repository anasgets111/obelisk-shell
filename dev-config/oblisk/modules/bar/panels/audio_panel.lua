-- Mirrors AudioPanel.qml: a masthead, one card for the output and one for the microphone, each a
-- name, a percentage, a mute button and a slider, with a device picker folded under it when there
-- is more than one device to pick; then the application mixer, one slider per stream.
--
-- The sliders are `components/slider.lua`, on `button`'s `on_drag`/`on_wheel` (ADR-0116). The
-- device pickers and the mixer fold open on a click, `PanelRow.expandable`, held in three `state()`
-- signals here; the mirror closes both pickers when the panel closes and this does not, since a
-- picker left open across a close is a picker the user opened.
--
-- Not carried over: the mirror's 150% headroom with a marker at 100% (`set_volume` clamps to
-- `[0.0, 1.0]`, § 3.2), and the per-stream application icon resolved through a desktop-entry
-- lookup, which this does through `oblisk.applications` where the process name is an `app_id` and
-- falls back to a note glyph where it is not.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local panel_action_icon = require("components.panel_action_icon")
local panel_header = require("components.panel_header")
local panel_card = require("components.panel_card")
local panel_row = require("components.panel_row")
local slider = require("components.slider")

local KIND = "audio"
local SLIDER_HEIGHT = theme.s(20, 16)
local STREAM_SLIDER_HEIGHT = theme.s(16, 12)
local MIXER_SCROLL = scroll("audio_mixer")

local output_picker_open = state("audio_output_picker", false)
local input_picker_open = state("audio_input_picker", false)
local mixer_open = state("audio_mixer_open", false)

local function percent(fraction)
    return string.format("%d%%", math.floor((fraction or 0) * 100 + 0.5))
end

local function active_device(devices)
    for _, device in ipairs(devices or {}) do
        if device.active then
            return device
        end
    end
    return nil
end

-- `AudioService.deviceIconFor`: PipeWire's `device.icon-name` says what kind of thing a device is,
-- and a headset row should look like one. The hint is a theme icon name, so the words in it are
-- what is matched; a device with no hint gets the picker's default.
local function device_glyph(device, default)
    local hint = (device and device.icon) or ""
    if hint:find("headset") or hint:find("hands%-free") then
        return icons.headset
    elseif hint:find("headphone") then
        return icons.headphones
    elseif hint:find("phone") or hint:find("portable") then
        return icons.phone
    end
    return default
end

-- `AudioService.normalizeDeviceName`: the ALSA description carries words a person does not need.
local function device_name(device)
    if device == nil then
        return nil
    end
    local name = device.name or ""
    name = name:gsub("%s*[Hh]igh [Dd]efinition [Aa]udio [Cc]ontroller", "")
    name = name:gsub("%s*HD [Aa]udio [Cc]ontroller", "")
    name = name:gsub("%s*[Dd]igital [Ss]tereo", ""):gsub("%s*[Aa]nalog [Ss]tereo", "")
    name = name:gsub("%s*%(HDMI%)", " HDMI"):gsub("%s+", " ")
    name = name:match("^%s*(.-)%s*$")
    return name ~= "" and name or device.name
end

-- The mirror's `AudioControl`: a card holding a title row (glyph, title over the device name, the
-- percentage, a mute button), a slider, and whatever the caller folds under it.
---@class AudioControlOpts
---@field name string The slider's state name.
---@field title string
---@field glyph_on string
---@field glyph_off string
---@field volume fun(a: AudioState): number
---@field muted fun(a: AudioState): boolean
---@field device fun(a: AudioState): AudioDevice? The active device, for the subtitle.
---@field set_volume string The `oblisk.audio` action taking one fraction.
---@field toggle_mute string The `oblisk.audio` action taking nothing.
---@field visible? Bound
---@field under? Node[]

---@param opts AudioControlOpts
local function audio_control(opts)
    local is_muted = oblisk.audio:map(function(a)
        return a ~= nil and opts.muted(a)
    end)
    local glyph = is_muted:map(function(m)
        return m and opts.glyph_off or opts.glyph_on
    end)
    local tint = is_muted:map(function(m)
        return m and theme.TEXT_OFF or theme.ACCENT
    end)

    local children = {
        row {
            width = "Fill",
            spacing = theme.spacing.sm,
            align_v = "Center",
            children = {
                cell(glyph, tint, theme.icon.lg, { align_v = "Center" }),
                column {
                    width = "Fill",
                    align_v = "Center",
                    children = {
                        cell({ { text = opts.title, bold = true } }, theme.FG, theme.font.sm, { width = "Fill" }),
                        cell(util.label(oblisk.audio, function(a)
                            return device_name(opts.device(a)) or "no device"
                        end), theme.TEXT_OFF, theme.font.xs, { width = "Fill" }),
                    },
                },
                cell(util.label(oblisk.audio, function(a)
                    return percent(opts.volume(a))
                end), tint, theme.font.sm, { align_v = "Center" }),
                icon_button(glyph, function()
                    oblisk.audio:invoke(opts.toggle_mute)
                end, {
                    slot = "audio-mute-" .. opts.name,
                    size = theme.control.md,
                    icon_size = theme.icon.sm,
                    background = is_muted:map(function(m)
                        return m and theme.GLASS_CONTROL or theme.ACCENT
                    end),
                }),
            },
        },
        slider {
            name = "audio_pending_" .. opts.name,
            signal = oblisk.audio,
            read = opts.volume,
            on_commit = function(fraction)
                oblisk.audio:invoke(opts.set_volume, fraction)
            end,
            height = SLIDER_HEIGHT,
            color = is_muted:map(function(m)
                return m and theme.INACTIVE or theme.ACCENT
            end),
        },
    }
    for _, node in ipairs(opts.under or {}) do
        children[#children + 1] = node
    end

    return panel_card(children, {
        width = "Fill",
        visible = opts.visible,
        spacing = theme.spacing.sm,
        background = theme.GLASS_CONTENT,
        padding = { top = theme.spacing.md, right = theme.spacing.md, bottom = theme.spacing.md, left = theme.spacing.md },
    })
end

-- The mirror's `DevicePicker`: a "choose device" row that folds open into one row per device, the
-- active one ringed and ticked. Shown only with something to choose between.
local function device_picker(opts)
    local devices = oblisk.audio:map(function(a)
        return (a and opts.list(a)) or {}
    end)
    local has_choice = devices:map(function(list)
        return #list > 1
    end)
    return column {
        width = "Fill",
        spacing = theme.spacing.xs,
        visible = has_choice,
        children = {
            panel_row {
                slot = "audio-picker-" .. opts.name,
                icon = opts.open:map(function(open)
                    return open and icons.chevron_up or icons.chevron_down
                end),
                title = "choose device",
                on_activate = function()
                    opts.open:set(not opts.open:get())
                end,
            },
            list {
                width = "Fill",
                spacing = theme.spacing.xs,
                visible = opts.open,
                source = devices,
                itemfn = function(device)
                    return panel_row {
                        slot = "audio-device-" .. opts.name .. "-" .. tostring(device.id),
                        icon = device_glyph(device, opts.default_glyph),
                        title = device_name(device) or "?",
                        selected = device.active,
                        trailing = cell(icons.check, device.active and theme.ACCENT or "#00000000", theme.font.sm),
                        on_activate = function()
                            oblisk.audio:invoke(opts.set_default, device.id)
                            opts.open:set(false)
                        end,
                    }
                end,
                key = function(device)
                    return tostring(device.id)
                end,
            },
        },
    }
end

-- One mixer stream, the mirror's `StreamItem`: the application's icon and name, its percentage,
-- a mute glyph, and a thinner slider under them.
local function stream_row(app)
    local name = app.name or app.process_name or "unknown"
    local entry = util.app_entry(oblisk.applications:get(), app.process_name or app.name)
    local leading = entry and entry.icon and icon { name = entry.icon, size = theme.icon.md, align_v = "Center" }
        or cell(icons.music_note, theme.FG, theme.icon.md, { align_v = "Center" })
    local tint = app.muted and theme.TEXT_OFF or theme.ACCENT
    return column {
        width = "Fill",
        spacing = theme.spacing.xs,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = {
            row {
                width = "Fill",
                spacing = theme.spacing.sm,
                align_v = "Center",
                opacity = app.muted and theme.opacity.muted or nil,
                children = {
                    leading,
                    cell(name, theme.FG, theme.font.sm, { width = "Fill", align_v = "Center" }),
                    cell(percent(app.volume), tint, theme.font.sm, { align_v = "Center" }),
                    panel_action_icon(app.muted and icons.vol_muted or icons.vol_high, function()
                        oblisk.audio:invoke("set_app_muted", app.id, not app.muted)
                    end, { slot = "audio-stream-mute-" .. tostring(app.id), tint = tint }),
                },
            },
            slider {
                name = "audio_pending_app_" .. tostring(app.id),
                signal = oblisk.audio,
                read = function(a)
                    for _, stream in ipairs(a.apps or {}) do
                        if stream.id == app.id then
                            return stream.volume
                        end
                    end
                    return 0
                end,
                on_commit = function(fraction)
                    oblisk.audio:invoke("set_app_volume", app.id, fraction)
                end,
                height = STREAM_SLIDER_HEIGHT,
                color = tint,
            },
        },
    }
end

local streams = oblisk.audio:map(function(a)
    return (a and a.apps) or {}
end)

local body = {
    panel_header {
        title = "audio",
        icon = oblisk.audio:map(util.volume_glyph),
        active = oblisk.audio:map(function(a)
            return a ~= nil and not a.muted
        end),
        subtitle = "volume, devices and applications",
    },
    audio_control {
        name = "output",
        title = "output",
        glyph_on = icons.vol_high,
        glyph_off = icons.vol_muted,
        volume = function(a)
            return a.volume
        end,
        muted = function(a)
            return a.muted
        end,
        device = function(a)
            return active_device(a.sinks)
        end,
        set_volume = "set_volume",
        toggle_mute = "toggle_mute",
        under = {
            device_picker {
                name = "output",
                open = output_picker_open,
                default_glyph = icons.speaker,
                list = function(a)
                    return a.sinks
                end,
                set_default = "set_default_sink",
            },
        },
    },
    audio_control {
        name = "input",
        title = "microphone",
        glyph_on = icons.mic_on,
        glyph_off = icons.mic_off,
        volume = function(a)
            return a.source_volume
        end,
        muted = function(a)
            return a.source_muted
        end,
        device = function(a)
            return active_device(a.sources)
        end,
        set_volume = "set_source_volume",
        toggle_mute = "toggle_source_mute",
        visible = util.shown_when(oblisk.audio, function(a)
            return #(a.sources or {}) > 0
        end),
        under = {
            device_picker {
                name = "input",
                open = input_picker_open,
                default_glyph = icons.mic_on,
                list = function(a)
                    return a.sources
                end,
                set_default = "set_default_source",
            },
        },
    },
    -- The mirror's `MixerSection`: one row that says how many applications are playing, folding
    -- open into a slider per stream, capped at a few rows and scrolling past that.
    panel_card({
        panel_row {
            slot = "audio-mixer",
            icon = icons.mixer,
            title = "application mixer",
            subtitle = util.label(streams, function(list)
                if #list == 0 then
                    return "no applications playing audio"
                end
                return string.format("%d active", #list)
            end),
            trailing = cell(mixer_open:map(function(open)
                return open and icons.chevron_up or icons.chevron_down
            end), theme.TEXT_OFF, theme.font.sm),
            on_activate = function()
                mixer_open:set(not mixer_open:get())
            end,
        },
        list {
            width = "Fill",
            max_height = theme.control.lg * 4 + theme.spacing.sm * 3,
            scroll = MIXER_SCROLL,
            spacing = theme.spacing.sm,
            visible = computed({ mixer_open, streams }, function(open, list)
                return open and #list > 0
            end),
            source = streams,
            itemfn = stream_row,
            key = function(app)
                return tostring(app.id)
            end,
        },
    }, {
        width = "Fill",
        spacing = theme.spacing.sm,
        background = theme.GLASS_CONTENT,
        padding = { top = theme.spacing.sm, right = theme.spacing.sm, bottom = theme.spacing.sm, left = theme.spacing.sm },
    }),
}

return { kind = KIND, body = body }
