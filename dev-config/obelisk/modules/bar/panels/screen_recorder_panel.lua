-- Mirrors ScreenRecorderPanel.qml: two capture buttons, four encoder choices in one expandable row,
-- and a folder action.
--
-- `ScreenRecorder.qml`'s three mouse buttons cover only the two common captures. This panel names
-- them, shows capture status, and exposes encoder settings without a keybind.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local glyph = require("components.glyph")
local panel_row = require("components.panel_row")
local panel_header = require("components.panel_header")
local panel_toggle_card = require("components.panel_toggle_card")
local section_header = require("components.section_header")
local action_button = require("components.action_button")
local info_badge = require("components.info_badge")
local store = require("lib.store")
local ui_state = require("lib.ui_state")
local recorder = require("lib.screen_recording")

local KIND = "screen_recorder"

-- `settingGroups` follows the mirror's order: capture, quality, frame rate, and format. `fallback`
-- repeats `lib/store.lua`'s default because an older `state.json` can lack a key, leaving a group
-- with no selected tile.
local GROUPS = {
    {
        key = "audio",
        title = "audio",
        fallback = "desktop",
        options = {
            { value = "off",     label = "No audio",      icon = icons.vol_muted },
            { value = "desktop", label = "Desktop",       icon = icons.vol_high },
            { value = "mic",     label = "Desktop + mic", icon = icons.mic_on },
        },
    },
    {
        key = "quality",
        title = "quality",
        fallback = "high",
        options = {
            {
                value = "low",
                label = "Low",
                icon = icons.quality_low,
                detail = "Smallest files, softest detail in motion"
            },
            { value = "medium", label = "Medium", icon = icons.quality_medium, detail = "Balanced size and detail" },
            { value = "high",   label = "High",   icon = icons.quality_high,   detail = "Sharpest detail, largest files" },
        },
    },
    {
        key = "fps",
        title = "frame rate",
        fallback = 60,
        -- No glyphs: a frame rate is a number, and the mirror leaves `icon` unset here too.
        options = {
            { value = 30,  label = "30 fps" },
            { value = 60,  label = "60 fps" },
            { value = 120, label = "120 fps" },
        },
    },
    {
        key = "container",
        title = "format",
        fallback = "mp4",
        options = {
            { value = "mp4", label = "MP4", icon = icons.file_mp4, detail = "Plays and uploads anywhere" },
            {
                value = "mkv",
                label = "MKV",
                icon = icons.file_mkv,
                detail = "Stays playable if the session crashes mid-recording"
            },
        },
    },
}

-- Unlike `ScreenRecorderPanel.qml`, this stays expanded across close, matching
-- `modules/bar/indicators/system_info.lua`; `lib/ui_state.lua` has no close hook to reset it.
local settings_expanded = state("recorder_settings_expanded", false)

local function selected_option(group, settings)
    local chosen = (type(settings) == "table" and settings[group.key]) or group.fallback
    for _, option in ipairs(group.options) do
        if option.value == chosen then
            return option
        end
    end
    return nil
end

-- `settingsSummary` keeps the four choices visible in the row without opening it.
local settings_summary = store.screen_recorder:map(function(settings)
    local words = {}
    for _, group in ipairs(GROUPS) do
        local option = selected_option(group, settings)
        if option then
            words[#words + 1] = option.label
        end
    end
    return table.concat(words, " · ")
end)

local status_text = computed(
    { recorder.recording, recorder.paused, recorder.capture_label, recorder.monitor, recorder.start_error },
    function(up, held, label, output, failure)
        if not up then
            if failure ~= nil and failure ~= "" then
                return failure
            end
            return string.format("Ready · %s", output ~= "" and output or "no output")
        end
        local words = held and "Paused" or "Recording"
        return label ~= "" and string.format("%s · %s", words, label) or words
    end
)

-- The detail line follows the selected tile, not every tile, as in the mirror. Only the current
-- choice needs an explanation.
local function option_group(group)
    local tiles = {}
    for _, option in ipairs(group.options) do
        tiles[#tiles + 1] = panel_toggle_card({
            slot = string.format("recorder-%s-%s", group.key, tostring(option.value)),
            icon = option.icon,
            label = option.label,
            height = theme.control.lg,
            signal = store.screen_recorder,
            read = function(settings)
                local option_of = selected_option(group, settings)
                return option_of ~= nil and option_of.value == option.value
            end,
            -- A radio, not a switch: ignore the tile's checked state so clicking the lit one cannot
            -- turn every option off.
            on_change = function()
                recorder.set_setting(group.key, option.value)
            end,
        })
    end

    local detail = store.screen_recorder:map(function(settings)
        local option = selected_option(group, settings)
        return (option and option.detail) or ""
    end)

    return column {
        width = "Fill",
        spacing = theme.spacing.xs,
        children = {
            section_header(group.title),
            row { width = "Fill", spacing = theme.spacing.xs, children = tiles },
            cell(detail, theme.DIM, theme.font.xs, {
                width = "Fill",
                wrap = "Word",
                visible = detail:map(function(line)
                    return line ~= ""
                end),
            }),
        },
    }
