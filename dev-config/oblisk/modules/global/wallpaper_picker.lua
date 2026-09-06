-- Mirrors `WallpaperPicker.qml`: searchable file grid, settings beside it, click/Enter applies to
-- the chosen screen or all screens.
--
-- ## Engine pieces
--
-- `oblisk.files` follows the folder (ADR-0120), so the grid lists a `computed` and does no
-- scanning.
-- Each tile uses `image` with `async = true` (ADR-0122); the pool downsizes 4K files while the card
-- is up, avoiding fifty inline decodes that held the shell for one second on open. Search reuses
-- launcher's autofocus/navigation/submit and two-stage Escape.
--
-- ## Grid rows
--
-- No wrapping layout: `rows` chunks into `theme.wallpaper_columns`, then `list` stacks them. Tab /
-- Shift-Tab move one tile; Up/Down move a row because `on_navigate` has no left/right, matching the
-- mirror's `GridView`.
--
-- ## Not carried over
--
-- Displays tab (`DisplaySettings.qml`), transition/theme/dark-mode rows (no animation model,
-- ADR-0055 decision 4, and this config has one theme), and `~/.cache/thumbnails`: pool downscale
-- makes tiles cheap and each generation decodes once.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local wallpaper = require("lib.wallpaper")
local panel_card = require("components.panel_card")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")

local SCROLL = scroll("wallpaper_grid")
local COLUMNS = theme.wallpaper_columns
-- `"all"` is the mirror's "All displays" option; no connector has that name.
local ALL = "all"

local query = state("wallpaper_query", "")
local selected_path = state("wallpaper_selected", "")
local monitor = state("wallpaper_monitor", ALL)
-- Plain locals, not state: previous field text for Escape's two stages.
local typed = ""
local emptied_a_query = false

-- ## Sizes
--
-- Tile width is the grid card's inner width divided across four columns, so no right gutter. Height
-- is 16:9, matching the mirror's `cellHeight`.
local card_padding = theme.spacing.lg
local grid_padding = theme.spacing.sm
local tile_gap = theme.spacing.xs
local grid_inner = theme.wallpaper_picker_width
    - 2 * card_padding
    - theme.wallpaper_sidebar_width
    - theme.spacing.md
    - 2 * grid_padding
local TILE_WIDTH = math.floor((grid_inner - (COLUMNS - 1) * tile_gap) / COLUMNS)
local TILE_HEIGHT = math.floor(TILE_WIDTH * 9 / 16)

-- ## List

local function entries_of(f)
    local folder = wallpaper.folder_in(f)
    return folder and folder.entries or {}
end

local trimmed = query:map(function(text)
    return (text:gsub("^%s+", ""):gsub("%s+$", "")):lower()
end)

