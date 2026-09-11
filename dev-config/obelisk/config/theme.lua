-- Catppuccin Mocha. An invented palette would be a worse version of one already balanced.
-- Shared tokens live here because every `shell.lua` module reads them and no module owns them.
-- ADR-0047 clears the module cache before each re-evaluation, so edits recolour the bar in place.
local theme = {}

-- ## The responsive scale
--
-- `s(base)` is `Theme.qml`'s function. It scales non-colour tokens from 1080p values so a 4K panel
-- does not use 1080p pixels.
-- Read once during evaluation: `obelisk.screens` is the only signal available then (§ 2.15), while
-- capabilities read nil until the first snapshot, so other module-scope reads would use fallback.
--
-- ponytail: hotplug is not followed. Tokens are numbers baked into node maps at evaluation;
-- moving monitors leaves the old scale until touched. Upgrade `s()` to return a signal, which
-- consumers already accept (§ 5.1), at one `computed` per token when a second monitor matters.
local function main_screen()
    -- The only module-scope signal read. Component tests in `renderer/src/lua/mod.rs` load
    -- `components/` through a bare `Loader` with no `obelisk` namespace, so other capabilities are
    -- read inside node properties after evaluation.
    local screens = obelisk and obelisk.screens and obelisk.screens:get() or {}
    -- First output, matching `MonitorService.activeMainScreen` on one head; focused-output
    -- selection is not available here.
    return screens[1]
end

-- `Theme.qml`'s 1080p fallback and this config's design height. Unknown and 1080p outputs share it,
-- so tests and sessions measure the same bar; the `socket.rs` harness has empty screens, while real
-- sessions seed them first in `wayland/mod.rs` (ADR-0041 decision 2).
local FALLBACK_HEIGHT = 1080

-- Logical width of the same output, for the one token measured across the screen rather than down
-- it. Same 1080p-era fallback reasoning as `FALLBACK_HEIGHT`.
local MAIN_WIDTH = (function()
    local screen = main_screen()
    if screen ~= nil and screen.width ~= nil then
        return screen.width
    end
    return 1920
end)()

-- Only for the aspect ratio behind `title_limit`; every vertical token comes from `SCALE` below.
local MAIN_HEIGHT = (function()
    local screen = main_screen()
    if screen ~= nil and screen.height ~= nil then
        return screen.height
    end
    return FALLBACK_HEIGHT
end)()

local SCALE = (function()
    local screen = main_screen()
    -- Use `screen.height` directly. `wayland/output.rs` reports logical pixels, so a 3840x2160
    -- panel at scale 2 arrives as 1080; `screen.scale` is not another divisor.
    local logical_height = FALLBACK_HEIGHT
    if screen ~= nil and screen.height ~= nil then
        logical_height = screen.height
    end
    local factor = 0.9 + ((logical_height - 1080) / 360) * 0.1
    return math.max(0.75, math.min(1.4, factor))
end)()

-- `min` is a small-screen floor, not a default: a 10px label at 0.75 is 8px and legible; an 8px
-- icon becomes an illegible 6px smudge.
function theme.s(base, min)
    return math.max(min or 0, math.floor(base * SCALE + 0.5))
end

local s = theme.s

-- ## Colour helpers
--
-- Helpers use the engine's `#RRGGBB`/`#RRGGBBAA` strings, checked by
-- `layout::node::parse_hex_color`, so results feed `background` directly.

local function channels(hex)
    local digits = hex:gsub("^#", "")
    return tonumber(digits:sub(1, 2), 16) or 0, tonumber(digits:sub(3, 4), 16) or 0, tonumber(digits:sub(5, 6), 16) or 0
end

-- Missing alpha means opaque; `#RRGGBB` is legal and half the palette uses it.
local function alpha_of(hex)
    local digits = hex:gsub("^#", "")
    return (tonumber(digits:sub(7, 8), 16) or 255) / 255
end

-- Replaces rather than multiplies alpha like `Theme.qml`'s `withOpacity`; otherwise
-- `withOpacity(bgSubtle, 0.5)` would differ from `withOpacity(bgColor, 0.5)`.
function theme.with_opacity(hex, alpha)
    local r, g, b = channels(hex)
    return string.format("#%02x%02x%02x%02x", r, g, b, math.floor(math.max(0, math.min(1, alpha)) * 255 + 0.5))
end

-- Linear blend toward white, the `Qt.lighter` equivalent used by two elevated-surface call sites;
-- HSV is unnecessary for these small steps above `BG`.
local function lighten(hex, amount)
    local r, g, b = channels(hex)
    local mix = function(c)
        return math.floor(c + (255 - c) * amount + 0.5)
    end
    return string.format("#%02x%02x%02xff", mix(r), mix(g), mix(b))
