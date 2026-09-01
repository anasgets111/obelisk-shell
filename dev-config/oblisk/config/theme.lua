-- Catppuccin Mocha, because a bar needs a palette and an invented one would just be a worse
-- version of a palette someone already balanced.
--
-- Its own file because every module in `shell.lua` reads these values and none of them owns them,
-- which is the first thing a config of any size wants to move out. Editing this file recolours the
-- bar in place: ADR-0047 points `require` at this directory and drops the module cache before each
-- re-evaluation, so a required file is re-read rather than served stale.
--
-- Neither half of that worked before Phase 26, and the failure was quiet both times. `package.path`
-- was Lua's compiled-in default, so a `require` searched `/usr/local/share/lua/5.4/` and then the
-- process's working directory, which nothing sets. An edit to a file that did resolve then reached
-- a cached copy and changed nothing on screen.
--
-- This was ten colours until the scales below arrived. The ten were enough while every module was
-- one pill of text; they stopped being enough the moment two files had to agree on how far apart
-- two things sit. `Config/Theme.qml` in the config this mirrors is 272 lines and almost all of it
-- is this: named steps, so a panel and the bar round their corners the same amount without either
-- knowing the number.
local theme = {}

-- ## The responsive scale
--
-- `s(base)` is `Theme.qml`'s own function, and it does one thing: a 4K panel should not be a 1080p
-- panel's pixel count. Everything below that is not a colour goes through it.
--
-- Read once, at evaluation. `oblisk.screens` is the one signal available from a generation's first
-- evaluation (§ 2.15) rather than after the first snapshot, which is what makes a plain `:get()`
-- here work at all -- every capability under `oblisk` reads nil at this point and would scale the
-- whole config to the fallback.
--
-- ponytail: this does not follow a hotplug. A token is a number baked into a node's property map at
-- evaluation time, and nothing re-evaluates when an output appears. Moving the shell to a different
-- monitor mid-session leaves it scaled for the old one until the config is touched. The upgrade is
-- for `s()` to return a signal rather than a number, which every consumer already accepts (§ 5.1),
-- and which costs a `computed` per token; not worth it until a second monitor is actually in play.
local function main_screen()
    -- Guarded, because this is the only module-scope read of a signal in the whole config and so
    -- the only file that cares whether the `oblisk` namespace has been built yet. Everything else
    -- reads a capability inside a node property, which is resolved long after evaluation. The
    -- component tests in `renderer/src/lua/mod.rs` load `components/` through a bare `Loader` with
    -- no namespace at all, and an unguarded index there takes down every one of them with a
    -- traceback into this file rather than into whatever they were testing.
    local screens = oblisk and oblisk.screens and oblisk.screens:get() or {}
    -- The first output, which is what `MonitorService.activeMainScreen` resolves to on a single
    -- head and is a guess on more than one. The reference config picks by the compositor's focused
    -- output; nothing here carries that yet.
    return screens[1]
end

-- 1080p, which is `Theme.qml`'s own fallback and, more usefully, the height this config's numbers
-- were written against. Deliberately not a separate "unscaled" branch: an unknown output and a
-- 1080p output must produce identical tokens, or the bar a test measures is not the bar a session
-- draws. `oblisk.screens` is empty during evaluation in `socket.rs`'s harness and populated during
-- evaluation in a real session (`wayland/mod.rs` seeds it first, ADR-0041 decision 2), so the
-- two paths differ by exactly this value.
local FALLBACK_HEIGHT = 1080

local SCALE = (function()
    local screen = main_screen()
    -- Logical height, not physical. Dividing by `scale` first is what stops a 2x 4K panel from
    -- being read as a 2160px-tall desktop and scaled up twice, which is the note
    -- `Config/Theme.qml`'s own `internal.dpr` carries.
    local logical_height = FALLBACK_HEIGHT
    if screen ~= nil then
        logical_height = screen.height / math.max(screen.scale or 1, 0.1)
    end
    local factor = 0.9 + ((logical_height - 1080) / 360) * 0.1
    return math.max(0.75, math.min(1.4, factor))
end)()

