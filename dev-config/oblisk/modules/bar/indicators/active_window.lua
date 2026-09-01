-- Mirrors ActiveWindow.qml: the focused window's icon and title, side by side in the centre zone.
--
-- Both halves read one push, `oblisk.workspaces.active_client`, so the caption tracks focus with
-- nothing asking it to. It used to be a click: the title lived in a `state` signal that a
-- `process.run("niri", {"msg", "-j", "focused-window"})` filled in, and until you clicked it the bar
-- read "click for the focused window". That was there to demonstrate `process.run` feeding
-- `json.decode` (ADR-0057), and it was the wrong module to demonstrate it in. ADR-0056 keeps
-- window *lists* out of `workspaces`, which is true and is not this: the focused window alone has
-- been in every snapshot since that ADR, as `active_client`, which is where the icon was already
-- getting `class`. Shelling out to niri for a string the supervisor had already pushed bought a
-- worse answer, a subprocess per click, and a request counter to throw away the replies that landed
-- out of order.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- The same budget `modules/bar/indicators/media.lua` uses, because the two share the centre zone
-- one at a time and a title that changed width when the player stopped would move the whole bar.
--
-- A character budget rather than the bounded box `components/cell.lua` argues for, and the argument
-- there is right in general: "WWWW" and "iiii" are the same four characters at twice the width.
-- This node has to stay content-sized anyway, because the centre zone's midpoint is its content's
-- midpoint, and a box wide enough to elide against is a box that fixes the zone's width.
local TITLE_LIMIT = 44

-- `nil` until the first snapshot, and `nil` again whenever nothing holds focus, which the
-- supervisor sends as an absent key rather than a null (`workspaces/controller.rs`).
local function focused(workspaces)
    return workspaces and workspaces.active_client
end

-- The second `oblisk.applications` consumer (ADR-0061). `active_client.class` is a toplevel's
-- `app_id` (ADR-0056 decision 5), which is the spelling `util.app_entry` maps onto a `.desktop`
-- entry. Before that capability existed there was nowhere for an `app_id` to become an icon, which
-- is the caller ADR-0054 decision 5 said would arrive one day.
local focused_icon = icon {
    name = computed({ oblisk.applications, oblisk.workspaces }, function(applications, workspaces)
        local client = focused(workspaces)
        local entry = util.app_entry(applications, client and client.class)
        return (entry and entry.icon) or ""
    end),
    size = theme.icon.lg,
    align_v = "Center",
}

-- No pill and no button. `ActiveWindow.qml` puts the icon and the title straight on the bar with no
-- ground behind them, which is what makes the centre read as a caption rather than one more
-- control. The button that used to wrap the title was the click target for the fetch above and had
-- nothing to do once the title stopped needing one.
return row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.sm,
    -- Nothing focused means nothing to caption. Hiding the row rather than drawing an empty one
    -- also gives the zone its width back, since an invisible child costs its parent no space and no
    -- spacing (`layout::scene`'s row arm).
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