end

-- WCAG relative luminance and the mirror's 0.179 black/white threshold, matching `Theme.qml`'s
-- `textContrast`. Composite translucent colours over `BG` first. Measuring the swatch made every
-- hover glyph black: light-purple `ON_HOVER` at 45% alpha reaches the screen dark; alpha 1 is a
-- no-op. Callers get the matching foreground with the background.
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

-- ## Opacity steps
--
-- `opacity` is a base property (§ 5.1) that multiplies down the subtree, so disabling a control
-- needs one property rather than dimmer colours on each part.
theme.opacity                   = {
    subtle   = 0.15,
    light    = 0.25,
    medium   = 0.35,
    disabled = 0.5,
    muted    = 0.7,
    strong   = 0.8,
    full     = 0.95,
}

-- ## Colours
--
-- Keep the ten original names: matching `Theme.qml`'s `textActiveColor`/`bgElevated` would rename
-- thirty files for the same eleven Catppuccin swatches.
theme.BG                        = "#1e1e2eff"
theme.SURFACE                   = "#313244ff"
-- Catppuccin surface1, one step above SURFACE; pointer highlights use it instead of inventing blue
-- (ADR-0062).
theme.HOVER                     = "#45475aff"
theme.FG                        = "#cdd6f4ff"
-- Catppuccin subtext0, `Theme.qml`'s `textInactiveColor`. Overlay0 (#6c7086) made the keyboard
-- layout and date look disabled instead of secondary.
--
-- This is the mirror's one secondary-text colour: 73 uses against a single `textDisabled`. Faded
-- controls use node `opacity`; use a dimmer colour only when a QML line asks for one.
theme.DIM                       = "#a6adc8ff"
-- Mauve, not blue: the mirror's `activeColor` and every accent are #cba6f7. `MAUVE` exposes the
-- swatch by colour name.
theme.ACCENT                    = "#cba6f7ff"
theme.GREEN                     = "#a6e3a1ff"
theme.YELLOW                    = "#f9e2afff"
theme.PEACH                     = "#fab387ff"
theme.RED                       = "#f38ba8ff"
theme.MAUVE                     = "#cba6f7ff"
-- Catppuccin blue, formerly `ACCENT`; two modules need blue specifically.
theme.BLUE                      = "#89b4faff"
-- Mirror's `inactiveColor` and `onHoverColor`, passed to `with_opacity`, not painted directly.
theme.INACTIVE                  = "#494d64ff"
theme.ON_HOVER                  = "#a28dcdff"
theme.DISABLED                  = "#232634ff"

-- Derived steps from the eleven swatches, so a scheme swap remains eleven edits.
theme.ELEVATED                  = lighten(theme.BG, 0.12)
theme.ELEVATED_HOVER            = lighten(theme.BG, 0.18)
-- `textDisabled`: `withOpacity(textInactiveColor, opacityMedium)`, dimmer than DIM. The mirror uses
-- it once on `OToggle`'s disabled border; this config has no disabled toggle. It had spread to
-- thirty-one places at 35% alpha, so use `DIM` for secondary text.
theme.TEXT_OFF                  = theme.with_opacity(theme.DIM, theme.opacity.medium)
theme.BORDER                    = theme.with_opacity(theme.SURFACE, 0.75)
theme.BORDER_SUBTLE             = theme.with_opacity(theme.SURFACE, 0.35)
-- Shared translucent card ground for panels, menus, popups and cards. Mantle makes a card read as a
-- sheet above the bar rather than the same tone as it. The tunable alpha fell from 0.933 to 0.88
--
-- after compositor blur was added (ADR-0195); glass that cannot be seen through is a dark
-- rectangle.
theme.GLASS                     = theme.with_opacity("#181825", 0.88)
theme.GLASS_CONTENT             = theme.with_opacity(theme.ELEVATED, 0.46)
-- `glassInputColor`: a text field sits on the base tone, not the elevated one, so a search box reads
-- as a well cut into the card it shares an edge with rather than a second card.
theme.GLASS_INPUT               = theme.with_opacity(theme.BG, 0.62)
theme.GLASS_HOVER               = theme.with_opacity(theme.ELEVATED_HOVER, 0.62)
theme.ACCENT_SUBTLE             = theme.with_opacity(theme.ACCENT, theme.opacity.subtle)
theme.ACCENT_LIGHT              = theme.with_opacity(theme.ACCENT, theme.opacity.light)
theme.ACCENT_MEDIUM             = theme.with_opacity(theme.ACCENT, theme.opacity.medium)
-- Hover for an opaque `ACCENT` ground. The three alpha tints cannot lift an opaque colour; the
-- mirror's `OButton` primary variant lightens it instead.
theme.ACCENT_HOVER              = lighten(theme.ACCENT, 0.16)
-- The same lift for the one opaque `RED` ground: `ScreenRecorderPanel.qml`'s stop button, whose
-- `bgColor: Theme.critical` goes through `OButton`'s primary hover the same way accent does.
theme.RED_HOVER                 = lighten(theme.RED, 0.16)
-- Mirror's `bgSubtle`, used as the plate behind a notification card's application icon.
theme.BG_SUBTLE                 = theme.with_opacity(theme.BG, theme.opacity.subtle)

