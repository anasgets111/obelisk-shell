-- Mirrors ActiveWindow.qml: the focused window's icon and title in the centre zone.
--
-- Both halves read `oblisk.workspaces.active_client`; the old title used a `state` signal.
-- It showed `click for the focused window` until clicked, then ran
-- `process.run("niri", {"msg", "-j", "focused-window"})` fed its result to
-- `json.decode` (ADR-0057); a request counter handled out-of-order replies.
-- ADR-0056 excludes window lists; `active_client` is in every snapshot. The subprocess duplicated
-- pushed data and cost one process per click.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- `nil` before the first snapshot and whenever nothing holds focus; the supervisor omits the key
-- rather than sending null (`workspaces/controller.rs`).
local function focused(workspaces)
    return workspaces and workspaces.active_client
end

-- `text: hasActive ? baseLabel : "Desktop"`, lowercased here. The mirror captions an empty desktop.
-- `CenterSide.qml` anchors this node unconditionally; hiding it moves the midpoint when the last
-- window closed.
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

-- `active_client.class` is the toplevel `app_id` (ADR-0056 decision 5).
-- `util.app_entry` maps it to a `.desktop` entry. This is the second `oblisk.applications` consumer
-- (ADR-0061), reserved by ADR-0054 decision 5 before the capability existed.
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

-- No pill or button: `ActiveWindow.qml` puts icon and title on bar. No ground makes it a caption.
-- It is not another control; the old button served the removed fetch.
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