end

local settings_children = {}
for _, group in ipairs(GROUPS) do
    settings_children[#settings_children + 1] = option_group(group)
end
settings_children[#settings_children + 1] = cell("Changes apply to the next recording", theme.DIM, theme.font.xs, {
    width = "Fill",
    wrap = "Word",
    visible = recorder.recording,
})

local body = {
    panel_header {
        title = "Screen recorder",
        icon = icons.record,
        subtitle = status_text,
        -- `accent: recording ? Theme.critical : Theme.activeColor`: red marks an active capture,
        -- not
        -- "off".
        accent = recorder.recording:map(function(up)
            return up and theme.RED or theme.ACCENT
        end),
        trailing = {
            -- `badgeColor: root.paused ? Theme.warning : Theme.critical`: red while frames are
            -- written, peach while they are not.
            info_badge(recorder.elapsed_text, recorder.paused:map(function(held)
                return held and theme.PEACH or theme.RED
            end), { visible = recorder.recording }),
        },
    },

    -- Four buttons in two slots, not two colour-changing buttons. `OButton` binds `bgColor` and
    -- `variant` live (`bgColor: recording ? critical : activeColor`; `variant: recording ?
    -- "secondary" : "primary"`). `components/action_button.lua` fixes its three grounds from static
    -- `tone`. Making `tone` live would map rest, hover, border, and ink through one signal each for
    -- one caller. Invisible nodes take no size or spacing gap (`layout/scene.rs`), so a pair per
    -- state costs the same row and each button keeps one label, one tone, and one job.
    row {
        width = "Fill",
        spacing = theme.spacing.sm,
        children = {
            action_button("Region", function()
                ui_state.close_panel()
                recorder.start("selection")
            end, "recorder-region", {
                tone = "solid",
                width = "Fill",
                height = theme.control.xl,
                glyph = icons.region,
                visible = recorder.recording:map(function(up)
                    return not up
                end),
            }),
            action_button("Screen", function()
                ui_state.close_panel()
                recorder.start()
            end, "recorder-screen", {
                tone = "solid",
                width = "Fill",
                height = theme.control.xl,
                glyph = icons.display,
                visible = recorder.recording:map(function(up)
                    return not up
                end),
            }),
            -- This control ends a running capture, so it uses the alert colour instead of the
            -- accent.
            action_button("Stop", recorder.stop, "recorder-stop", {
                tone = "danger",
                width = "Fill",
                height = theme.control.xl,
                glyph = icons.record_stop,
                visible = recorder.recording,
            }),
            action_button(recorder.paused:map(function(held)
                return held and "Resume" or "Pause"
            end), recorder.toggle_pause, "recorder-pause", {
                tone = "accent",
                width = "Fill",
                height = theme.control.xl,
                glyph = recorder.paused:map(function(held)
                    return held and icons.play or icons.pause
                end),
                visible = recorder.recording,
            }),
        },
    },

    -- The mirror's hairline `Rectangle` separates capture actions from configuration.
    rect { width = "Fill", height = theme.border_width, background = theme.BORDER_SUBTLE },

    panel_row {
        title = "Recording settings",
        subtitle = settings_summary,
        icon = icons.settings,
        slot = "recorder-settings",
        trailing = glyph(settings_expanded:map(function(open)
            return open and icons.chevron_down or icons.chevron_right
        end), theme.DIM, theme.icon.sm),
        on_activate = function()
            settings_expanded:set(not settings_expanded:get())
        end,
    },
    -- Invisible children take no size or spacing gap (`layout/scene.rs`), so the collapsed row
    -- costs nothing and the card's measured height follows the reveal.
    column {
        width = "Fill",
        spacing = theme.spacing.md,
        visible = settings_expanded,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = settings_children,
    },

    panel_row {
        title = "Open recordings folder",
        -- Collapse the home prefix to `~`, as with the mirror's `saveDirectory`; it tells the
        -- reader nothing when repeated on every path.
        subtitle = recorder.directory:map(function(dir)
            local home = os.getenv("HOME") or ""
            if home ~= "" and dir:sub(1, #home) == home then
                return "~" .. dir:sub(#home + 1)
            end
            return dir
        end),
        icon = icons.folder,
        slot = "recorder-folder",
        on_activate = function()
            ui_state.close_panel()
            recorder.open_directory()
        end,
    },
}

return { kind = KIND, body = body }