-- ## The glass layer
--
-- The chrome is translucent: the bar is background at 0.5 over wallpaper, controls are surface2 at
-- 0.42, and near-white 0.18 borders separate them. Painting controls opaque turned floating pills
-- into filled rectangles; radius could not fix it. Alpha reaches the compositor, so the layer
-- surface composites against the wallpaper, not black.
theme.GLASS_SURFACE             = theme.with_opacity(theme.BG, 0.5)
theme.GLASS_CONTROL             = theme.with_opacity(theme.INACTIVE, 0.42)
-- 0.45, not the mirror's 0.68: on a glass control over wallpaper, 0.68 made hover the bar's
-- brightest element. 0.45 keeps the glyph white instead of inverting it.
theme.GLASS_CONTROL_HOVER       = theme.with_opacity(theme.ON_HOVER, 0.45)
theme.GLASS_BORDER              = theme.with_opacity(theme.FG, 0.18)
theme.GLASS_BORDER_HOVER        = theme.with_opacity(theme.FG, 0.34)
theme.ALERT_BG                  = "#45253aff"
-- `modalScrimColor` is 0.45 rather than the mirror's 0.88: it lays over wallpaper, where 0.88 is a
-- blackout.
theme.SCRIM                     = theme.with_opacity(theme.BG, 0.45)

-- ## Scales
--
-- Named steps keep `spacing.sm` at the same 8px in bar and panel, and let one edit change both.
theme.spacing                   = {
    xs = s(4, 2),
    sm = s(8, 4),
    md = s(12, 8),
    lg = s(16, 10),
    xl = s(24, 16),
}

-- `components/glyph.lua` names the font on every icon node (ADR-0144). `shell.lua` declares both
-- `Propo` and `Mono`; the latter keeps indicators to one cell, so the node names it explicitly.
theme.icon_font                 = "JetBrainsMono Nerd Font Mono"

theme.font                      = {
    xs   = s(10, 8),
    sm   = s(12, 10),
    md   = s(14, 12),
    lg   = s(16, 14),
    xl   = s(20, 16),
    xxl  = s(28, 20),
    hero = s(48, 32),
}

theme.radius                    = {
    sm = s(6, 4),
    md = s(12, 8),
    lg = s(18, 12),
    xl = s(40, 20),
}

theme.icon                      = {
    xs = s(12, 10),
    sm = s(14, 12),
    md = s(18, 14),
    lg = s(24, 18),
    xl = s(32, 24),
}

-- Control heights keep adjacent toggles and buttons aligned without pixel literals.
--
-- `Theme.qml`'s `_controlHeights`, step for step. `IconButton`'s `size: "sm"` is 28px here, not
-- the old 24px, because the call sites already named the mirror's intended step.
theme.control                   = {
    xs = s(24, 20),
    sm = s(28, 24),
    md = s(34, 28),
    lg = s(42, 34),
    xl = s(52, 42),
}

theme.border_width              = 1
-- `borderWidthMedium`: twice the hairline, for cards floating over wallpaper.
theme.border_width_medium       = 2

-- ## Surface geometry
--
-- Shared surface sizes replace values duplicated in each module and opener. `basePanelHeight` makes
-- the bar 38px at 1080p instead of 34; its 31px `item` then has a little margin, whereas 34px made
-- controls touch both edges or shrink.
theme.bar_height                = s(42, 28)

-- ## The item scale
--
-- `control` sizes panel contents; `item` sizes bar controls such as icon buttons, battery pills and
-- the clock. They diverged when the bar got its own height. `item_radius` is half of `item_height`
-- by construction and has its own `s()` call: the mirror rounds it independently, and 18 vs 15.5
-- separates a circle from a round square.
-- `ActiveWindow.qml`'s `maxLength`, and `Theme.qml`'s `isUltrawide: (width / height) > 2.1`. A
-- character budget rather than a box: "WWWW" and "iiii" are both four characters and twice the
-- width apart, and the centre zone must stay content-sized so its midpoint is the bar's.
theme.title_limit               = (MAIN_WIDTH / math.max(1, MAIN_HEIGHT)) > 2.1 and 74 or 47

