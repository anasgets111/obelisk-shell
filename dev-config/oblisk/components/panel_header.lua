-- A panel's title and its own close affordance, so a window does not depend on the button that
-- opened it to close it again. `modules/bar/panels/settings.lua` had no way to close itself before
-- this; the only path back was re-clicking the bar button that opened it.
--
-- The title takes `width = "Fill"`, which pins the close button to the far edge in one property.
-- That is new: a `Fill` child on a row's main axis used to resolve against the parent's whole
-- content width regardless of its siblings, so a `Fill` title took the entire card and pushed the
-- button out of it. `scene.rs` now sizes a `Fill` child from what its siblings leave, and this file
-- is the first thing in the config to spend that.
--
-- The title elides rather than pushing the button, because `Fill` gives it a real box and
-- `components/cell.lua` declares `elide = "End"` on every cell. A long panel name is cut with an
-- ellipsis at the edge of the space the button does not want.
local theme = require("config.theme")
local cell = require("components.cell")
local icons = require("config.icons")
local icon_button = require("components.icon_button")

return function(title, on_close)
    return row {
        width = "Fill",
        spacing = theme.spacing.sm,
        align_v = "Center",
        children = {
            cell(title, theme.FG, theme.font.lg, { width = "Fill" }),
            icon_button(icons.close, on_close, { size = theme.control.sm, icon_size = theme.icon.sm }),
        },
    }
end
