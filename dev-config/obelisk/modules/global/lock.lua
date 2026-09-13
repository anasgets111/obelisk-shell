-- Mirrors Global/LockScreen.qml and Global/LockContent.qml: the wallpaper under a scrim, one glass
-- card centred on every output, and a password pill that says what PAM is doing.
--
-- The wallpaper's `MultiEffect` blur is absent because the engine has no effect node; the scrim
-- separates it instead.
local theme         = require("config.theme")
local icons         = require("config.icons")
local util          = require("lib.util")
local wallpaper     = require("lib.wallpaper")
local cell          = require("components.cell")
local glyph         = require("components.glyph")
local panel_card    = require("components.panel_card")
local identity      = require("lib.identity")
local weather       = require("lib.weather")

local PAD           = theme.spacing.xl
-- What the card's children have to share, for the nodes that need a number rather than "Fill".
local CONTENT       = theme.lock_card_width - PAD * 2
local FIELD_HEIGHT  = theme.control.xl
-- `Layout.maximumWidth: shell.implicitWidth * 0.82`. The pill stops short of the card's own edges
-- on both sides; using `CONTENT` makes it read as a search bar rather than a card control.
local FIELD_WIDTH   = math.floor(theme.lock_card_width * 0.82)
-- Half the height, the way `theme.item_radius` is half `item_height`. `radius.xl` is the fully
-- round token, sized for the card's corner and only close to this box by coincidence.
local FIELD_RADIUS  = math.floor(FIELD_HEIGHT / 2)
local BADGE_HEIGHT  = theme.control.xs
-- The mirror's multipliers, checked against a recording: a 68px clock, 36px initials, and a 23px
-- name on a 1200px-tall screen. Shared steps are sized for the bar, not this card.
local CLOCK_SIZE    = theme.s(72, 44)
local INITIALS_SIZE = theme.s(36, 26)
local NAME_SIZE     = theme.s(24, 18)
-- Heavier than `theme.SCRIM` at 0.45. This wallpaper is scenery, so the card needs contrast against
-- a photograph with no blur to soften it.
local SCRIM         = theme.with_opacity(theme.BG, 0.6)

-- How long this screen takes to leave, and what the engine is told to wait.
--
-- The engine removes the lock after authentication, not when the tween ends (ADR-0190), so this
-- value has to cover the exit rather than describe it, and is read off the card's animation below
-- instead of written twice. The card is the only thing that moves on the way out; the ground holds
-- until the compositor takes the surface away. See the ground's own note for why it cannot fade.
local EXIT_MS       = theme.animation_slow_ms
-- Slack for the Supervisor-to-Renderer state push and first frame before motion begins. Without it,
-- the exit loses that round trip and looks like a rendering bug.
local LEAVE_SLACK   = 60
local LEAVE_MS      = EXIT_MS + LEAVE_SLACK
obelisk.lock:invoke("set_unlock_animation", LEAVE_MS)

-- True exactly while the card should be up: the compositor has granted the lock and PAM has not yet
-- answered. Both edges of the card's motion are this one flag changing value.
--
-- `animate.from` only applies when a node has no displayed value. This subtree outlives the lock,
-- so entry needs the `active` value change as well as exit. Keying on `active` avoids finishing the
-- fade before `ext_session_lock_v1` has presented every output and set `locked`.
local up           = obelisk.lock:map(function(l)
    return l ~= nil and l.active and not l.unlocking
end)
-- `Theme.lockClosedScale`, where the card starts its entry.
local CLOSED_SCALE = 0.94

local full_name    = identity.full_name
local account      = identity.account
local initials     = identity.initials

-- Read from `obelisk.lock`, not `rescue`, per ADR-0052 decision 4: while lock surfaces are mapped
-- the bar's `rescue_cell` is unreachable. Print `attempts` because capability state is sampled at
-- layout time (ADR-0044); identical `error` strings would otherwise hide the second failure.
local hint         = util.label(obelisk.lock, function(l)
    if l.error ~= nil and l.error ~= "" then
        return string.format("%s (%d)", l.error, l.attempts or 0)
    end
    if l.authenticating then
        return "Authenticating..."
    end
    if not l.active then
        return "Locking..."
    end
    return "Press Enter to unlock"
end)

