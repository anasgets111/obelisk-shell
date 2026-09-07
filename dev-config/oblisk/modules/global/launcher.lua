-- Mirrors `AppLauncher.qml`: autofocus search, arrow navigation, Enter launch.
--
-- ## Engine pieces (ADR-0112)
--
-- A plain `textfield` with `autofocus = true` gets the keyboard on map and opens empty. `on_change`
-- filters, `on_navigate` moves selection and calls the list scroll signal's `reveal`, `on_submit`
-- launches, and `on_cancel` closes. A compositor keybind toggles it through the shared `modal`
-- state (`oblisk toggle modal launcher`, `lib/ui_state.lua`), which is why that is a named
-- `state`, not a local.
--
-- ## Layer surface, not `window`
--
-- The old `xdg_toplevel` was tiled by niri beside other windows at the column's size. The mirror's
-- `OModal` is a screen scrim and centred card; a screen-sized `panel` bound to `launcher_open`
-- takes the keyboard on map, returns it on unmap, and closes through its outside catcher.
--
-- ## Selection
--
-- `selected_id` stores the last key/hover choice: app id, `WEB`, or empty. `effective_selected`
-- keeps that row if visible, else the first row, computed once for the list so each row maps one
-- signal. This keeps three hundred rows within the 5ms graph budget. Hover selection matches the
-- mirror's `hoverSelectionArmed`, so mouse and arrows move the same ring.
--
-- Dropped calculator and currency rows: both end in "Enter to copy", but the engine lacks a
-- clipboard.
-- Keep the web row because `applications:open_url` completes it.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local glyph = require("components.glyph")
local ui_state = require("lib.ui_state")
local modal = require("components.modal")
local panel_card = require("components.panel_card")
local panel_empty_state = require("components.panel_empty_state")

local SCROLL = scroll("launcher_list")
local MAX_RESULTS = 200
local PAGE = 8
-- Web-row id in `selected_id`, not a desktop-file id; desktop-file ids never start with a space.
local WEB = " web"

local query = state("launcher_query", "")
local selected_id = state("launcher_selected", "")
-- Plain locals, not state: the previous field text for Escape's two stages.
local typed = ""
local emptied_a_query = false

local function entries_of(applications)
    return (applications and applications.entries) or {}
end

-- ## Matching
--
-- Not fzf: name prefix beats word, substring, then comment; ordered letters are the fallback, with
-- shorter names breaking ties. "fi" therefore gives Firefox, Files, Profile Editor. Ten lines beat
-- a scoring library.
local function subsequence(haystack, needle)
    local position = 1
    for i = 1, #needle do
        position = haystack:find(needle:sub(i, i), position, true)
        if not position then
            return false
        end
        position = position + 1
    end
    return true
end