-- `CenterSide.qml` gives the zone `parent.width / 3` while media is up, so the spectrum has a span
-- to fill. Static: `s()` does not follow hotplug either, and the note at the top of this file
-- covers both.
theme.center_zone_width         = math.floor(MAIN_WIDTH / 3)

theme.item_height               = s(34, 20)
theme.item_width                = s(34, 20)
theme.item_radius               = s(18, 6)
-- The mirror's `batteryPillWidth`, enough for a glyph and "100%"; the bar's non-circular item.
theme.battery_pill_width        = s(80, 60)
-- Width when the pointer hovers the volume control; it leaves room for the percentage. Collapsed
-- width is `item_width`.
theme.volume_expanded_width     = s(120, 90)
-- `Theme.qml`'s `animationDuration`, in ms, for a node's `animate` table (ADR-0145). The engine's
-- default is `InOutQuad`; `animation_fast_ms` mirrors `animationFast` for hover zooms.
theme.animation_ms              = 147
theme.animation_fast_ms         = 100
-- `animationSlow`, the pace of a pulse rather than a transition: slow enough to read as breathing.
theme.animation_slow_ms         = 250
-- `NotificationService.qml`'s own `animationDuration`, not `Theme.animationDuration`:
-- `Math.round(Theme.animationDuration * 1.4)`. Notification travel uses it; colour transitions use
-- `animation_ms`. Derive it rather than writing 206 so it follows the base.
theme.notification_slide_ms     = math.floor(theme.animation_ms * 1.4 + 0.5)
-- For a fill the user is scrubbing: a volume key on repeat, a brightness button held down. An
-- eased tween restarts from a standstill when its target moves (ADR-0145), so key repeat keeps the
-- fill behind the number. A spring carries velocity across the retarget (ADR-0154).
--
-- The 400/42 spring is critically damped: `damping` is just above the
-- `2 * math.sqrt(stiffness)` threshold. A single press still lands in about a tenth of a second.
theme.spring_tracking           = { spring = { stiffness = 400, damping = 42 } }
-- One width replaces `Theme.qml`'s `networkPanelWidth: 340` and `bluetoothPanelWidth: 360`. Bar
-- panels share one card in `modules/shell/panel_host.lua`; ADR-0110 makes it as tall as the panel.
-- Each list is capped at `Math.min(contentHeight, Theme.itemHeight * 7)`, then scrolls.
theme.panel_width               = s(340, 280)
theme.panel_list_height         = s(280, 210)
-- Where a closed panel card sits before its first layout has measured it (`geometry`, ADR-0147):
-- above the bar by the tallest card. After that it drops from its own height, `PanelHost.qml`'s
-- `-height`.
theme.panel_slide               = s(760, 570)
-- Notification history holds the popup's cards. Its mirror width is `notificationPanelWidth: 420`;
-- `maxAvailableHeight` lets the list use most of the screen before it scrolls.
theme.notification_panel_width  = s(420, 340)
theme.notification_list_height  = s(640, 480)
-- Update rows need a name and two versions: at 340px, `ca-certificates-mozilla` and
-- `3.128-1 -> 3.129-1` collide. At 460px the name still elided `gpu-screen-recorder-git`; use
-- 520px for fixed version columns.
theme.update_panel_width        = s(520, 400)

-- `Theme.audioPanelWidth`: two named sliders and a mixer.
theme.audio_panel_width         = s(380, 300)

