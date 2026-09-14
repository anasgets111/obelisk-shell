-- Mirrors AudioPanel.qml: masthead, output/microphone cards with pickers, and one slider per app
-- stream.
--
-- Sliders use `components/slider.lua` and `button`'s `on_drag`/`on_wheel` (ADR-0116). Device
-- pickers and the mixer expand on click through `PanelRow.expandable` and three `state()` signals.
-- Unlike the mirror, closing the panel leaves an open picker open. Stream icons use
-- `obelisk.applications` and `app_id`, falling back to a note glyph.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local glyph = require("components.glyph")
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

local function percent(value)
    return value and string.format("%d%%", math.floor(value * 100 + 0.5)) or "--"
end

-- `AudioService.normalizeDeviceName`: remove redundant ALSA description words.
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

-- Mirror `AudioControl`: title/device, percentage, mute button, slider, and caller-supplied rows.
---@class AudioControlOpts
---@field name string The slider's state name.
---@field title string
---@field glyph_on string
---@field glyph_off string
---@field volume fun(a: AudioState): number?
---@field muted fun(a: AudioState): boolean
---@field device fun(a: AudioState): AudioDevice? The active device, for the subtitle and leading glyph.
---@field is_input? boolean
---@field set_volume string The `obelisk.audio` action taking one volume.
---@field headroom? boolean Past 100%: a red fill and a marker at 100%.
---@field toggle_mute string The `obelisk.audio` action taking nothing.
---@field visible? Bound
---@field under? Node[]

---@param opts AudioControlOpts
local function audio_control(opts)
    local is_muted = obelisk.audio:map(function(a)
        return a ~= nil and opts.muted(a)
    end)
    local ready = obelisk.audio:map(function(a)
        return a ~= nil and opts.volume(a) ~= nil
    end)
    local mute_glyph = is_muted:map(function(m)
        return m and opts.glyph_off or opts.glyph_on
    end)
    local tint = is_muted:map(function(m)
        return m and theme.DIM or theme.ACCENT
    end)
    local leading_glyph = computed({ obelisk.audio, mute_glyph }, function(a, fallback)
        return a and not opts.muted(a) and util.audio_device_glyph(opts.device(a), opts.is_input) or fallback
    end)
    local held = state("audio_pending_" .. opts.name, -1)

    local children = {
        row {
            width = "Fill",
            spacing = theme.spacing.sm,
            align_v = "Center",
            children = {
                glyph(leading_glyph, tint, theme.icon.lg, { align_v = "Center" }),
                column {
                    width = "Fill",
                    align_v = "Center",
                    children = {
                        cell({ { text = opts.title, bold = true } }, theme.FG, theme.font.sm, { width = "Fill" }),
                        cell(util.label(obelisk.audio, function(a)
                            return device_name(opts.device(a)) or "no device"
                        end), theme.DIM, theme.font.xs, { width = "Fill" }),
                    },
                },
                cell(computed({ obelisk.audio, held }, function(a, h)
                    return { { text = percent(h >= 0 and h or a and opts.volume(a)), bold = true } }
                end), tint, theme.font.sm, { align_v = "Center" }),
                icon_button(mute_glyph, function()
                    obelisk.audio:invoke(opts.toggle_mute)
                end, {
                    slot = "audio-mute-" .. opts.name,
                    size = theme.control.md,
                    icon_size = theme.icon.sm,
                    opacity = ready:map(function(r)
                        return r and 1 or theme.opacity.disabled
                    end),
                    background = is_muted:map(function(m)
                        return m and theme.GLASS_CONTROL or theme.ACCENT
                    end),
                }),
            },
        },
        slider {
            name = "audio_pending_" .. opts.name,
            signal = obelisk.audio,
            read = opts.volume,
            on_commit = function(value)
                obelisk.audio:invoke(opts.set_volume, value)
            end,
            pending = held,
            max = opts.headroom and util.MAX_VOLUME or nil,
            split_at = opts.headroom and 1 or nil,
            marker = opts.headroom,
            headroom_color = is_muted:map(function(m)
                return m and theme.INACTIVE or theme.RED
            end),
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

-- Mirror `DevicePicker`: a "choose device" row expands to one row per device, with the active one
-- ringed and ticked. Show it only when there is a choice.
local function device_picker(opts)
    local devices = obelisk.audio:map(function(a)
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
                        icon = util.audio_device_glyph(device, opts.is_input)
                            or (opts.is_input and icons.mic_on or icons.speaker),
                        title = device_name(device) or "?",
                        selected = device.active,
                        trailing = glyph(icons.check, device.active and theme.ACCENT or "#00000000", theme.font.sm),
                        on_activate = function()
                            obelisk.audio:invoke(opts.set_default, device.id)
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

-- One mixer stream, the mirror's `StreamItem`: app icon/name, percentage, mute glyph, and thin
-- slider.
local function stream_row(app)
    local name = app.name or app.process_name or "unknown"
    local entry = util.app_entry(obelisk.applications:get(), app.process_name or app.name)
    local leading = entry and entry.icon and icon { name = entry.icon, size = theme.icon.md, align_v = "Center" }
        or glyph(icons.music_note, theme.FG, theme.icon.md, { align_v = "Center" })
    local tint = app.muted and theme.DIM or theme.ACCENT
    return column {
        width = "Fill",
        spacing = theme.spacing.xs,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = {
            row {
                width = "Fill",
                spacing = theme.spacing.sm,
                align_v = "Center",
                opacity = (app.muted or app.volume == nil) and theme.opacity.muted or nil,
                children = {
                    leading,
                    cell(name, theme.FG, theme.font.sm, { width = "Fill", align_v = "Center" }),
                    cell(percent(app.volume), tint, theme.font.sm, { align_v = "Center" }),
                    panel_action_icon(app.muted and icons.vol_muted or icons.vol_high, app.volume and function()
                        obelisk.audio:invoke("set_app_muted", app.id, not app.muted)
                    end, { slot = "audio-stream-mute-" .. tostring(app.id), tint = tint }),
                },
            },
            slider {
                name = "audio_pending_app_" .. tostring(app.id),
                signal = obelisk.audio,
                read = function(a)
                    for _, stream in ipairs(a.apps or {}) do
                        if stream.id == app.id then
                            return stream.volume
                        end
                    end
                end,
                on_commit = function(value)
                    obelisk.audio:invoke("set_app_volume", app.id, value)
                end,
                height = STREAM_SLIDER_HEIGHT,
                color = tint,
            },
        },
    }
end

local streams = obelisk.audio:map(function(a)
    return (a and a.apps) or {}
end)

local body = {
    panel_header {
        title = "audio",
        icon = obelisk.audio:map(util.volume_glyph),
        active = obelisk.audio:map(function(a)
            return a ~= nil and a.volume ~= nil and not a.muted
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
            return util.active_device(a.sinks)
        end,
        set_volume = "set_volume",
        toggle_mute = "toggle_mute",
        headroom = true,
        under = {
            device_picker {
                name = "output",
                open = output_picker_open,
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
            return util.active_device(a.sources)
        end,
        is_input = true,
        set_volume = "set_source_volume",
        toggle_mute = "toggle_source_mute",
        visible = util.shown_when(obelisk.audio, function(a)
            return #(a.sources or {}) > 0
        end),
        under = {
            device_picker {
                name = "input",
                open = input_picker_open,
                is_input = true,
                list = function(a)
                    return a.sources
                end,
                set_default = "set_default_source",
            },
        },
    },
    -- `MixerSection`: application count, expanding to one slider per stream, capped and scrollable.
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
            end), theme.DIM, theme.font.sm),
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
