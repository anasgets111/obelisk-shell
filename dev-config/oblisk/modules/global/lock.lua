-- Mirrors Global/LockScreen.qml and Global/LockContent.qml: the wallpaper under a scrim, one glass
-- card centred on every output, and a password pill that says what PAM is doing.
--
-- Two things the mirror draws are missing here because nothing supplies them, and both are left out
-- rather than faked: the wallpaper's `MultiEffect` blur, since the engine has no effect node and the
-- scrim does the separating instead, and the weather status item, since there is no weather
-- capability. Everything else on the card is the mirror's, reading the same facts.
local theme         = require("config.theme")
local icons         = require("config.icons")
local util          = require("lib.util")
local wallpaper     = require("lib.wallpaper")
local cell          = require("components.cell")
local glyph         = require("components.glyph")
local panel_card    = require("components.panel_card")
local identity      = require("lib.identity")

local PAD           = theme.spacing.xl
-- What the card's children have to share, for the nodes that need a number rather than "Fill".
local CONTENT       = theme.lock_card_width - PAD * 2
local FIELD_HEIGHT  = theme.control.xl
-- `Layout.maximumWidth: shell.implicitWidth * 0.82`. The pill stops short of the card's own edges
-- on both sides; run to `CONTENT` and it reads as a search bar in a window rather than a control on
-- a card.
local FIELD_WIDTH   = math.floor(theme.lock_card_width * 0.82)
-- Half the height, the way `theme.item_radius` is half `item_height`. `radius.xl` is the fully
-- round token, but it is sized for the card's corner and only happens to be close on this box.
local FIELD_RADIUS  = math.floor(FIELD_HEIGHT / 2)
local BADGE_HEIGHT  = theme.control.xs
-- The mirror's own multipliers on `fontHero` and `fontXl`, checked against a recording of it: a
-- 68px clock, 36px initials and a 23px name on a 1200px-tall screen. The shared steps are sized for
-- the bar, where `hero` is a tooltip heading; this card's first line is read from across a room.
local CLOCK_SIZE    = theme.s(72, 44)
local INITIALS_SIZE = theme.s(36, 26)
local NAME_SIZE     = theme.s(24, 18)
-- Heavier than `theme.SCRIM`. That one is 0.45 because it lays a *panel's* modal over the wallpaper
-- and the wallpaper is still the desktop underneath. Here the wallpaper is scenery: the card has to
-- read against a photograph with no blur to soften it.
local SCRIM         = theme.with_opacity(theme.BG, 0.6)

-- How long this screen takes to leave, and what the engine is told to wait.
--
-- The lock comes down when the *engine* says so, not when the tween ends: the user has already
-- authenticated, and a config is not allowed to keep them looking at a lock screen (ADR-0190).
-- So this number has to cover the exit rather than describe it, and it is derived from the two
-- animations below instead of written twice.
--
-- The card fades over `animation_slow_ms`; the ground waits `animation_ms` and then fades over
-- `animation_ms`, so the pair ends at whichever of those runs longer.
local EXIT_MS       = math.max(theme.animation_slow_ms, theme.animation_ms * 2)
-- Slack, because the window opens when the Supervisor schedules the release and not when this
-- config hears about it: a state push has to reach the Renderer and a first frame be scheduled
-- before anything moves. Without it the exit is cut off by exactly that round trip, which is the
-- kind of shortfall that looks like a rendering bug rather than a timing one.
local LEAVE_SLACK   = 60
local LEAVE_MS      = EXIT_MS + LEAVE_SLACK
oblisk.lock:invoke("set_unlock_animation", LEAVE_MS)

-- True from the moment PAM says yes until the lock is off the glass.
local leaving      = oblisk.lock:map(function(l)
    return l ~= nil and l.unlocking
end)

-- True exactly while the card should be up: the compositor has granted the lock and PAM has not yet
-- answered. Both edges of the card's motion are this one flag changing value.
--
-- `animate`'s `from` is not enough, and that is the difference between the two directions. `from`
-- applies only where a node has no previously displayed value; this subtree outlives the lock, so
-- it already displayed `opacity = 1` and the entry had nothing to move. Leaving worked for exactly
-- the reason arriving did not -- it is a value *change* on a node that is already there.
--
-- Keyed on `active` rather than on the surface existing, because `ext_session_lock_v1` withholds
-- `locked` until every output has presented a frame. A fade run during that handshake would be over
-- before the screen it introduces was ever shown.
local up           = oblisk.lock:map(function(l)
    return l ~= nil and l.active and not l.unlocking
end)
-- `Theme.lockClosedScale`, where the card starts its entry.
local CLOSED_SCALE = 0.94