-- `min` is the floor a token refuses to shrink past on a small screen, not a default. A 10px label
-- scaled to 0.75 is 8px and still legible; an 8px icon at 0.75 is 6px and is a smudge.
function theme.s(base, min)
    return math.max(min or 0, math.floor(base * SCALE + 0.5))
end
local s = theme.s

theme.scale = SCALE

-- ## Colour helpers
--
-- Both take and return this engine's own colour string (`#RRGGBB` or `#RRGGBBAA`, checked in
-- `layout::node::parse_hex_color`), so a result feeds straight back into `background` with no
-- conversion step and no colour type of its own.

local function channels(hex)
    local digits = hex:gsub("^#", "")
    return tonumber(digits:sub(1, 2), 16) or 0, tonumber(digits:sub(3, 4), 16) or 0, tonumber(digits:sub(5, 6), 16) or 0
end

-- The alpha byte, defaulting to opaque, because `#RRGGBB` is a legal colour here and half the
-- palette is written that way.
local function alpha_of(hex)
    local digits = hex:gsub("^#", "")
    return (tonumber(digits:sub(7, 8), 16) or 255) / 255
end

-- Replaces the alpha rather than multiplying into it, matching `Theme.qml`'s `withOpacity`: every
-- caller there passes an opaque base and one of the `opacity` steps below, and compounding would
-- make `withOpacity(bgSubtle, 0.5)` mean something different from `withOpacity(bgColor, 0.5)`.
function theme.with_opacity(hex, alpha)
    local r, g, b = channels(hex)
    return string.format("#%02x%02x%02x%02x", r, g, b, math.floor(math.max(0, math.min(1, alpha)) * 255 + 0.5))
end

-- Blends toward white by `amount`, which is `Qt.lighter`'s job in the reference config. Lua has no
-- HSV here and this does not need one: the two call sites want an elevated surface above `BG`, and
-- a linear step toward white is what that reads as at these small amounts.
local function lighten(hex, amount)
    local r, g, b = channels(hex)
    local mix = function(c)
        return math.floor(c + (255 - c) * amount + 0.5)
    end
    return string.format("#%02x%02x%02xff", mix(r), mix(g), mix(b))
end

-- WCAG relative luminance, then black or white against it. `Theme.qml`'s `textContrast`, digit for
-- digit including the 0.179 threshold, because a button that picks its own background needs to pick
-- its own foreground or a caller has to remember to do both.
-- Composited over `BG` before it is measured, which the reference does not have to do because
-- nothing it hands this is translucent. Everything on this bar is. A control at 42% alpha is not
-- the colour of its swatch, and measuring the swatch is why every hover flipped its glyph to black:
-- `ON_HOVER` is a light purple, `text_contrast` read it as light, and the 45% of it that actually
-- reaches the screen is dark. Opaque callers are unaffected, since the blend is a no-op at alpha 1.
function theme.text_contrast(hex)
    local function linear(byte)
        local c = byte / 255
        if c <= 0.04045 then
            return c / 12.92
        end
        return ((c + 0.055) / 1.055) ^ 2.4
    end
    local r, g, b = channels(hex)
    local mix = alpha_of(hex)
    if mix < 1 then
        local gr, gg, gb = channels(theme.BG)
        r, g, b = r * mix + gr * (1 - mix), g * mix + gg * (1 - mix), b * mix + gb * (1 - mix)
    end
    local luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    return luminance > 0.179 and "#000000ff" or "#ffffffff"
end

