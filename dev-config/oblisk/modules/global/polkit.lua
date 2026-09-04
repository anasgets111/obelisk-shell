-- Mirrors Modules/Global/PolkitDialog.qml: the prompt polkitd raises when something wants to be
-- authorised (ADR-0114). Reading `oblisk.polkit` is what registers this shell as the session's
-- authentication agent (ADR-0070).
--
-- Not mirrored: the Authenticate button, because a click cannot reach the password -- it lives in
-- a native buffer only Enter sends (ADR-0005); Escape-to-cancel, because a masked field's Escape
-- clears and stays (ADR-0092); and the `●` mask, because `mask_character` is one byte.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local panel_card = require("components.panel_card")
local action_button = require("components.action_button")

local function read(fn)
    return util.label(oblisk.polkit, fn)
end

local active = util.shown_when(oblisk.polkit, function(p)
    return p.active
end)

local pad = theme.spacing.lg

return panel {
    id = "polkit_dialog",
    namespace = "oblisk-polkit",
    layer = "Overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = active,
    -- Exclusive while open: the one `secure_submit` field on this surface is armed the moment the
    -- surface takes the keyboard, so it is typable with no click (see `modules/global/lock.lua`).
    keyboard_interactivity = active:map(function(open)
        return open and "Exclusive" or "None"
    end),
    child = rect {
        width = "Fill",
        height = "Fill",
        background = theme.SCRIM,
        children = {
            panel_card({
                row {
                    width = "Fill",
                    spacing = theme.spacing.lg,
                    children = {
                        icon {
                            name = read(function(p)
                                return p.icon_name ~= "" and p.icon_name or "dialog-password"
                            end),
                            size = theme.icon.xl,
                            align_v = "Center",
                        },
                        column {
                            width = "Fill",
                            spacing = theme.spacing.xs,
                            children = {
                                cell(read(function(p)
                                    return { { text = p.message, bold = true } }
                                end), theme.FG, theme.font.md, { width = "Fill", wrap = "Word" }),
                                cell(read(function(p)
                                    return p.action_id
                                end), theme.DIM, theme.font.xs, { width = "Fill" }),
                            },
                        },
                    },
                },
                cell("Password:", theme.FG, theme.font.sm),
                column {
                    width = "Fill",
                    padding = { top = 0, right = theme.spacing.sm, bottom = 0, left = theme.spacing.sm },
                    background = theme.GLASS_CONTROL,
                    radius = theme.radius.md,
                    border_width = theme.border_width,
                    border_color = theme.ACCENT,
                    children = {
                        textfield {
                            width = "Fill",
                            height = theme.control.md,
                            placeholder = "then Enter",
                            mask_character = "*",
                            secure_submit = { capability = "polkit", action = "authenticate" },
                            font_size = theme.font.sm,
                        },
                    },
                },
                -- "Authentication Failed" in the mirror; ours carries PAM's reason in the lock
                -- screen's words. Nothing animates, so "checking" is the line a spinner would be.
                cell(read(function(p)
                    return p.authenticating and "checking..." or p.error
                end), read(function(p)
                    return p.authenticating and theme.DIM or theme.RED
                end), theme.font.sm, {
                    width = "Fill",
                    visible = util.shown_when(oblisk.polkit, function(p)
                        return p.authenticating or p.error ~= ""
                    end),
                }),
                row {
                    width = "Fill",
                    align_h = "End",
                    children = {
                        action_button("cancel", function()
                            oblisk.polkit:invoke("cancel")
                        end, "polkit-cancel", { tone = "quiet" }),
                    },
                },
            }, {
                width = theme.dialog_width,
                align_h = "Center",
                align_v = "Center",
                spacing = theme.spacing.md,
                padding = { top = pad, right = pad, bottom = pad, left = pad },
                radius = theme.radius.lg,
                background = theme.GLASS,
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