---@return FileEntry[]
local filtered = computed({ oblisk.files, trimmed }, function(f, needle)
    local entries = entries_of(f)
    if needle == "" then
        return entries
    end
    local found = {}
    for _, entry in ipairs(entries) do
        if entry.name:lower():find(needle, 1, true) then
            found[#found + 1] = entry
        end
    end
    return found
end)

local rows = filtered:map(function(entries)
    local chunks = {}
    for i = 1, #entries, COLUMNS do
        chunks[#chunks + 1] = { table.unpack(entries, i, math.min(i + COLUMNS - 1, #entries)) }
    end
    return chunks
end)

-- ## Click target and current display state
--
-- "all" targets every screen, otherwise one. The current path is the common path or `""` when
-- screens differ, so the badge and ring do not choose among conflicting answers. An unplugged
-- selection reads as "all" (`onMonitorOptionsChanged`), because `oblisk.screens` has no
-- `on_change`.
local function choice_among(chosen, screens)
    for _, screen in ipairs(screens or {}) do
        if screen.name == chosen then
            return chosen
        end
    end
    return ALL
end

local effective_monitor = computed({ monitor, oblisk.screens }, choice_among)

local function targets_now()
    local chosen = choice_among(monitor:get(), oblisk.screens:get())
    if chosen == ALL then
        return wallpaper.outputs()
    end
    return { chosen }
end

local function common(values)
    local first = values[1]
    if first == nil then
        return ""
    end
    for _, value in ipairs(values) do
        if value ~= first then
            return ""
        end
    end
    return first
end

local current_path = computed({ wallpaper.all(), oblisk.screens, effective_monitor }, function(w, screens, chosen)
    local paths = {}
    for _, screen in ipairs(screens or {}) do
        if chosen == ALL or chosen == screen.name then
            paths[#paths + 1] = wallpaper.path_in(w, screen.name)
        end
    end
    return common(paths)
end)

local current_fit = computed({ wallpaper.all(), oblisk.screens, effective_monitor }, function(w, screens, chosen)
    local fits = {}
    for _, screen in ipairs(screens or {}) do
        if chosen == ALL or chosen == screen.name then
            fits[#fits + 1] = wallpaper.fit_in(w, screen.name)
        end
    end
    return common(fits)
end)

-- Ring `selected_path` if visible, else the applied file, else the first tile. One `computed`
-- serves
-- the grid; each tile asks it once.
local effective_selected = computed({ selected_path, current_path, filtered }, function(chosen, applied, entries)
    local first = ""
    for _, entry in ipairs(entries or {}) do
        if first == "" then
            first = entry.path
        end
        if entry.path == chosen then
            return chosen
        end
    end
    for _, entry in ipairs(entries or {}) do
        if entry.path == applied then
            return applied
        end
    end
    return first
end)

local function index_of(entries, path)
    for i, entry in ipairs(entries) do
        if entry.path == path then
            return i
        end
    end
    return 0
end

local function move(delta)
    local entries = filtered:get() or {}
    if #entries == 0 then
        return
    end
    local current = math.max(1, index_of(entries, effective_selected:get()))
    local next_index = math.max(1, math.min(current + delta, #entries))
    selected_path:set(entries[next_index].path)
    SCROLL:reveal(math.ceil(next_index / COLUMNS))
end

local function select_first()
    selected_path:set("")
    SCROLL:reveal(1)
end

local function close()
    ui_state.wallpaper_picker_open:set(false)
end

local function apply(path)
    if path == nil or path == "" then
        return
    end
    for _, output in ipairs(targets_now()) do
        wallpaper.set(output, path)
    end
end

local function apply_selected()
    apply(effective_selected:get())
end

-- ## Tiles

---@param entry FileEntry
local function tile(entry)
    local hovered = hover("wallpaper-tile-" .. entry.path)
    local selected = effective_selected:map(function(path)
        return path == entry.path
    end)
    local applied = current_path:map(function(path)
        return path == entry.path
    end)
    return button {
        width = TILE_WIDTH,
        height = TILE_HEIGHT,
        radius = theme.radius.lg,
        clip = "Rounded",
        hover = hovered,
        background = theme.GLASS_CONTENT,
        border_width = selected:map(function(on)
            return on and theme.border_width_medium or theme.border_width
        end),
        border_color = computed({ selected, hovered }, function(on, hot)
            if on then
                return theme.ACCENT
            end
            return hot and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
        end),
        on_hover = function(inside)
            if inside then
                selected_path:set(entry.path)
            end
        end,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            selected_path:set(entry.path)
            apply(entry.path)
        end,
        children = {
            image {
                source = entry.path,
                fit = "cover",
                async = true,
                width = "Fill",
                height = "Fill",
            },
            -- Bottom name strip, the mirror's `shadowColorStrong` band.
            rect {
                width = "Fill",
                height = theme.control.md,
                align_v = "End",
                background = theme.SCRIM,
                padding = { left = theme.spacing.sm, right = theme.spacing.sm },
                children = {
                    cell(entry.name, theme.FG, theme.font.xs, { width = "Fill", align = "Center", align_v = "Center" }),
                },
            },
            -- Applied badge, top left.
            rect {
                width = theme.control.xs,
                height = theme.control.xs,
                radius = theme.radius.xl,
                margin = { left = theme.spacing.sm, top = theme.spacing.sm },
                background = theme.ACCENT,
                visible = applied,
                children = {
                    cell(icons.check, theme.text_contrast(theme.ACCENT), theme.font.xs, { align = "Center", align_v = "Center" }),
                },
            },
        },
    }
end

local function grid_row(entries)
    local tiles = {}
    for _, entry in ipairs(entries) do
        tiles[#tiles + 1] = tile(entry)
    end
    return row { width = "Fill", spacing = tile_gap, children = tiles }
end

local grid = list {
    width = "Fill",
    height = "Fill",
    scroll = SCROLL,
    spacing = tile_gap,
    source = rows,
    itemfn = grid_row,
    key = function(entries)
        local paths = {}
        for i, entry in ipairs(entries) do
            paths[i] = entry.path
        end
        return table.concat(paths, "\n")
    end,
}

-- ## Empty states
--
-- In order: folder unreadable, listing pending, folder empty, no match.
local folder_state = computed({ oblisk.files, trimmed }, function(f, needle)
    local folder = wallpaper.folder_in(f)
    if folder == nil or not folder.ready then
        return "loading"
    elseif folder.error then
        return "error"
    elseif #folder.entries == 0 then
        return "empty"
    elseif needle ~= "" then
        local shown = 0
        for _, entry in ipairs(folder.entries) do
            if entry.name:lower():find(needle, 1, true) then
                shown = shown + 1
            end
        end
        return shown == 0 and "no_match" or "ok"
    end
    return "ok"
end)

local function state_is(name)
    return folder_state:map(function(current)
        return current == name
    end)
end

local empty_states = {
    panel_empty_state("Loading wallpapers…", state_is("loading"), { icon = icons.wallpaper }),
    panel_empty_state(
        oblisk.files:map(function(f)
            local folder = wallpaper.folder_in(f)
            return string.format("cannot read %s: %s", wallpaper.FOLDER, folder and folder.error or "")
        end),
        state_is("error"),
        { icon = icons.wallpaper }
    ),
    panel_empty_state("No wallpapers found", state_is("empty"), { icon = icons.wallpaper }),
    panel_empty_state("No results found", state_is("no_match")),
}

-- ## Search box, `OInput` at the top of the card

local search = rect {
    width = "Fill",
    height = theme.control.xl,
    radius = theme.radius.md,
    background = theme.GLASS_CONTENT,
    border_width = theme.border_width,
    border_color = theme.GLASS_BORDER,
    padding = { left = theme.spacing.md, right = theme.spacing.md },
    children = {
        textfield {
            width = "Fill",
            height = "Fill",
            autofocus = true,
            placeholder = "Search wallpapers…",
            font_size = theme.font.lg,
            foreground = theme.FG,
            on_change = function(text)
                emptied_a_query = text == "" and typed ~= ""
                typed = text
                query:set(text)
                select_first()
            end,
            on_submit = apply_selected,
            on_cancel = function()
                if emptied_a_query then
                    emptied_a_query = false
                    return
                end
                close()
            end,
            on_navigate = function(key)
                if key == "backtab" then
                    move(-1)
                elseif key == "tab" then
                    move(1)
                elseif key == "up" then
                    move(-COLUMNS)
                elseif key == "down" then
                    move(COLUMNS)
                elseif key == "page_up" then
                    move(-COLUMNS * 3)
                elseif key == "page_down" then
                    move(COLUMNS * 3)
                end
            end,
        },
    },
}

-- ## Sidebar, `OComboBox` rows as segments
--
-- This config has no combo popup. Each small option set is a segmented row of buttons; `choice`
-- builds one. The monitor row is a `list` because screens can change.
local function choice(value, label, current, on_pick, slot)
    local hovered = hover(slot)
    local chosen = current:map(function(now)
        return now == value
    end)
    return button {
        width = "Fill",
        height = theme.control.md,
        radius = theme.radius.sm,
        hover = hovered,
        background = computed({ chosen, hovered }, function(on, hot)
            if on then
                return theme.ACCENT
            end
            return hot and theme.GLASS_HOVER or theme.GLASS_CONTENT
        end),
        on_click = function(_, mouse_button)
            if mouse_button == "left" then
                on_pick(value)
            end
        end,
        children = {
            cell(label, chosen:map(function(on)
                return on and theme.text_contrast(theme.ACCENT) or theme.FG
            end), theme.font.xs, { width = "Fill", align = "Center", align_v = "Center" }),
        },
    }
end

local monitor_options = oblisk.screens:map(function(screens)
    local options = { { value = ALL, label = "All displays" } }
    for _, screen in ipairs(screens or {}) do
        if screen.name and screen.name ~= "" then
            options[#options + 1] = { value = screen.name, label = screen.name }
        end
    end
    return options
end)

local monitor_row = list {
    width = "Fill",
    direction = "Horizontal",
    spacing = theme.spacing.xs,
    source = monitor_options,
    itemfn = function(option)
        return choice(option.value, option.label, effective_monitor, function(value)
            monitor:set(value)
            selected_path:set("")
        end, "wallpaper-monitor-" .. option.value)
    end,
    key = function(option)
        return option.value
    end,
}

local fit_buttons = {}
for _, fit in ipairs(wallpaper.FITS) do
    fit_buttons[#fit_buttons + 1] = choice(fit.value, fit.label, current_fit, function(value)
        for _, output in ipairs(targets_now()) do
            wallpaper.set_fit(output, value)
        end
    end, "wallpaper-fit-" .. fit.value)
end
local fit_row = row { width = "Fill", spacing = theme.spacing.xs, children = fit_buttons }

local sidebar = panel_card({
    row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = {
            cell({ { text = "Wallpaper settings", bold = true } }, theme.FG, theme.font.lg, { width = "Fill" }),
            icon_button(icons.close, close, { slot = "wallpaper-close", size = theme.control.sm, icon_size = theme.icon.sm }),
        },
    },
    cell("Monitor", theme.TEXT_OFF, theme.font.xs),
    monitor_row,
    cell(current_fit:map(function(fit)
        return fit == "" and "Fill mode · mixed" or "Fill mode"
    end), theme.TEXT_OFF, theme.font.xs),
    fit_row,
    cell("Folder", theme.TEXT_OFF, theme.font.xs),
    cell(wallpaper.FOLDER, theme.DIM, theme.font.xs, { width = "Fill" }),
    cell(filtered:map(function(entries)
        return string.format("%d file(s)", #entries)
    end), theme.DIM, theme.font.xs, { width = "Fill" }),
}, {
    width = theme.wallpaper_sidebar_width,
    align_v = "Start",
    spacing = theme.spacing.md,
    background = theme.GLASS_CONTENT,
    border_width = theme.border_width,
    border_color = theme.GLASS_BORDER,
    padding = { top = theme.spacing.md, right = theme.spacing.md, bottom = theme.spacing.md, left = theme.spacing.md },
})

local grid_card_children = { grid }
for _, empty in ipairs(empty_states) do
    grid_card_children[#grid_card_children + 1] = empty
end

local body = row {
    width = "Fill",
    height = "Fill",
    spacing = theme.spacing.md,
    children = {
        panel_card(grid_card_children, {
            width = "Fill",
            height = "Fill",
            background = theme.GLASS_CONTENT,
            border_width = theme.border_width,
            border_color = theme.GLASS_BORDER,
            padding = { top = grid_padding, right = grid_padding, bottom = grid_padding, left = grid_padding },
        }),
        sidebar,
    },
}

-- Centered below the bar, like the launcher.
local card_margin = oblisk.screens:map(function(screens)
    local screen = screens and screens[1]
    if not (screen and screen.width and screen.height) then
        return { left = 0, top = 0 }
    end
    local free_height = screen.height - theme.bar_height
    return {
        left = math.max(0, math.floor((screen.width - theme.wallpaper_picker_width) / 2)),
        top = math.max(0, math.floor((free_height - theme.wallpaper_picker_height) / 2)),
    }
end)

return panel {
    id = "wallpaper_picker",
    namespace = "oblisk-wallpaper-picker",
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = ui_state.wallpaper_picker_open,
    keyboard_interactivity = ui_state.wallpaper_picker_open:map(function(open)
        return open and "Exclusive" or "None"
    end),
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            button {
                width = "Fill",
                height = "Fill",
                cursor = "default",
                background = theme.SCRIM,
                on_click = close,
            },
            panel_card({ search, body }, {
                width = theme.wallpaper_picker_width,
                height = theme.wallpaper_picker_height,
                margin = card_margin,
                spacing = theme.spacing.md,
                padding = { top = card_padding, right = card_padding, bottom = card_padding, left = card_padding },
                radius = theme.radius.lg,
                background = theme.GLASS,
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
