-- Mirrors Bar.qml.
--
local theme = require("config.theme")
local left = require("modules.bar.left_side")
local center = require("modules.bar.center_side")
local right = require("modules.bar.right_side")

-- Three zones at 46/13/41. Each is a fixed-width row distributing its own spare space by its own
-- `align_h`. That was once the only way to centre anything here, because a `Fill` child took the
-- parent's whole budget rather than the remainder, so the flexbox trick of two `Fill` spacers
-- failed: both spacers took the full width and the middle zone was pushed off the end. `scene.rs`
-- now sizes a `Fill` child from what its siblings leave, so the trick works and the percentages
-- below are a workaround kept past its cause. See the note further down before rewriting them.

-- The sides are equal because that is what makes the middle a centre. A 30/40/30 split with the
-- modules this bar carries put the right zone over its 576px and ran the battery off the edge of a
-- 1920px output; widening only the right zone would have fixed the overflow and moved the clock off
-- centre, since a centre zone is only centred while it is symmetric about the middle.

-- Which module sits in which zone is `~/.config/quickshell`'s arrangement rather than this file's
-- own: power menu, updates, keyboard, battery, launcher and workspaces on the left; the media
-- widget or the focused window's title in the middle; the status indicators, the tray and the
-- clock on the right. It is worth copying because it is a layout someone has actually lived with,
-- and because the two zones that grow on their own -- the tray, which follows however many
-- `StatusNotifierItem`s are registered, and the workspace strip -- end up on opposite sides of the
-- bar rather than fighting each other for one side's budget.

-- The split has moved three times, every time measured rather than guessed, and every time because
-- a module changed shape rather than because the numbers were wrong:
--
--   40/20/40 -> 43/14/43  a 20% centre stopped holding a clock and a date and started holding one
--                         module at a time (`center_side.lua`)
--   43/14/43 -> 44/13/43  the tray began laying out horizontally and needed real width
--   44/13/43 -> 46/13/41  the workspace strip became a button per workspace instead of one cell
--
-- The sides are no longer equal, and that is a real cost paid deliberately rather than an oversight.
-- Equal sides are what keep the middle zone's midpoint on the middle of the bar, and at these
-- module widths they are arithmetically impossible: the left zone needs 845px at worst case and two
-- of those plus a 229px centre is 1919px on a 1904px bar. The asymmetry puts the centre's midpoint
-- at 52.5% instead of 50%, about forty-eight pixels right of true centre on a 1920px output. That
-- is the cheapest thing available to give up, and it is also the clearest signal yet that three
-- fixed percentages is the wrong model for this bar.
--
-- The gap underneath all of this is closed. A `Fill` child is now sized from the remainder its
-- siblings leave, so a zone can borrow from its neighbour: two `Fill` spacers around a content-sized
-- centre zone centre it exactly, at any module width, and the arithmetic above stops mattering.
--
-- Left standing anyway, for now. Every number in this comment was measured against real module
-- widths on a real output, and swapping three fixed percentages for two spacers moves every module
-- on the bar at once. That wants a live session to look at, not a passing edit.
return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    -- Reserves screen area along the anchored edge, derived from the height the compositor
    -- actually configures, so it stays right if this changes.
    exclusive = true,
    -- Not what a real bar wants, and deliberate: this file exercises the engine, so it opts
    -- into Phase 21 item 2's focus path rather than leaving it dark. The cost is that clicking
    -- the bar takes keyboard focus off the window behind it.
    keyboard_interactivity = "OnDemand",
    width = "Fill",
    height = 34,
    child = row {
        width = "Fill",
        height = "Fill",
        background = theme.BG,
        padding = { left = 8, right = 8, top = 4, bottom = 4 },
        children = { left, center, right },
    },
}
