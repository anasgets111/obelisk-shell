local theme = require("config.theme")
local left = require("modules.bar.left")
local center = require("modules.bar.center")
local right = require("modules.bar.right")

-- Three zones at 40/20/40. Each is a fixed-width row distributing its own spare space by its own
-- `align_h`, which is the only way to centre anything here: a `Fill` child takes the parent's whole
-- budget rather than the remainder (`resolve_non_content` in scene.rs), so the flexbox trick of two
-- `Fill` spacers does not work, they both take the full width.

-- The sides are equal because that is what makes the middle a centre. A 30/40/30 split with the
-- modules this bar carries put the right zone over its 576px and ran the battery off the edge of a
-- 1920px output; widening only the right zone would have fixed the overflow and moved the clock off
-- centre, since a centre zone is only centred while it is symmetric about the middle.

-- The tray is in the left zone and most status-shaped modules are on the right, which reads
-- backwards until you notice the tray is the one module with no width of its own: it grows with
-- however many `StatusNotifierItem`s happen to be registered.

-- Adding `power` to the battery pill put the right zone over its 768px again and clipped `lock`
-- off the edge, and trimming two text modules did not buy back enough. Two things moved instead.
-- `notifications` left the bar entirely: the `notification_area` surface
-- (`modules/notification/popup.lua`) already draws the same newest notification, so the bar copy
-- was the one place that information appeared twice.
-- `brightness` moved to the left zone, which has the room and no reason to prefer the right.

-- Worth naming rather than fixing again by shaving characters: neither side can grow. The sides
-- are equal because that is what makes the middle a centre, and a 20% centre holding a clock and a
-- date has slack that a 40% side cannot borrow. Every module added from here costs another module
-- its place until this engine has a real space-between.
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