-- `trayMenuWidth`. A tray menu is an application's own words, so it needs more room than the
-- shell's own panels: "Preferences and settings" is a normal entry and the 340px card elides it.
theme.tray_menu_width           = s(300, 240)
-- Idle modal: action rows plus AC and battery columns, each with a timeout and switch. The mirror's
-- `Theme.idleModalWidth` is 820px; earlier 640px and 700px versions were cramped.
theme.idle_modal_width          = s(820, 640)
-- `idleTimeoutControlWidth` plus its switch.
theme.idle_profile_column       = s(190, 150)
theme.idle_row_height           = s(60, 46)
-- Timeline track, wide as the card and tall enough for a glyph plus duration per stage, unlike the
-- 6px `components/meter.lua` percentage meter.
theme.idle_track_height         = s(36, 28)
theme.update_list_height        = s(360, 260)
-- `updateOldVersionColumnWidth`: fixed column so versions align down the table rather than ragged
-- content-sized cells. Wide enough for `6.1.0.r4.gc8f50c4-1`.
theme.update_version_width      = s(116, 88)
-- Keep the log shorter than the package list; its last dozen lines explain a failure.
theme.update_log_height         = s(200, 150)
-- `panelToggleCardHeight`: a radio tile tall enough for a glyph over a word.
theme.panel_toggle_height       = s(56, 44)
-- `PanelEmptyState`'s `Layout.minimumHeight`: an empty list holds a glyph and line, reading as a
-- state rather than a gap.
theme.panel_empty_height        = s(120, 90)
theme.panel_gap                 = 4
theme.notification_width        = s(380, 300)
-- `notificationAppIconSize`: an `item_height` icon square with a few pixels of plate around it.
theme.notification_app_icon     = s(40, 32)
-- Popup stack surface height, not card height: up to four cards grow when groups/bodies expand, and
-- the fixed layer surface clips overflow. It is generous rather than measured; the inner column
-- sizes to content and only needs to fit inside.
theme.notification_stack_height = s(560, 420)
-- `OSDCard.qml`'s `osdSliderWidth`, `osdCardHeight`, `osdToggleIconContainerSize` and
-- `osdSliderTrackHeight`.
-- `osdSliderWidth`. The track layout is a fixed width: its content is a glyph, a bar and a
-- percentage, none of which changes length, so a card that resized under a volume key would be
-- the only thing on screen moving while the user holds it still.
theme.osd_width                 = s(300, 240)
-- `osdToggleMinWidth`. The toggle layout has no bar to fill the middle, and its text is whatever
-- the system had to say -- a sink name, a keyboard layout. So that card is sized to its content
-- and this is only its floor, keeping "num lock on" from drawing a card as narrow as the words.
theme.osd_toggle_min            = s(220, 176)
theme.osd_height                = s(80, 60)
theme.osd_tile                  = s(48, 36)
theme.osd_track                 = s(12, 8)
-- `dialogWidth`: the polkit card (`modules/global/polkit.lua`), narrower than the launcher because
-- it holds one sentence, one field and two buttons.
theme.dialog_width              = s(450, 360)

-- ## The lock card (`modules/global/lock.lua`)
--
-- `LockContent.qml`'s own formula, kept as a formula: 38% of the output, clamped to 480..720. This
-- is the one token measured from screen *width*, because the card is landscape -- 715px on this
-- 1920px seat, against the 448px an `s(480)` height-scaled token produced. That card was taller
-- than it was wide and nothing inside it read like the mirror.
--
-- ponytail: sampled once at evaluation, like `SCALE`. Moving the shell to a narrower output leaves
-- the old width until the next edit.
theme.lock_card_width           = math.max(480, math.min(math.floor(MAIN_WIDTH * 0.38), 720))
-- `controlHeightLg * 2.4`, the initials disc, measured off the mirror at 106px on a 1200px-tall
-- screen.
theme.lock_avatar               = s(112, 72)

-- `mediaPanelWidth`/`mediaArtworkSize`. Wider than the shared 340px card because the artwork sits
-- beside the title, transport row and seek bar rather than above them.
theme.media_panel_width         = s(460, 380)
theme.media_artwork             = s(96, 80)

-- ## The launcher (`modules/global/launcher.lua`)
--
-- `launcherWindowWidth/Height`, `launcherRowHeight`, `launcherSpecialRowHeight` and
-- `launcherIconSize` as the mirror sets them. The earlier 720x560 with 56px rows claimed rows here
-- carry less than the mirror's; they carry the same name and one-line comment, so the smaller card
-- only cost the list a row.
theme.launcher_width            = s(860, 645)
theme.launcher_height           = s(680, 510)
theme.launcher_row_height       = s(64, 48)
-- Taller than an app row because the provider row carries a badge and an "Enter to copy" hint
-- beside the two text lines.
theme.launcher_special_height   = s(86, 65)
theme.launcher_icon             = s(42, 32)

-- ## The wallpaper picker (`modules/global/wallpaper_picker.lua`)
--
-- The mirror's `wallpaperModalWidth/Height` and `wallpaperSidebarWidth` define the reference card.
-- This fixed-width card uses four columns, unlike the mirror's "as many 230px tiles as fit"; the
-- remaining card width determines each tile.
theme.wallpaper_picker_width    = s(1180, 900)
theme.wallpaper_picker_height   = s(880, 660)
theme.wallpaper_sidebar_width   = s(250, 200)
theme.wallpaper_columns         = 4

return theme