-- ## Colours
--
-- The ten originals keep their names. Every module in this config reads them, and renaming them to
-- match `Theme.qml`'s `textActiveColor`/`bgElevated` spelling would be a rename across thirty files
-- that buys nothing: these are the same eleven Catppuccin swatches either way.
theme.BG      = "#1e1e2eff"
theme.SURFACE = "#313244ff"
-- Catppuccin surface1, one step up from SURFACE. The hover shade: a module that highlights under
-- the pointer reads this rather than inventing its own lighter blue (ADR-0062).
theme.HOVER   = "#45475aff"
theme.FG      = "#cdd6f4ff"
-- Catppuccin subtext0, which is `Theme.qml`'s `textInactiveColor`. This was overlay0 (#6c7086),
-- two steps darker, and it is why every secondary label on this bar read as switched off rather
-- than as secondary: the keyboard layout and the date were painted at the contrast the mirror
-- reserves for a disabled control.
theme.DIM     = "#a6adc8ff"
-- Mauve, not blue. `Theme.qml`'s `activeColor` is #cba6f7 and every accent in the mirror is that
-- one swatch; #89b4fa was this config's own invention and made the two bars read as different
-- themes before anything else about them differed. `MAUVE` below is the same value under the name
-- of the colour rather than the name of the role, which is what a caller wanting the swatch reads.
theme.ACCENT  = "#cba6f7ff"
theme.GREEN   = "#a6e3a1ff"
theme.YELLOW  = "#f9e2afff"
theme.PEACH   = "#fab387ff"
theme.RED     = "#f38ba8ff"
theme.MAUVE   = "#cba6f7ff"
-- Catppuccin blue, which used to be `ACCENT`. Kept because two modules want a blue specifically
-- rather than whatever the active colour happens to be.
theme.BLUE    = "#89b4faff"
-- Surface2 and the hover tint the mirror lifts a control to, its `inactiveColor` and `onHoverColor`.
-- Both exist to be handed to `with_opacity` below rather than painted directly.
theme.INACTIVE = "#494d64ff"
theme.ON_HOVER = "#a28dcdff"
theme.DISABLED = "#232634ff"

-- The derived layer, which is what `Theme.qml` spends most of its length on. Each is a step off one
-- of the eleven above rather than a new swatch, so the palette stays eleven colours and a
-- scheme swap stays eleven edits.
theme.ELEVATED       = lighten(theme.BG, 0.12)
theme.ELEVATED_HOVER = lighten(theme.BG, 0.18)
-- Dimmer than DIM, for a label that is present but off. `textDisabled` in the reference, which is
-- `withOpacity(textInactiveColor, opacityMedium)` there and here.
theme.TEXT_OFF       = theme.with_opacity(theme.DIM, 0.35)
theme.BORDER         = theme.with_opacity(theme.SURFACE, 0.75)
theme.BORDER_SUBTLE  = theme.with_opacity(theme.SURFACE, 0.35)
-- The translucent card ground every popup and panel in this config already hand-wrote as
-- `"#181825ee"`. Named here so the next one does not invent a twelfth swatch.
theme.GLASS          = "#181825ee"
theme.GLASS_CONTENT  = theme.with_opacity(theme.ELEVATED, 0.46)
theme.GLASS_HOVER    = theme.with_opacity(theme.ELEVATED_HOVER, 0.62)
theme.ACCENT_SUBTLE  = theme.with_opacity(theme.ACCENT, 0.15)
theme.ACCENT_MEDIUM  = theme.with_opacity(theme.ACCENT, 0.35)

-- ## The glass layer
--
-- What the mirror's chrome is actually made of, and the reason its bar looks like a bar rather than
-- a strip of solid boxes: nothing on it is opaque. The bar is the background colour at half alpha
-- over the wallpaper, each control is surface2 at 0.42, and every control carries a hairline border
-- of near-white at 0.18 that is what separates one from the next. Painting those same controls
-- opaque, which is what this config did, turns a row of floating pills into a row of filled
-- rectangles, and no amount of getting the radius right fixes it.
--
-- Alpha reaches the compositor. The bar is a layer surface over the wallpaper panel, so a
-- half-alpha ground composites against the image below it rather than against black.
theme.GLASS_SURFACE       = theme.with_opacity(theme.BG, 0.5)
theme.GLASS_CONTROL       = theme.with_opacity(theme.INACTIVE, 0.42)
-- 0.45, not the mirror's 0.68. The reference lifts an opaque control to an opaque tint; here the
-- lift lands on a glass control over a wallpaper, and at 0.68 the hover was the brightest thing on
-- the bar. Low enough that the glyph stays white through the transition, which is what stops a
-- hover from also being a colour inversion.
theme.GLASS_CONTROL_HOVER = theme.with_opacity(theme.ON_HOVER, 0.45)
theme.GLASS_BORDER        = theme.with_opacity(theme.FG, 0.18)
theme.GLASS_BORDER_HOVER  = theme.with_opacity(theme.FG, 0.34)
-- The alert ground `rescue` and `privacy` both hand-wrote. Same value, one name.
theme.ALERT_BG       = "#45253aff"

