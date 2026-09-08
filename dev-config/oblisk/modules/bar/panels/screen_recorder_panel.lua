-- Mirrors ScreenRecorderPanel.qml: the two captures as buttons, the four encoder choices behind one
-- expandable row, and a way to the folder.
--
-- The panel is where the choices live. `ScreenRecorder.qml`'s three mouse buttons cover the two
-- common captures and nothing else, deliberately -- this is the surface that names them, shows what
-- a running capture is doing, and lets the encoder settings be changed without remembering a
-- keybind.
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

-- `settingGroups`, in the mirror's order: what is captured, then how well, then how fast, then in
-- what. `fallback` repeats `lib/store.lua`'s default because a store key can be missing -- an older
-- `state.json` predating this block -- and a tile group with nothing lit reads as broken.
local GROUPS = {
    {
        key = "audio",
        title = "audio",
        fallback = "desktop",
        options = {
            { value = "off",     label = "no audio",      icon = icons.vol_muted },
            { value = "desktop", label = "desktop",       icon = icons.vol_high },
            { value = "mic",     label = "desktop + mic", icon = icons.mic_on },
        },
    },
    {
        key = "quality",
        title = "quality",
        fallback = "high",
        options = {
            {
                value = "low",
                label = "low",
                icon = icons.quality_low,
                detail = "smallest files, softest detail in motion"
            },
            { value = "medium", label = "medium", icon = icons.quality_medium, detail = "balanced size and detail" },
            { value = "high",   label = "high",   icon = icons.quality_high,   detail = "sharpest detail, largest files" },
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
            { value = "mp4", label = "mp4", icon = icons.file_mp4, detail = "plays and uploads anywhere" },
            {
                value = "mkv",
                label = "mkv",
                icon = icons.file_mkv,
                detail = "stays playable if the session crashes mid-recording"
            },
        },
    },
}

-- `ScreenRecorderPanel.qml` collapses this on close (`onIsOpenChanged`). Ours keeps it, matching
-- `modules/bar/indicators/system_info.lua`, which is the config's other expandable section: coming
-- back to a panel you left open on the settings is the answer people expect, and there is no
-- close edge to hang the reset on without `lib/ui_state.lua` requiring this file back.
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

-- `settingsSummary`: the four chosen words on one line, so the row says what a capture will be
-- without opening.
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
            return string.format("ready · %s", output ~= "" and output or "no output")
        end
        local words = held and "paused" or "recording"
        return label ~= "" and string.format("%s · %s", words, label) or words
    end
)

-- One group: its heading, its tiles side by side, and the chosen tile's explanation underneath.
-- The detail line follows the selection rather than sitting on each tile, as in the mirror: three
-- sentences at once is a paragraph, and only the current choice needs defending.
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
            -- A radio, not a switch: the tile's own checked state is ignored, because clicking the
            -- lit one must not turn every option off.
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
settings_children[#settings_children + 1] = cell("changes apply to the next recording", theme.DIM, theme.font.xs, {
    width = "Fill",
    wrap = "Word",
    visible = recorder.recording,
})

local body = {
    panel_header {
        title = "screen recorder",
        icon = icons.record,
        subtitle = status_text,
        -- `accent: recording ? Theme.critical : Theme.activeColor`. Red is not "off" here; it is
        -- the alert that something is being captured right now.
        accent = recorder.recording:map(function(up)
            return up and theme.RED or theme.ACCENT
        end),
        trailing = {
            -- `badgeColor: root.paused ? Theme.warning : Theme.critical`: red while frames are
            -- being written, peach while they are not.
            info_badge(recorder.elapsed_text, recorder.paused:map(function(held)
                return held and theme.PEACH or theme.RED
            end), { visible = recorder.recording }),
        },
    },

    -- Four buttons in two slots, not two buttons that change colour. `OButton` binds `bgColor` and
    -- `variant` live -- `bgColor: recording ? critical : activeColor` on the left, `variant:
    -- recording ? "secondary" : "primary"` on the right -- and `components/action_button.lua` picks
    -- its three grounds from a static `tone` at build time. Making `tone` live would mean mapping
    -- rest, hover, border and ink through one signal each, for one caller. An invisible node takes
    -- no size and no spacing gap (`layout/scene.rs`), so a pair per state costs the same row and
    -- each button keeps one label, one tone and one job.
    row {
        width = "Fill",
        spacing = theme.spacing.sm,
        children = {
            action_button("region", function()
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
            action_button("screen", function()
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
            -- The one control in a panel that ends something already running, so it wears the
            -- alert colour rather than the accent every other primary action uses.
            action_button("stop", recorder.stop, "recorder-stop", {
                tone = "danger",
                width = "Fill",
                height = theme.control.xl,
                glyph = icons.record_stop,
                visible = recorder.recording,
            }),
            action_button(recorder.paused:map(function(held)
                return held and "resume" or "pause"
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

    -- The mirror's hairline `Rectangle`: the two captures above are actions, everything below is
    -- configuration, and the rule is what says so.
    rect { width = "Fill", height = theme.border_width, background = theme.BORDER_SUBTLE },

    panel_row {
        title = "recording settings",
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
    -- Invisible children take no size and no spacing gap (`layout/scene.rs`), so the collapsed row
    -- costs nothing and the card's measured height follows the reveal on its own.
    column {
        width = "Fill",
        spacing = theme.spacing.md,
        visible = settings_expanded,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = settings_children,
    },

    panel_row {
        title = "open recordings folder",
        -- `~`-collapsed like the mirror's `saveDirectory`: the home prefix is the same on every row
        -- of a path and tells the reader nothing.
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
