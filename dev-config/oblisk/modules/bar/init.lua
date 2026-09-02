-- Mirrors Bar.qml.
--
local theme = require("config.theme")
local ui_state = require("lib.ui_state")
local left = require("modules.bar.left_side")
local center = require("modules.bar.center_side")
local right = require("modules.bar.right_side")

-- Three zones: two that grow and a middle that does not. Each side takes `width = "Fill"` and
-- distributes its own spare space by its own `align_h`, so the two sides always hold equal shares
-- of whatever the centre leaves and the centre's midpoint is the bar's midpoint by construction.
--
-- This was three fixed percentages until now, and the percentages were a workaround for an engine
-- bug rather than a layout choice. A `Fill` child used to resolve against the parent's whole
-- content width regardless of its siblings, so the obvious flexbox shape failed: every `Fill` zone
-- took the full bar and the ones after it were pushed off the end. `scene.rs` now sizes a `Fill`
-- child from the remainder its siblings leave, which is what makes the line below work at all.
--
-- Which module sits in which zone is `~/.config/quickshell`'s arrangement rather than this file's
-- own: power menu, updates, keyboard, battery, launcher and workspaces on the left; the media
-- widget or the focused window's title in the middle; the status indicators, the tray and the
-- clock on the right. It is worth copying because it is a layout someone has actually lived with,
-- and because the two zones that grow on their own -- the tray, which follows however many
-- `StatusNotifierItem`s are registered, and the workspace strip -- end up on opposite sides of the
-- bar rather than fighting each other for one side's budget.
--
-- What the rewrite retires, recorded because the numbers cost real measurement to find. The split
-- moved three times, every time because a module changed shape:
--
--   40/20/40 -> 43/14/43  a 20% centre stopped holding a clock and a date and started holding one
--                         module at a time (`center_side.lua`)
--   43/14/43 -> 44/13/43  the tray began laying out horizontally and needed real width
--   44/13/43 -> 46/13/41  the workspace strip became a button per workspace instead of one cell
--
-- The last of those gave up a centred centre to buy the left zone room: 46/41 put the centre's
-- midpoint at 52.5%, about forty-eight pixels right of true on a 1920px output. Two `Fill` sides
-- give that back and cost nothing, because a side that needs more now borrows from the other
-- instead of from a number written here.
--
-- One failure mode survives and is worth naming. `row` still does not shrink a child to make its
-- siblings fit, so a left zone whose modules exceed its half of the remainder paints past its own
-- edge exactly as a full 46% zone did. What changed is that the budget follows the content instead
-- of a constant: `socket.rs`'s `the_shipped_dev_configs_bar_zones_hold_their_modules_without_
-- overflowing` is still the thing that catches it.
return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    -- Reserves screen area along the anchored edge, derived from the height the compositor
    -- actually configures, so it stays right if this changes.
    exclusive = true,
    -- `"None"` until a bar panel is open, `"Exclusive"` for exactly as long as one is. Bound rather
    -- than constant because `keyboard_interactivity` is one of the four `panel` fields layer-shell
    -- permits changing on a mapped surface, so a `Signal` here resolves as a value change instead
    -- of a generation swap (ADR-0044 decision 1, and `layout::node::parse_keyboard_interactivity`).
    -- `Modules/Shell/MainScreen.qml` binds `WlrLayershell.keyboardFocus` to a panel-derived boolean
    -- the same way, and for the same reason this one is not bound to something narrower.
    --
    -- Bound, and not simply `"OnDemand"`, because of what niri does with that: it gives an
    -- `on_demand` layer surface keyboard focus when the surface *maps*, with no click involved, so
    -- this line spent a while as `"OnDemand"` taking focus off whatever was focused at startup and
    -- making the session unusable. A surface that asks for the keyboard only while a panel is up
    -- never maps in that state, which is why the same property is safe here in a form it was not
    -- safe in as a constant.
    --
    -- **Why `panel_open` and not `network.password_ssid`**, which is the thing that actually wants
    -- the keyboard. Changing this on a *mapped* surface makes the compositor re-evaluate keyboard
    -- focus, and that breaks `panel_host`'s popup grab: niri dismissed the panel in the same frame
    -- the password field was armed, so the prompt appeared and vanished together. Bound to
    -- `panel_open` the change lands on the pass that opens the popup instead -- the bar is earlier
    -- in the surface list than `panel_host`, so `apply_spec_change` sends this before `show_popup`
    -- takes the grab -- and nothing touches the layer surface again while the panel is up.
    --
    -- The cost is that any open panel takes the keyboard, not just the one asking for a password.
    -- That is the honest trade rather than a shortcut: `panel_host` is a grabbing popup, so it
    -- already swallows every pointer event and closes on the first click elsewhere. A surface that
    -- owns the pointer and not the keyboard is the odder of the two.
    --
    -- `"Exclusive"` rather than `"OnDemand"`, because the field must be typable without first
    -- clicking it: the engine arms the sole `secure_submit` field in reach when the compositor
    -- hands this surface keyboard focus, and the field on `panel_host` is in reach because a shown
    -- popup joins its parent's focus scope (`layout::secure_submit`'s `sole_secure_submit_in_scope`).
    -- The keys arrive here; the entry belongs to the popup. The prompt appearing later, under a
    -- focus that already arrived, is armed by `wayland::input`'s
    -- `arm_secure_focus_if_the_scope_now_declares_one` -- there is no second `enter` to do it.
    keyboard_interactivity = ui_state.panel_open:map(function(open)
        return open and "Exclusive" or "None"
    end),
    width = "Fill",
    height = theme.bar_height,
    -- Translucent, and vertically unpadded. Both are the mirror's, and both are load-bearing.
    --
    -- `GLASS_SURFACE` is the background colour at half alpha, so the bar composites against the
    -- wallpaper panel below it instead of covering it. An opaque ground here is most of why this bar
    -- read as a black strip with boxes on it while the one it mirrors reads as glass.
    --
    -- The vertical padding is gone because an item is 31px and the bar is 38px: `align_v = "Center"`
    -- already leaves 3.5px above and below, and the 4px of padding that used to be here made the
    -- content box 30px, one pixel short of the thing it had to hold. Horizontal padding stays and
    -- grows to `spacing.md`, which is `Theme.qml`'s `panelMargin`.
    child = row {
        width = "Fill",
        height = "Fill",
        background = theme.GLASS_SURFACE,
        padding = { left = theme.spacing.md, right = theme.spacing.md },
        children = { left, center, right },
    },
}