-- ## Opacity steps
--
-- `opacity` is a base property now (§ 5.1) and multiplies down the subtree, so a whole disabled
-- control is one property rather than a dimmer colour on each of its parts.
theme.opacity = {
    disabled = 0.5,
    muted    = 0.7,
    solid    = 0.6,
    strong   = 0.8,
    full     = 0.95,
}

-- ## Scales
--
-- Named steps rather than numbers at the call site, which is the whole point: `spacing.sm` is the
-- same 8px in the bar and in a panel because both name the step, and changing it changes both.
theme.spacing = {
    xs = s(4, 2),
    sm = s(8, 4),
    md = s(12, 8),
    lg = s(16, 10),
    xl = s(24, 16),
}

theme.font = {
    xs   = s(10, 8),
    sm   = s(12, 10),
    md   = s(14, 12),
    lg   = s(16, 14),
    xl   = s(20, 16),
    xxl  = s(28, 20),
    hero = s(48, 32),
}

theme.radius = {
    xs = s(3, 2),
    sm = s(6, 4),
    md = s(12, 8),
    lg = s(18, 12),
    xl = s(40, 20),
}

theme.icon = {
    xs = s(12, 10),
    sm = s(14, 12),
    md = s(18, 14),
    lg = s(24, 18),
    xl = s(32, 24),
}

-- Control heights, which is what makes a toggle and a button beside it line up without either
-- naming a pixel count.
theme.control = {
    xs = s(18, 16),
    sm = s(24, 20),
    md = s(28, 24),
    lg = s(34, 28),
    xl = s(44, 36),
}

theme.border_width = 1

-- ## Surface geometry
--
-- The sizes a surface declares, which are neither spacing nor a control height and were each
-- written twice before this: once in the module and once in whatever opened it.
-- `Theme.qml`'s `basePanelHeight`, so the bar is 38px at 1080p rather than the 34 it was. The four
-- extra pixels are not cosmetic: an `item` below is 31px tall and the mirror's bar is sized to hold
-- one with a hair of margin, which is what makes its controls read as sitting *in* the bar. At 34
-- the same control either touched both edges or had to shrink.
theme.bar_height          = s(42, 28)

-- ## The item scale
--
-- One control on the bar: a circular icon button, a battery pill, the clock. `Theme.qml` keeps this
-- separate from `control` above because they answer different questions -- `control` is how tall a
-- thing inside a panel is, `item` is how tall a thing on the bar is -- and the two diverged the
-- moment the bar got its own height.
--
-- `item_radius` is half of `item_height` by construction rather than by coincidence, which is what
-- makes a square item a circle. It is written as its own `s()` call, matching the mirror, because
-- the two round independently and 18 vs 15.5 is the difference between a circle and a very round
-- square at some scales.
theme.item_height = s(34, 20)
theme.item_width  = s(34, 20)
theme.item_radius = s(18, 6)
-- A workspace dot, deliberately smaller than an item. Twelve full-size controls is 416px of a
-- 1920px bar, and a workspace is not a control in the sense the others are: it carries one digit
-- and one state. At `control.sm` the whole strip costs about 300px and stops being the widest thing
-- on the left.
theme.workspace_size = theme.control.sm
-- Wide enough for a glyph and "100%". The mirror's `batteryPillWidth`, and the one module on the
-- bar that is a pill rather than a circle.
theme.battery_pill_width = s(80, 60)
-- The width the volume control grows to when the pointer is on it, so the percentage has somewhere
-- to go. Collapsed it is `item_width`.
theme.volume_expanded_width = s(120, 90)
-- `Theme.qml` gives each panel its own width (`networkPanelWidth: 340`, `bluetoothPanelWidth:
-- 360`). One number here, because this config puts every bar panel in one shared `popup`
-- (`modules/shell/panel_host.lua`) and a surface has one size.
theme.panel_width         = s(340, 280)
theme.panel_height        = s(400, 300)
theme.panel_gap           = 4
theme.notification_width  = s(380, 300)
theme.notification_height = s(96, 80)
theme.osd_width           = s(260, 220)
theme.osd_height          = s(44, 36)
theme.launcher_row_height = s(30, 24)

return theme