local function score(app, needle)
    local name = app.name:lower()
    if name:sub(1, #needle) == needle then
        return 5
    end
    if name:find(" " .. needle, 1, true) then
        return 4
    end
    if name:find(needle, 1, true) then
        return 3
    end
    if app.comment and app.comment:lower():find(needle, 1, true) then
        return 2
    end
    if subsequence(name, needle) then
        return 1
    end
    return nil
end

---@param applications ApplicationsState|nil
---@param text string
---@return AppSummary[]
local function filter(applications, text)
    local entries = entries_of(applications)
    local needle = text:lower():gsub("^%s+", ""):gsub("%s+$", "")
    if needle == "" then
        return { table.unpack(entries, 1, math.min(#entries, MAX_RESULTS)) }
    end
    local scored = {}
    for _, app in ipairs(entries) do
        local s = score(app, needle)
        if s then
            scored[#scored + 1] = { app = app, score = s }
        end
    end
    table.sort(scored, function(a, b)
        if a.score ~= b.score then
            return a.score > b.score
        end
        if #a.app.name ~= #b.app.name then
            return #a.app.name < #b.app.name
        end
        return a.app.name < b.app.name
    end)
    local results = {}
    for i = 1, math.min(#scored, MAX_RESULTS) do
        results[i] = scored[i].app
    end
    return results
end

local results = computed({ oblisk.applications, query }, filter)

-- ## Web row
--
-- `WebProvider.qml`: hostname-shaped input opens as a link; other input searches. Show for a URL
-- always, otherwise only when no application matches.
local function looks_like_url(text)
    return text:match("^https?://[^%s]+$") ~= nil or text:match("^[%w%-]+%.[%w%-%.]+[%w]/?[^%s]*$") ~= nil
end

local function web_target(text)
    if looks_like_url(text) then
        return text:match("^https?://") and text or ("https://" .. text), "Open link"
    end
    local encoded = text:gsub("[^%w%-_%.~]", function(c)
        return string.format("%%%02X", c:byte())
    end)
    return "https://duckduckgo.com/?q=" .. encoded, "Web search"
end

local trimmed = query:map(function(text)
    return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end)

local web_shown = computed({ trimmed, results }, function(text, found)
    return text ~= "" and (looks_like_url(text) or #found == 0)
end)

-- Ring `selected_id` when its row shows, else the first row. That is the launch target before input
-- and after filtering removes the hovered row. One `computed` serves the list; each row asks it
-- once.
local effective_selected = computed({ selected_id, results, web_shown }, function(id, found, web)
    local first = web and WEB or (found and found[1] and found[1].id) or ""
    if id == "" then
        return first
    end
    if id == WEB then
        return web and WEB or first
    end
    for _, app in ipairs(found or {}) do
        if app.id == id then
            return id
        end
    end
    return first
end)

-- ## Selection
--
-- Arrow-key order: visible web row first, then results. Read it when the key arrives, never inside
-- a
-- `computed`.
local function rows_now()
    local text = trimmed:get()
    local found = results:get() or {}
    local ids = {}
    if text ~= "" and (looks_like_url(text) or #found == 0) then
        ids[#ids + 1] = WEB
    end
    for _, app in ipairs(found) do
        ids[#ids + 1] = app.id
    end
    return ids
end

local function select_first()
    local ids = rows_now()
    selected_id:set(ids[1] or "")
    SCROLL:reveal(1)
end

local function move(delta)
    local ids = rows_now()
    if #ids == 0 then
        return
    end
    local current = 1
    for i, id in ipairs(ids) do
        if id == effective_selected:get() then
            current = i
            break
        end
    end
    local next_index = math.max(1, math.min(current + delta, #ids))
    selected_id:set(ids[next_index])
    -- List index excludes the web row above it.
    local in_list = next_index - (ids[1] == WEB and 1 or 0)
    if in_list >= 1 then
        SCROLL:reveal(in_list)
    end
end

local function close()
    ui_state.close_modal("launcher")
end

local function activate()
    local id = effective_selected:get()
    if id == "" then
        return
    end
    if id == WEB then
        local url = web_target(trimmed:get())
        oblisk.applications:invoke("open_url", url)
    else
        oblisk.applications:invoke("launch", id)
    end
    close()
end

-- ## Rows

local function is_selected(id)
    return effective_selected:map(function(selected)
        return selected == id
    end)
end

local function row_shell(id, slot, children, opts)
    local hovered = hover(slot)
    local selected = is_selected(id)
    return button {
        hover = hovered,
        width = "Fill",
        height = theme.launcher_row_height,
        radius = theme.radius.md,
        visible = opts and opts.visible,
        background = computed({ selected, hovered }, function(on, hot)
            if on then
                return theme.ACCENT_SUBTLE
            end
            return hot and theme.GLASS_HOVER or nil
        end),
        border_width = theme.border_width,
        border_color = selected:map(function(on)
            return on and theme.ACCENT or "#00000000"
        end),
        -- The mirror arms hover selection only on pointer motion, so scrolling under a parked
        -- pointer, or opening under one, cannot steal the keyboard ring. `on_hover` has the same
        -- rule (ADR-0112 amendment), so no arming flag.
        on_hover = function(inside)
            if inside then
                selected_id:set(id)
            end
        end,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            selected_id:set(id)
            activate()
        end,
        children = { row {
            width = "Fill",
            height = "Fill",
            spacing = theme.spacing.md,
            align_v = "Center",
            padding = { left = theme.spacing.sm, right = theme.spacing.sm },
            children = children,
        } },
    }
end

---@param app AppSummary
local function app_row(app)
    local selected = is_selected(app.id)
    local lines = {
        cell(app.name, selected:map(function(on)
            return on and theme.ACCENT or theme.FG
        end), theme.font.md, { width = "Fill" }),
    }
    if app.comment and app.comment ~= "" then
        lines[#lines + 1] = cell(app.comment, theme.TEXT_OFF, theme.font.xs, { width = "Fill" })
    end
    return row_shell(app.id, "launcher-app-" .. app.id, {
        -- `Utils.resolveIconSource(..., "application-x-executable")`: entries without `Icon=` still
        -- get a generic picture.
        -- `AppLauncher.qml`: the selected row's icon grows 1.3x in place (ADR-0149).
        icon {
            name = app.icon or "application-x-executable",
            size = theme.launcher_icon,
            align_v = "Center",
            scale = selected:map(function(on)
                return on and 1.3 or 1
            end),
            animate = { scale = { duration = theme.animation_fast_ms, easing = "OutCubic" } },
        },
        column { width = "Fill", align_v = "Center", children = lines },
    })
end

local web_row = row_shell(WEB, "launcher-web", {
    glyph(icons.web, theme.TEXT_OFF, theme.launcher_icon, { align_v = "Center" }),
    column {
        width = "Fill",
        align_v = "Center",
        children = {
            cell(trimmed, theme.FG, theme.font.md, { width = "Fill" }),
            cell(trimmed:map(function(text)
                local _, what = web_target(text)
                return what
            end), theme.TEXT_OFF, theme.font.xs, { width = "Fill" }),
        },
    },
}, { visible = web_shown })

local app_list = list {
    width = "Fill",
    height = "Fill",
    scroll = SCROLL,
    spacing = theme.spacing.xs,
    source = results,
    itemfn = app_row,
    key = function(app)
        return app.id
    end,
}

-- ## Search box
--
-- `OInput` at `size: "xl"`: a glass, hairlined field taller than bar controls. The engine paints
-- text/caret; this `rect` paints its ground and ring because `textfield` has no box.
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
            placeholder = "Search apps, or type a link",
            font_size = theme.font.lg,
            foreground = theme.FG,
            on_change = function(text)
                -- Escape empties the field before `on_cancel`; remember whether text existed here.
                -- Opening with autofocus also sends `""`, resetting selection and scroll.
                emptied_a_query = text == "" and typed ~= ""
                typed = text
                query:set(text)
                select_first()
            end,
            on_submit = activate,
            -- `handleSearchKey`'s two-stage Escape: text clears and stays; empty closes. The engine
            -- already cleared the field and released the keyboard; autofocus takes it back.
            on_cancel = function()
                if emptied_a_query then
                    emptied_a_query = false
                    return
                end
                close()
            end,
            on_navigate = function(key)
                if key == "up" or key == "backtab" then
                    move(-1)
                elseif key == "down" or key == "tab" then
                    move(1)
                elseif key == "page_up" then
                    move(-PAGE)
                elseif key == "page_down" then
                    move(PAGE)
                end
            end,
        },
    },
}

local no_results = panel_empty_state("No results found",
    computed({ trimmed, results, web_shown }, function(text, found, web)
        return text ~= "" and #found == 0 and not web
    end))

local no_apps = panel_empty_state("no applications found",
    computed({ oblisk.applications, trimmed }, function(apps, text)
        return text == "" and #entries_of(apps) == 0
    end))

-- Centered below the bar, `OModal`'s `anchors.centerIn: parent`. The surface excludes the bar's
-- reservation, so center in its own height; `screens[1]` follows `panel_host.lua`'s clamp.
local card_margin = oblisk.screens:map(function(screens)
    local screen = screens and screens[1]
    if not (screen and screen.width and screen.height) then
        return { left = 0, top = 0 }
    end
    local free_height = screen.height - theme.bar_height
    return {
        left = math.max(0, math.floor((screen.width - theme.launcher_width) / 2)),
        top = math.max(0, math.floor((free_height - theme.launcher_height) / 2)),
    }
end)

return modal({
    kind = "launcher",
    keyboard = true,
    card = panel_card({
        search,
        panel_card({ web_row, app_list, no_results, no_apps }, {
            width = "Fill",
            height = "Fill",
            background = theme.GLASS_CONTENT,
            border_width = theme.border_width,
            border_color = theme.GLASS_BORDER,
            padding = {
                top = theme.spacing.sm,
                right = theme.spacing.sm,
                bottom = theme.spacing.sm,
                left = theme.spacing.sm,
            },
        }),
    }, {
        width = theme.launcher_width,
        height = theme.launcher_height,
        margin = card_margin,
        spacing = theme.spacing.sm,
        padding = {
            top = theme.spacing.lg,
            right = theme.spacing.lg,
            bottom = theme.spacing.lg,
            left = theme.spacing.lg,
        },
        radius = theme.radius.lg,
        background = theme.GLASS,
        border_width = theme.border_width,
        border_color = theme.BORDER,
    }),
})
