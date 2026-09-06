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

-- The same budget as `modules/bar/indicators/media.lua`: the centre zone shows one at a time, and
-- changing width when playback stops would move the whole bar.
--
-- A character budget, not `components/cell.lua`'s bounded box: "WWWW" and "iiii" are both four
-- characters, but "WWWW" is twice the width. This node must stay content-sized so the centre
-- midpoint remains the bar midpoint; an elision box fixes the zone width.
local TITLE_LIMIT = 44

-- `nil` before the first snapshot and whenever nothing holds focus; the supervisor omits the key
-- rather than sending null (`workspaces/controller.rs`).
local function focused(workspaces)
    return workspaces and workspaces.active_client
end

-- The second `oblisk.applications` consumer (ADR-0061). `active_client.class` is the toplevel
-- `app_id` (ADR-0056 decision 5), which `util.app_entry` maps to a `.desktop` entry. ADR-0054
-- decision 5 reserved this caller before the capability existed.
local focused_icon = icon {
    name = computed({ oblisk.applications, oblisk.workspaces }, function(applications, workspaces)
        local client = focused(workspaces)
        local entry = util.app_entry(applications, client and client.class)
        return (entry and entry.icon) or ""
    end),
    size = theme.icon.lg,
    align_v = "Center",
}

-- No pill or button: `ActiveWindow.qml` puts the icon and title directly on the bar. No ground
-- behind them makes the centre read as a caption rather than one more control. The old button only
-- existed as the click target for the removed fetch.
return row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.sm,
    -- Nothing focused means nothing to caption. Hiding the row returns its width and spacing
    -- because invisible children cost neither (`layout::scene`'s row arm).
    visible = oblisk.workspaces:map(function(workspaces)
        return focused(workspaces) ~= nil
    end),
    children = {
        focused_icon,
        cell(oblisk.workspaces:map(function(workspaces)
            local client = focused(workspaces)
            return util.truncate(client and client.title or "", TITLE_LIMIT)
        end), theme.FG, theme.font.sm, { align_v = "Center" }),
    },
}