local failed       = util.shown_when(obelisk.lock, function(l)
    return l.error ~= nil and l.error ~= ""
end)

-- Match `passwordInput`'s border and hint colours, so failure is one state change.
-- `animate` carries the pill's `Theme.ColorTransition`.
local field_border = obelisk.lock:map(function(l)
    if l == nil then
        return theme.GLASS_BORDER
    end
    if l.error ~= nil and l.error ~= "" then
        return theme.RED
    end
    return l.authenticating and theme.ACCENT or theme.GLASS_BORDER
end)

local caps         = obelisk.keyboard:map(function(k)
    return k ~= nil and k.caps_lock == true
end)

-- Black on yellow, chosen by the same helper the bar's buttons use.
local BADGE_FG     = theme.text_contrast(theme.YELLOW)

-- One icon-and-reading pair from the divider row. Three literal children need no `list`.
local function status_item(icon_glyph, label, visible)
    return row {
        spacing = theme.spacing.xs,
        visible = visible,
        children = {
            glyph(icon_glyph, theme.with_opacity(theme.ACCENT, 0.6), theme.icon.sm, { align_v = "Center" }),
            cell(label, theme.DIM, theme.font.sm, { align_v = "Center" }),
        },
    }
end

-- Built per output because the compositor calls `child` for each lock surface. Each surface
-- owns its wallpaper and field, matching `LockScreen.qml`'s per-`screen`
-- `WallpaperService.wallpaperPath`. Unlike `LockContent.qml`, the field is on every output, so the
-- compositor can focus the sole `secure_submit` field on whichever screen has keyboard focus.
local function content(output)
    -- Authentication routes through a `textfield` with `secure_submit`. With `mask_character`
    -- too, keystrokes stay in a native buffer on the Renderer's Wayland thread and leave as a
    -- `("lock", "authenticate")` envelope, never a Lua value (ADR-0005/ADR-0027). No
    -- `on_change`/`on_submit`: either callback would reopen the closed path.
    --
    -- It is the surface's only `secure_submit` field, so compositor keyboard focus needs no click.
    --
    -- Two fields would make the engine refuse to guess. `mask_character` draws one glyph per key;
    -- the old field reserved 28px and showed nothing, making pam's two-second delay look like a
    -- slow unlock. `pam_faillock`'s third failure then cost ten minutes. The one-byte mask uses
    -- `*`, not the mirror's `●`.
    local password_field = textfield {
        width = "Fill",
        height = FIELD_HEIGHT,
        placeholder = "Password",
        mask_character = "*",
        secure_submit = { capability = "lock", action = "authenticate" },
        font_size = theme.font.lg,
        foreground = theme.with_opacity(theme.FG, 0.7),
        align_v = "Center",
    }

    local card = panel_card({
        column {
            width = "Fill",
            spacing = theme.spacing.xs,
            children = {
                -- The mirror's seat setting is 12-hour: build from `os.date("*t")` rather than `%I`
                -- (which pads to "01:40") or `%p` (which follows the locale).
                cell(util.label(obelisk.system, function(s)
                    local t = os.date("*t", s.time)
                    local hour = t.hour % 12
                    return {
                        {
                            text = string.format("%d:%02d %s", hour == 0 and 12 or hour, t.min,
                                t.hour < 12 and "AM" or "PM"),
                            bold = true,
                        },
                    }
                end), theme.FG, CLOCK_SIZE, { width = "Fill", align = "Center" }),
                -- Build the day in two calls. `%-d` is glibc-specific, and Lua rejected it before
                -- strftime saw it, returning `util.label`'s "!".
                cell(util.label(obelisk.system, function(s)
                    return string.format("%s %d", os.date("%A, %B", s.time), os.date("*t", s.time).day)
                end), theme.DIM, theme.font.lg, { width = "Fill", align = "Center" }),
            },
        },
        column {
            width = "Fill",
            spacing = theme.spacing.md,
            children = {
                -- One disc. The mirror's ring is a 50% accent glow under `MultiEffect`; without
                -- blur, two hard circles read as a button with a focus outline, so the ring is a
                -- border.
                rect {
                    width = theme.lock_avatar,
                    height = theme.lock_avatar,
                    radius = math.floor(theme.lock_avatar / 2),
                    align_h = "Center",
                    background = theme.ACCENT_LIGHT,
                    border_width = theme.border_width,
                    border_color = theme.with_opacity(theme.ACCENT, 0.45),
                    children = {
                        cell(initials, theme.FG, INITIALS_SIZE, {
                            width = "Fill",
                            align = "Center",
                            align_v = "Center",
                        }),
                    },
                },
                -- Its own column: the mirror sets the name a `spacingMd` under the disc and the
                -- account tighter still under the name, which one shared spacing cannot do.
                column {
                    width = "Fill",
                    spacing = theme.spacing.sm,
                    children = {
                        cell(full_name, theme.FG, NAME_SIZE, { width = "Fill", align = "Center" }),
                        cell(account, theme.with_opacity(theme.FG, 0.55), theme.font.sm, {
                            width = "Fill",
                            align = "Center",
                        }),
                    },
                },
            },
        },
        column {
            width = "Fill",
            spacing = theme.spacing.md,
            children = {
                row {
                    width = FIELD_WIDTH,
                    height = FIELD_HEIGHT,
                    align_h = "Center",
                    padding = { right = theme.spacing.md, left = theme.spacing.md },
                    spacing = theme.spacing.sm,
                    -- `bgInput` at 0.85 is a well, not a raised control. `GLASS_CONTROL` sits above
                    -- this card's ground and made the field the lightest thing on the card.
                    background = theme.GLASS,
                    radius = FIELD_RADIUS,
                    border_width = theme.border_width_medium,
                    border_color = field_border,
                    animate = { border_color = theme.animation_ms },
                    children = {
                        glyph(icons.lock, theme.with_opacity(theme.ACCENT, 0.8), theme.icon.md, {
                            align_v = "Center",
                        }),
                        password_field,
                        -- Put the warning inside the pill; the bar's indicator is across the
                        -- screen.
                        row {
                            height = BADGE_HEIGHT,
                            align_v = "Center",
                            padding = { right = theme.spacing.sm, left = theme.spacing.sm },
                            spacing = theme.spacing.xs,
                            background = theme.YELLOW,
                            radius = math.floor(BADGE_HEIGHT / 2),
                            visible = caps,
                            children = {
                                glyph(icons.caps_lock, BADGE_FG, theme.icon.xs, { align_v = "Center" }),
                                cell("caps lock", BADGE_FG, theme.font.xs, { align_v = "Center" }),
                            },
                        },
                    },
                },
                -- Wrap: the unavailable-authentication line once clipped to
                -- "could not start authentication: pam worker f".
                cell(hint, failed:map(function(f)
                    return f and theme.RED or theme.with_opacity(theme.FG, 0.5)
                end), theme.font.sm, { width = "Fill", align = "Center", wrap = "Word", max_lines = 4 }),
            },
        },
        column {
            width = "Fill",
            spacing = theme.spacing.md,
            children = {
                -- Not `theme.BORDER`: surface2 at 0.75 disappears between these greys.
                rect {
                    width = math.floor(CONTENT * 0.6),
                    height = theme.border_width,
                    align_h = "Center",
                    background = theme.with_opacity(theme.FG, 0.15),
                },
                row {
                    width = "Fill",
                    align_h = "Center",
                    spacing = theme.spacing.lg,
                    children = {
                        status_item(
                            weather.code:map(weather.glyph),
                            -- `weatherLabel` is `currentTemp.split(" ")[0]`: the degrees without
                            -- the emoji beside them. Read against the code, not against a zero
                            -- temperature, which is a real winter reading in most of the world.
                            computed({ weather.code, weather.temperature }, function(code, celsius)
                                return (code or -1) >= 0 and string.format("%d°C", celsius or 0) or "--"
                            end)
                        ),
                        status_item(
                            obelisk.battery:map(util.battery_glyph),
                            util.label(obelisk.battery, function(b)
                                return string.format("%d%%", b.percent)
                            end),
                            util.shown_when(obelisk.battery, function(b)
                                return b.present
                            end)
                        ),
                        status_item(
                            obelisk.network:map(util.network_glyph),
                            util.label(obelisk.network, function(n)
                                return n.ssid or "offline"
                            end)
                        ),
                        status_item(icons.keyboard, util.label(obelisk.keyboard, function(k)
                            return k.active_layout ~= "" and k.active_layout or "N/A"
                        end)),
                    },
                },
            },
        },
    }, {
        width = theme.lock_card_width,
        align_h = "Center",
        align_v = "Center",
        padding = { top = PAD, right = PAD, bottom = PAD, left = PAD },
        -- One gap between the four groups; tighter internal spacing pairs date/clock and
        -- hint/field.
        -- The mirror spends nine spacer `Item`s on the same rhythm.
        spacing = theme.spacing.xl,
        -- Lighter than its surround. The mirror's `bgColor` at 0.30 works over blurred wallpaper;
        -- on a sharp image it lets the scene run through the name. `ELEVATED` keeps the card lit
        -- against the dimmed screen while text clears the photograph.
        background = theme.with_opacity(theme.ELEVATED, 0.62),
        radius = theme.radius.xl,
        -- With no shadow node, the edge is the only thing separating the card from the picture.
        border_width = theme.border_width_medium,
        border_color = theme.GLASS_BORDER,
    })

    return rect {
        width = "Fill",
        height = "Fill",
        -- Opaque for the whole lock, including exit. QML fades this ground after the card because
        -- its session remains behind the window; `ext_session_lock_v1` hides every client and niri
        -- paints solid red before unlock. Fading would reveal that colour during `LEAVE_SLACK`.
        --
        -- `modules/global/wallpaper.lua` also leaves a failed decode dark rather than transparent,
        -- so the lock never becomes a hole through to the session.
        background = theme.BG,
        children = {
            image {
                source = wallpaper.path_of(output),
                fit = wallpaper.fit_of(output),
                width = "Fill",
                height = "Fill",
            },
            rect { width = "Fill", height = "Fill", background = SCRIM },
            -- `LockScreen.qml`'s `stage`: the wallpaper is present on the first frame and the card
            -- fades and grows into it (ADR-0146, ADR-0149). The screen-sized wrapper keeps the
            -- scale pivot centred; `OutBack` matches the mirror's overshoot.
            column {
                width = "Fill",
                height = "Fill",
                align_h = "Center",
                align_v = "Center",
                -- Both properties need targets; an absent property skips the entry
                -- (`Animatable::from_value` answers "nothing to animate"). On exit, the card
                -- shrinks to `CLOSED_SCALE` and fades over a stationary wallpaper.
                opacity = up:map(function(on)
                    return on and 1 or 0
                end),
                scale = up:map(function(on)
                    return on and 1 or CLOSED_SCALE
                end),
                animate = {
                    opacity = { duration = theme.animation_slow_ms, easing = "OutCubic", from = 0 },
                    scale = { duration = theme.animation_slow_ms, easing = "OutBack", from = CLOSED_SCALE },
                },
                children = { card },
            },
        },
    }
end

-- Declared, not open. `lock` refuses `visible`, `monitor`, `anchor`, `width`, and `height`, but
-- otherwise takes the common and box properties like any other surface. The compositor creates one
-- per output while locked. No Wayland object exists until `obelisk.lock:invoke("lock")` (ADR-0049).
return lock {
    id = "lock_screen",
    child = content,
}
