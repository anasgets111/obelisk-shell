-- Mirrors ActiveWindow.qml: the focused window's icon and title in the centre zone.
--
-- Both halves read one push, `oblisk.workspaces.active_client`. The old title lived in a `state`
-- signal and showed `click for the focused window` until clicked. Its click ran
-- `process.run("niri", {"msg", "-j", "focused-window"})`, fed the result to `json.decode`
-- (ADR-0057),
-- and needed a request counter for out-of-order replies. That demo belonged elsewhere. ADR-0056
-- excludes window lists, not the focused `active_client` already in every snapshot; shelling out to
-- niri bought a subprocess per click while duplicating pushed data.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- `nil` before the first snapshot and whenever nothing holds focus; the supervisor omits the key
-- rather than sending null (`workspaces/controller.rs`).
local function focused(workspaces)
    return workspaces and workspaces.active_client
end

-- `text: hasActive ? baseLabel : "Desktop"`, lowercased for this shell's voice. The mirror captions
-- an empty desktop rather than leaving a hole, and `CenterSide.qml` anchors this node
-- unconditionally, so hiding the row on an empty workspace would move the bar's midpoint every time
-- the last window closed.
local EMPTY_LABEL = "desktop"

-- `iconSource`'s fallback, `resolveIconSource("", "", "applications-system")`.
local EMPTY_ICON = "applications-system"

-- `baseLabel: title || displayName`, where `displayName` is the desktop entry's name and then the
-- raw `app_id`. A window that sets no title -- a splash, a freshly mapped terminal -- otherwise
-- captions as nothing at all while still holding focus.
local function label(applications, workspaces)
    local client = focused(workspaces)
    if client == nil then
        return EMPTY_LABEL
    end
    local title = client.title
    if title ~= nil and title ~= "" then
        return title
    end
    local entry = util.app_entry(applications, client.class)
    return (entry and entry.name) or client.class or EMPTY_LABEL
end

-- The second `oblisk.applications` consumer (ADR-0061). `active_client.class` is the toplevel
-- `app_id` (ADR-0056 decision 5), which `util.app_entry` maps to a `.desktop` entry. ADR-0054
-- decision 5 reserved this caller before the capability existed.
local focused_icon = icon {
    name = computed({ oblisk.applications, oblisk.workspaces }, function(applications, workspaces)
        local client = focused(workspaces)
        if client == nil then
            return EMPTY_ICON
        end
        local entry = util.app_entry(applications, client.class)
        return (entry and entry.icon) or EMPTY_ICON
    end),
    -- `height: Theme.controlHeightSm`, a step above the `icon.lg` this used: the centre caption is
    -- the bar's one piece of prose and its icon reads as an app rather than a status glyph.
    size = theme.control.sm,
    align_v = "Center",
}

-- No pill or button: `ActiveWindow.qml` puts the icon and title directly on the bar. No ground
-- behind them makes the centre read as a caption rather than one more control. The old button only
-- existed as the click target for the removed fetch.
return row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.xs,
    children = {
        focused_icon,
        cell(computed({ oblisk.applications, oblisk.workspaces }, function(applications, workspaces)
            return { { text = util.truncate(label(applications, workspaces), theme.title_limit), bold = true } }
        end), theme.FG, theme.font.sm, { align_v = "Center" }),
    },
}
