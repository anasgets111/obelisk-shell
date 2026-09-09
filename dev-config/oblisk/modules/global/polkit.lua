-- Mirrors Modules/Global/PolkitDialog.qml: polkitd's authorization prompt (ADR-0114). Reading
-- `oblisk.polkit` registers this shell as the session agent (ADR-0070).
--
-- Dropped: Escape-to-cancel, because masked-field Escape clears and stays (ADR-0092), and the `●`
-- mask, because `mask_character` is one byte. `submit = true` makes Authenticate equal Enter; the
-- password remains in a native buffer no callback can read.
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
    -- Exclusive while open: the sole `secure_submit` field is armed on keyboard focus, so no click
    -- is needed (see `modules/global/lock.lua`).
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
                -- `PolkitDialog.qml:112` draws polkitd's own `inputPrompt` here and hides the line
                -- when it is empty. `PolkitState` carries no such field, so this stays the fixed
                -- label the prompt always is in practice; see ADR-0163.
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
                            placeholder = "Password",
                            mask_character = "*",
                            secure_submit = { capability = "polkit", action = "authenticate" },
                            font_size = theme.font.sm,
                        },
                    },
                },
                -- The mirror says "Authentication Failed"; ours carries PAM's reason. With no
                -- animation, "checking" is the spinner's text.
                cell(read(function(p)
                    return p.authenticating and "checking..." or p.error
                end), oblisk.polkit:map(function(p)
                    return (p and p.authenticating) and theme.DIM or theme.RED
                end), theme.font.sm, {
                    width = "Fill",
                    visible = util.shown_when(oblisk.polkit, function(p)
                        return p.authenticating or p.error ~= ""
                    end),
                }),
                row {
                    width = "Fill",
                    align_h = "End",
                    spacing = theme.spacing.sm,
                    children = {
                        action_button("cancel", function()
                            oblisk.polkit:invoke("cancel")
                        end, "polkit-cancel", { tone = "quiet" }),
                        action_button("authenticate", nil, "polkit-authenticate", { tone = "solid", submit = true }),
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
                blur = true,
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
