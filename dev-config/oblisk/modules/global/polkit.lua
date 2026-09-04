-- Mirrors Modules/Global/PolkitDialog.qml: the prompt polkitd raises when something wants to be
-- authorised (ADR-0114).
--
-- Reading `oblisk.polkit` is what registers this shell as the session's authentication agent
-- (ADR-0070), so a config without this file leaves polkit to whatever agent was there before.
--
-- Two of the mirror's controls are not here. Its Authenticate button cannot exist: the password
-- lives in a native buffer that only Enter sends (ADR-0005), and a click has no way to reach it,
-- so the placeholder says what does. Its Escape-to-cancel is not here either: on a masked field
-- Escape clears and stays (ADR-0092), so the cancel is the button.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local panel_card = require("components.panel_card")
local action_button = require("components.action_button")

local active = util.shown_when(oblisk.polkit, function(p)
    return p.active
end)

local function cancel()
    oblisk.polkit:invoke("cancel")
end

local header = row {
    width = "Fill",
    spacing = theme.spacing.lg,
    children = {
        icon {
            name = oblisk.polkit:map(function(p)
                return (p and p.icon_name ~= "") and p.icon_name or "dialog-password"
            end),
            size = theme.icon.xl,
            align_v = "Center",
        },
        column {
            width = "Fill",
            spacing = theme.spacing.xs,
            children = {
                cell(util.label(oblisk.polkit, function(p)
                    return p.message
                end), theme.FG, theme.font.md, { width = "Fill", wrap = "Word" }),
                cell(util.label(oblisk.polkit, function(p)
                    return p.action_id
                end), theme.DIM, theme.font.xs, { width = "Fill" }),
            },
        },
    },
}

-- The field is the only `secure_submit` on this surface, so the engine arms it the moment the
-- surface takes the keyboard, and it is typable with no click (see `modules/global/lock.lua`).
local password = column {
    width = "Fill",
    padding = { top = 0, right = theme.spacing.sm, bottom = 0, left = theme.spacing.sm },
    background = theme.GLASS_CONTROL,
    radius = theme.radius.md,
    border_width = theme.border_width,
    border_color = theme.GLASS_BORDER,
    children = {
        textfield {
            width = "Fill",
            height = theme.control.md,
            placeholder = "password, then Enter",
            mask_character = "*",
            secure_submit = { capability = "polkit", action = "authenticate" },
            font_size = theme.font.sm,
        },
    },
}

-- "Authentication Failed" in the mirror; ours carries the reason PAM gave, in the lock screen's
-- words. Nothing animates, so "checking" is the line a spinner would be.
local status = cell(util.label(oblisk.polkit, function(p)
    if p.authenticating then
        return "checking..."
    end
    return p.error
end), oblisk.polkit:map(function(p)
    return (p and p.authenticating) and theme.DIM or theme.RED
end), theme.font.sm, {
    width = "Fill",
    visible = util.shown_when(oblisk.polkit, function(p)
        return p.authenticating or p.error ~= ""
    end),
})

return panel {
    id = "polkit_dialog",
    namespace = "oblisk-polkit",
    layer = "Overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = active,
    keyboard_interactivity = active:map(function(open)
        return open and "Exclusive" or "None"
    end),
    child = rect {
        width = "Fill",
        height = "Fill",
        background = theme.SCRIM,
        children = {
            panel_card({ header, password, status, action_button("cancel", cancel, "polkit-cancel", { tone = "quiet" }) }, {
                width = theme.dialog_width,
                align_h = "Center",
                align_v = "Center",
                spacing = theme.spacing.md,
                padding = {
                    top = theme.spacing.lg,
                    right = theme.spacing.lg,
                    bottom = theme.spacing.lg,
                    left = theme.spacing.lg,
                },
                radius = theme.radius.lg,
                background = theme.GLASS,
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