local full_name    = identity.full_name
local account      = identity.account
local initials     = identity.initials

-- Read from `oblisk.lock`, not `rescue`, per ADR-0052 decision 4: while lock surfaces are mapped
-- the bar's `rescue_cell` is unreachable.
--
-- The mirror's three-way `authHint` plus a fourth state it does not have: a mapped surface before
-- the Renderer has confirmed the lock. Print `attempts` because capability state is sampled at
-- layout time (ADR-0044); identical consecutive `error` strings would otherwise hide the second
-- failure.
local hint         = util.label(oblisk.lock, function(l)
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

local failed       = util.shown_when(oblisk.lock, function(l)
    return l.error ~= nil and l.error ~= ""
end)

-- `passwordInput`'s three border colours, and the same colour on the hint below it, so a failure is
-- one change of state rather than two unrelated reds. `Theme.ColorTransition` is `animate` on the
-- pill.
local field_border = oblisk.lock:map(function(l)
    if l == nil then
        return theme.GLASS_BORDER
    end
    if l.error ~= nil and l.error ~= "" then
        return theme.RED
    end
    return l.authenticating and theme.ACCENT or theme.GLASS_BORDER
end)

local caps         = oblisk.keyboard:map(function(k)
    return k ~= nil and k.caps_lock == true
end)

-- Black on yellow, chosen by the same helper the bar's buttons use rather than hard-coding the
-- ground's opposite here.
local BADGE_FG     = theme.text_contrast(theme.YELLOW)

-- One icon-and-reading pair from the row under the divider. The mirror's `statusItems` is a list
-- through a `Repeater`; three literal children need no `list`.
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

-- Built per output rather than once. The compositor makes a lock surface for every screen and calls
-- `child` for each (§ 6), so each gets its own nodes; one shared table would put one `textfield` on
-- two surfaces. That also gives every screen its own wallpaper, matching `LockScreen.qml`'s
-- per-`screen` `WallpaperService.wallpaperPath`.
--
-- Unlike the mirror, the field is on all of them. `LockContent.qml` draws it only on the main
-- monitor because its `LockService` holds one shared buffer; here each surface owns its field, and
-- the engine focuses the sole `secure_submit` field on the surface the compositor gave keyboard
-- focus to. Typing works on whichever screen you are looking at.
local function content(output)
    -- § 6 routes authentication through a `textfield` with `secure_submit`. With `mask_character`
    -- too, keystrokes stay in a native buffer on the Renderer's Wayland thread and leave as a
    -- `("lock", "authenticate")` envelope, never a Lua value (§ 5.2 item 8, ADR-0005/ADR-0027). No
    -- `on_change`/`on_submit`: either callback would reopen the closed path.
    --
    -- It is the surface's only `secure_submit` field. The engine focuses the sole field on
    -- compositor keyboard focus, so no click is needed; with two fields it refuses to guess and
    -- needs a mouse.
    --
    -- `mask_character` draws one glyph per keystroke. The old field reserved 28px and showed
    -- nothing; blind typing made `pam_unix`'s two-second wrong-password delay indistinguishable
    -- from a slow unlock, and `pam_faillock` locked the account after three, costing ten minutes.
    -- The mirror masks with `●`; `mask_character` is capped at one byte, so this is `*`.
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
                -- `TimeService.use24Hour ? "HH:mm" : "h:mm AP"`, on the seat's own setting, which
                -- is 12-hour. Built from `os.date("*t")` rather than `%I`, which pads to "01:40",
                -- and `%p`, which follows the locale rather than the mirror.
                cell(util.label(oblisk.system, function(s)
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
                -- The day in two calls rather than one `%-d`. That no-padding flag is glibc's,
                -- and Lua checks the format string itself before strftime ever sees it, so the
                -- whole line came back as `util.label`'s "!".
                cell(util.label(oblisk.system, function(s)
                    return string.format("%s %d", os.date("%A, %B", s.time), os.date("*t", s.time).day)
                end), theme.DIM, theme.font.lg, { width = "Fill", align = "Center" }),
            },
        },
        column {
            width = "Fill",
            spacing = theme.spacing.md,
            children = {
                -- One disc, where the mirror has a disc inside a ring. That ring is a glow: 50%
                -- accent under a `MultiEffect` shadow, and the blur behind it turns the pair into
                -- one soft circle. Drawn as two hard circles with no blur it reads as a button with
                -- a focus outline, so the ring is a border on the disc itself.
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
                    -- `bgInput` at 0.85: a well, not a raised control. `GLASS_CONTROL` is the bar's
                    -- ground and sits *above* this card's, which made the field the lightest thing
                    -- on the card instead of the hole you type into.
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
                        -- Inside the pill, where the eye already is when Caps Lock is the reason
                        -- the password keeps failing. The bar's indicator says the same thing
                        -- across the screen, which is no use to someone typing.
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
                -- Wrap: an unavailable-authentication line does not fit the card on one line.
                -- Clipped, it read "could not start authentication: pam worker f" and the user had
                -- to guess the rest.
                cell(hint, failed:map(function(f)
                    return f and theme.RED or theme.with_opacity(theme.FG, 0.5)
                end), theme.font.sm, { width = "Fill", align = "Center", wrap = "Word", max_lines = 4 }),
            },
        },
        column {
            width = "Fill",
            spacing = theme.spacing.md,
            children = {
                -- Not `theme.BORDER`. That is surface2 at 0.75, which separates two opaque
                -- greys; one pixel of it on the card's own ground is a line you cannot see.
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
                            oblisk.battery:map(util.battery_glyph),
                            util.label(oblisk.battery, function(b)
                                return string.format("%d%%", b.percent)
                            end),
                            util.shown_when(oblisk.battery, function(b)
                                return b.present
                            end)
                        ),
                        status_item(
                            oblisk.network:map(util.network_glyph),
                            util.label(oblisk.network, function(n)
                                return n.ssid or "offline"
                            end)
                        ),
                        status_item(icons.keyboard, util.label(oblisk.keyboard, function(k)
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
        -- One gap between the four groups; the tighter spacing inside each is what pairs the date
        -- with the clock and the hint with the field. The mirror spends nine spacer `Item`s on the
        -- same rhythm.
        spacing = theme.spacing.xl,
        -- Lighter than its surround, not darker. The mirror's card is `bgColor` at 0.30 over a
        -- blurred wallpaper, which reads as frosted glass lifted off the picture; the same alpha
        -- over a sharp one is a window with a hillside running through the name. `ELEVATED` is the
        -- lightened ground the panels already use, and at this alpha the card is the lit object on
        -- a dimmed screen while text still clears the photograph underneath.
        background = theme.with_opacity(theme.ELEVATED, 0.62),
        radius = theme.radius.xl,
        -- `borderWidthThin` on the mirror is on a card with a drop shadow under it. With no shadow
        -- node the edge is the only thing separating the card from the picture.
        border_width = theme.border_width_medium,
        border_color = theme.GLASS_BORDER,
    })

    return rect {
        width = "Fill",
        height = "Fill",
        -- The ground leaves a stage after the card, which is the mirror's `phase` 1 -> 0 following
        -- its 2 -> 1: the screen behind the card is the last thing to go, so the session does not
        -- appear through a card that is still on its way out.
        opacity = leaving:map(function(out)
            return out and 0 or 1
        end),
        animate = {
            opacity = { duration = theme.animation_ms, easing = "InCubic", delay = theme.animation_ms },
        },
        -- Under the image, as `modules/global/wallpaper.lua` is: a failed decode leaves the lock
        -- dark rather than transparent, which on a lock screen is the difference between a mistake
        -- and a hole through to the session.
        background = theme.BG,
        children = {
            image {
                source = wallpaper.path_of(output),
                fit = wallpaper.fit_of(output),
                width = "Fill",
                height = "Fill",
            },
            rect { width = "Fill", height = "Fill", background = SCRIM },
            -- `LockScreen.qml`'s `stage`, and the whole of this screen's motion: the wallpaper is
            -- there on the first frame and the card fades and grows into it (ADR-0146, ADR-0149).
            -- The wrapper is screen-sized so the scale pivots on the centre, where the card sits,
            -- as `components/modal.lua` does it. `OutBack` is the mirror's overshoot.
            column {
                width = "Fill",
                height = "Fill",
                align_h = "Center",
                align_v = "Center",
                -- Both spelled out, because a property the node never sets has no target for a
                -- tween to reach and the entry is skipped (`Animatable::from_value` on an absent
                -- property answers "nothing to animate").
                --
                -- On the way out both run backwards, which is `leaving` doing the same job the
                -- `from` does on the way in: the card shrinks back to `CLOSED_SCALE` and fades,
                -- and the ground below follows it a stage later.
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

-- Declared, not open. § 6 gives `lock` only `id` and `child`: the compositor creates one per output
-- while locked, with no `visible`, monitor, or size. This retains one node and zero Wayland objects
-- until `oblisk.lock:invoke("lock")`, the declaration/lifetime split of ADR-0049.
return lock {
    id = "lock_screen",
    child = content,
}
