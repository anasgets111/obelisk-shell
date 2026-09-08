-- Mirrors `TrayMenuPanel.qml`: the DBusMenu an application hangs off its tray icon, opened by a
-- right click on that icon.
--
-- `oblisk.tray` already carries the whole tree. `MenuItem.children` comes from one
-- `GetLayout(0, -1)` at registration, so no `tray:menu_will_show` round trip is needed to draw a
-- submenu; the command exists for applications that populate lazily, and nothing here has needed it.
--
-- ## Submenus expand in place
--
-- The mirror opens each submenu as its own `PopupWindow` anchored beside the row, revealed on hover
-- and retired by a 450ms timer. Neither half is available: a popup per submenu is a surface per
-- level, and hover-driven reveal cannot be exercised here at all. A click expands the row instead
-- and the children are drawn under it, indented -- the shape `modules/bar/panels/audio_panel.lua`
-- already uses for its device pickers.
--
-- Depth is whatever the application sent, up to the Supervisor's `MAX_MENU_DEPTH`, because the rows
-- are flattened from the tree rather than nested as nodes.
local theme = require("config.theme")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")

local KIND = "tray_menu"

-- Which item's menu is showing. A tray icon is not a panel kind of its own -- every item shares one
-- card, as they share one `panel_host` section -- so the id travels beside `panel_kind`.
local item_id = state("tray_menu_item", "")

-- Ids of the submenus the user has opened. A set rather than one id: the tree can be deeper than
-- two levels, and collapsing an ancestor should not have to remember what was open beneath it.
local expanded = state("tray_menu_expanded", {})

-- `components/panel_action_icon.lua` uses the same literal for the same reason.
local CLEAR = "#00000000"

local INDENT = theme.spacing.md

local function menu_of(t, id)
    for _, item in ipairs((t and t.items) or {}) do
        if item.id == id then
            return item.menu
        end
    end
    return nil
end

-- Depth-first, carrying the indent, so `list` gets a flat source and one row shape.
local function flatten(entries, depth, open, out)
    for _, entry in ipairs(entries or {}) do
        out[#out + 1] = { entry = entry, depth = depth }
        if #(entry.children or {}) > 0 and open[tostring(entry.id)] then
            flatten(entry.children, depth + 1, open, out)
        end
    end
end

local rows = computed({ oblisk.tray, item_id, expanded }, function(t, id, open)
    local out = {}
    flatten(menu_of(t, id), 0, open or {}, out)
    return out
end)

-- The mirror's own three markers, which are plain characters rather than glyphs from
-- `config/icons.lua`: this row draws an application's menu, and a checkmark here means what the
-- application's own toolkit would have drawn, not one of the shell's icons.
local SUBMENU = "\u{203A}"
local CHECKED = "\u{2713}"
local SELECTED = "\u{25CF}"

-- `hasChildren ? SUBMENU : checkState === Checked ? CHECKED or SELECTED : ""`. A toggle that is off
-- or indeterminate draws nothing, as the mirror's ternary chain does.
local function marker(entry)
    if #(entry.children or {}) > 0 then
        return SUBMENU
    end
    if entry.toggle_state ~= 1 then
        return ""
    end
    return entry.toggle_type == "radio" and SELECTED or CHECKED
end

-- DBusMenu marks a keyboard mnemonic with `_` before the letter, and the payload carries the label
-- exactly as sent (`MenuItem.label`), so `"_Quit"` arrives with the underscore in it. Drawing that
-- literally is wrong in every toolkit.
--
-- The mirror underlines the letter instead (`dbusmenu.cpp` rewrites `_X` to `<u>X</u>`), but an
-- underline in a menu means "press this letter to run this entry", and nothing here listens for it:
-- the panel takes no keyboard focus and the row is a pointer target. Advertising an accelerator
-- that does nothing is worse than not marking it, so the marker is simply removed -- Quickshell
-- keeps the same string as `mCleanLabel`. Underline it once the key actually works.
--
-- `__` is DBusMenu's escape for a real underscore, so a pair collapses to one plain character.
---@param label string?
---@return string
local function strip_mnemonics(label)
    return ((label or ""):gsub("__", "\0"):gsub("_", ""):gsub("%z", "_"))
end

local function activate(entry)
    if #(entry.children or {}) > 0 then
        local open = expanded:get() or {}
        local next_open = {}
        for key, value in pairs(open) do
            next_open[key] = value
        end
        local key = tostring(entry.id)
        next_open[key] = not open[key] or nil
        expanded:set(next_open)
        return
    end
    -- A disabled entry is drawn, not filtered, so the application's own layout survives; activating
    -- it is the no-op the spec says it is, and the Supervisor would refuse it anyway.
    if entry.enabled then
        oblisk.tray:invoke("activate_menu_item", item_id:get(), entry.id)
    end
    ui_state.close_panel()
end

local function row_for(row_entry)
    local entry = row_entry.entry
    local pad = theme.spacing.sm + row_entry.depth * INDENT
    if entry.menu_type == "separator" then
        return rect {
            width = "Fill",
            height = theme.spacing.sm,
            children = {
                rect {
                    width = "Fill",
                    height = theme.border_width,
                    align_v = "Center",
                    background = theme.BORDER,
                    margin = { left = pad, right = theme.spacing.sm },
                },
            },
        }
    end
    local slot = "tray-menu-" .. tostring(entry.id)
    local hovered = hover(slot)
    local children = {}
    if entry.icon_name then
        children[#children + 1] = icon {
            name = entry.icon_name,
            size = theme.icon.sm,
            align_v = "Center",
            foreground = theme.FG,
        }
    end
    children[#children + 1] = cell(strip_mnemonics(entry.label), theme.FG, theme.font.sm, {
        width = "Fill",
        align_v = "Center",
    })
    local trailing = marker(entry)
    if trailing ~= "" then
        children[#children + 1] = cell(trailing, theme.FG, theme.font.sm, { align_v = "Center" })
    end
    return button {
        width = "Fill",
        height = theme.item_height,
        radius = theme.item_radius,
        hover = hovered,
        -- `color: containsMouse ? glassControlHoverColor : "transparent"`, transparent and not
        -- `nil` because that is the colour the mirror names.
        background = hovered:map(function(on)
            return on and theme.GLASS_CONTROL_HOVER or CLEAR
        end),
        -- `opacity: entry.enabled ? 1 : opacityDisabled`, on the row so the icon dims with the word.
        opacity = entry.enabled and 1 or theme.opacity.disabled,
        on_click = function(_, mouse_button)
            if mouse_button == "left" then
                activate(entry)
            end
        end,
        children = { row {
            width = "Fill",
            height = "Fill",
            align_v = "Center",
            spacing = theme.spacing.sm,
            padding = { left = pad, right = theme.spacing.sm },
            children = children,
        } },
    }
end

local body = {
    list {
        width = "Fill",
        spacing = 0,
        source = rows,
        itemfn = row_for,
        -- Depth is part of the key: the same entry drawn at two levels is two rows, and reconciling
        -- them by id alone would reuse one node for both.
        key = function(row_entry)
            return tostring(row_entry.entry.id) .. ":" .. tostring(row_entry.depth)
        end,
    },
}

---Show `item`'s menu, anchored under its icon.
---@param item TrayItem
---@param anchor Rect
local function open(item, anchor)
    item_id:set(item.id)
    expanded:set({})
    ui_state.toggle_panel(KIND, anchor)
end

return { kind = KIND, body = body, open = open }
