-- Mirrors `Modules/Global/AppLauncher.qml`: a search box over a list of applications, typed into
-- the moment it opens, walked with the arrow keys, launched with Enter.
--
-- ## The engine pieces this leans on (ADR-0112)
--
-- The field is a plain `textfield` with `autofocus = true`, so the keyboard lands in it as the
-- surface maps and it opens empty every time. `on_change` filters, `on_navigate` moves the
-- selection and asks the list's scroll signal to `reveal` the row it moved to, `on_submit` launches
-- what is selected, `on_cancel` (Escape) closes. `oblisk toggle launcher_open` from a compositor
-- keybind is the other way in, which is why `launcher_open` is a named `state` and not a local.
--
-- ## A layer surface, not a `window`
--
-- This was an `xdg_toplevel`, which niri tiles: it opened in the layout beside the other windows,
-- at whatever size the column had, and took focus by the compositor's window rules. The mirror's
-- `OModal` is a scrim over the whole screen with a card in the middle, and a screen-sized `panel`
-- with `keyboard_interactivity` bound to `launcher_open` is that: it takes the keyboard on map and
-- gives it back on unmap, the catcher under the card closes on a click outside, and the card sits
-- where a modal sits rather than where the tiling put it.
--
-- ## What the selection is
--
-- `selected_id` holds what a key or a hover last chose: an application id, `WEB` for the row that
-- opens a search, or nothing. `effective_selected` turns that into the row the ring is on -- the
-- choice where it is still showing, the first row otherwise -- once, for the whole list, so a
-- row's own `selected` is one `map` over one signal. That is what keeps three hundred rows inside
-- the graph's 5ms budget. Hovering a row selects it, as the mirror's `hoverSelectionArmed` does,
-- so the mouse and the arrow keys move the same ring.
--
-- Not carried over: the calculator and currency rows. Both end in "Enter to copy", and this engine
-- has no clipboard yet; a result that can be read but not taken is half a feature. The web row
-- stays, since `applications:open_url` is already there to finish it.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local panel_card = require("components.panel_card")
local panel_empty_state = require("components.panel_empty_state")

local SCROLL = scroll("launcher_list")
local MAX_RESULTS = 200
local PAGE = 8
-- The id the web row goes by in `selected_id`. Not a desktop file id: those never start with a
-- space, which is the whole of what makes this one safe.
local WEB = " web"

local query = state("launcher_query", "")
local selected_id = state("launcher_selected", "")

local function entries_of(applications)
    return (applications and applications.entries) or {}
end

-- ## Matching
--
-- Not fzf, and not trying to be: a prefix of the name beats a word inside it beats a substring
-- beats the comment, letters in order anywhere is the fallback, and ties go to the shorter name.
-- That is the order a person expects "fi" to give -- Firefox, then Files, then Profile Editor --
-- and it is ten lines rather than a scoring library.
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

-- ## The web row
--
-- `WebProvider.qml`'s two shapes: something that reads as a hostname opens as a link, anything else
-- becomes a search. Shown for a URL always, and otherwise only when no application matched, so it
-- never sits above a real result.
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

-- What the ring is on: `selected_id` where it names a row that is showing, else the first row. The
-- fallback is what the launcher opens on, before any key has chosen, and what a hover-selected
-- row's disappearance falls back to when the next keystroke narrows the list past it. One
-- `computed` for the whole list; each row then asks one question of it.
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
-- The rows in the order the arrow keys walk them: the web row first when it is showing, then the
-- results. Read at the moment a key arrives, never inside a `computed`.
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
    -- The list's own index: the web row sits above the list, so it is not counted.
    local in_list = next_index - (ids[1] == WEB and 1 or 0)
    if in_list >= 1 then
        SCROLL:reveal(in_list)
    end
end

local function close()
    ui_state.launcher_open:set(false)
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
        -- The mirror arms hover-selection on pointer motion so a list scrolling under a still
        -- pointer does not steal the ring from the keyboard. `on_hover` fires on the crossing,
        -- which a wheel scroll also produces, so the ring can jump to the row that slid under the
        -- pointer -- the cost of not having motion events, and small.
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
        -- `Utils.resolveIconSource(..., "application-x-executable")`: an entry with no `Icon=`
        -- still gets a picture, and the generic one says what it is.
        icon { name = app.icon or "application-x-executable", size = theme.launcher_icon, align_v = "Center" },
        column { width = "Fill", align_v = "Center", children = lines },
    })
end

local web_row = row_shell(WEB, "launcher-web", {
    cell(icons.web, theme.TEXT_OFF, theme.launcher_icon, { align_v = "Center" }),
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

-- ## The search box
--
-- `OInput` at `size: "xl"`: a glass field with a hairline, taller than any control on the bar, the
-- one thing on the card that is not a row. The engine draws the field's text and caret; the ground
-- and the ring are this `rect`, since a `textfield` paints no box of its own.
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
                query:set(text)
                select_first()
            end,
            on_submit = activate,
            on_cancel = close,
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

local no_results = panel_empty_state("No results found", computed({ trimmed, results, web_shown }, function(text, found, web)
    return text ~= "" and #found == 0 and not web
end))

local no_apps = panel_empty_state("no applications found", computed({ oblisk.applications, trimmed }, function(apps, text)
    return text == "" and #entries_of(apps) == 0
end))

-- Centred in the space under the bar, `OModal`'s `anchors.centerIn: parent`. The surface is the
-- screen minus what the bar reserved, so its own height is what to centre in; `screens[1]` on the
-- same terms as `modules/shell/panel_host.lua`'s clamp.
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

return panel {
    id = "launcher",
    namespace = "oblisk-launcher",
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = ui_state.launcher_open,
    -- Exclusive, and only while open: the field must be typable with no click, and a surface that
    -- is not on screen must not hold anything.
    keyboard_interactivity = ui_state.launcher_open:map(function(open)
        return open and "Exclusive" or "None"
    end),
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            -- The scrim, and the click-outside catcher in one: `hit::descend` stops at the card
            -- above it, so only a click beside the card lands here.
            button {
                width = "Fill",
                height = "Fill",
                background = theme.SCRIM,
                on_click = close,
            },
            panel_card({
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
        },
    },
}
