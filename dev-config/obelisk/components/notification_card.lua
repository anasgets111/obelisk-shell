-- One application's notifications as one card, matching
-- `Modules/Notification/NotificationCard.qml`.
-- The popup stack (`modules/notification/popup.lua`) and history panel
-- (`modules/bar/panels/notification_history.lua`) share it; `opts.scope` carries their different
-- ground, timestamp, and entry motion.
-- Structure is built because `children` takes an array, not a `Bound`. A `list`'s `itemfn` runs for
-- every element on every pass (`layout::node::spec`), so moving reads rebuild the tree and
-- `state:set` dirties the scene (ADR-0044). Appearance stays bound because it does not change it.
-- Rust added nearly all card requirements: summary/body `wrap` and `max_lines` (ADR-0089),
-- `actions`/`invoke_action` (ADR-0090), `image_path` beside `app_icon` (ADR-0091), keyboard-reading
-- `textfield` (ADR-0092), `timestamp` (ADR-0093), and `hold_expiry`/`on_hover` so reading or
-- replying does not remove the card (ADR-0094, ADR-0095). Body spans preserve bold, italic,
-- underline, and accent links (ADR-0104); links also get opener buttons (ADR-0103). Cards and
-- messages slide/fade (ADR-0146, ADR-0150), and message hover ground eases (ADR-0145). The gap the
-- cards below a dismissed one close still snaps: a leaving node takes no room, so what travels out
-- of a card is clipped by the card closing over it.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local action_button = require("components.action_button")

-- Card border by urgency, matching the mirror's `_urgencyConfig` opacity: low fades into glass,
-- normal uses accent, critical is red. Read the group's newest notification, which leads the card.
local BORDER_BY_URGENCY = {
    low = theme.BORDER,
    normal = theme.ACCENT_MEDIUM,
    critical = theme.with_opacity(theme.RED, 0.6),
}

-- Shared chevron for groups and messages, with the same three properties in both places.
local function expander(is_open, on_activate, slot)
    return icon_button(is_open and icons.chevron_up or icons.chevron_down, on_activate, {
        size = theme.control.xs,
        icon_size = theme.icon.xs,
        background = theme.GLASS_CONTENT,
        background_hover = theme.GLASS_HOVER,
        border = false,
        foreground = theme.FG,
        slot = slot,
    })
end

-- One notification inside its group card.
-- `standalone` is a group of one: omit its dismiss button because the card header closes it, and
-- its ground because there is nothing to distinguish. `NotificationCard.qml` uses `isMultipleItems`
-- for both.
local function message(notification, ui, opts)
    local id = notification.id
    local expanded = (ui.expanded_messages:get() or {})[tostring(id)] or false
    local body = util.notification_body(notification.body, theme.ACCENT)
    local body_length = util.runs_length(body)
    local summary = notification.summary or ""

    local heading = {}
    -- `image_path`, not `app_icon`, is the message attachment; the application mark is in the
    -- header (ADR-0091).
    if notification.image_path then
        heading[#heading + 1] = icon {
            name = notification.image_path,
            size = theme.icon.xl,
            align_v = "Center",
        }
    end
    heading[#heading + 1] = cell(summary, theme.FG, theme.font.md, {
        width = "Fill",
        -- Centre a lone card title, but left-align a grouped message, matching the mirror's
        -- `horizontalAlignment` switch on `isMultipleItems`.
        align = opts.standalone and "Center" or "Start",
        align_v = "Center",
        wrap = "Word",
        -- `0` means "no limit" (ADR-0089), so expansion needs no second tree.
        max_lines = expanded and 0 or 2,
    })
    -- Only when requested (`showTimestamp`): history is about when; a popup is about now.
    if opts.age then
        heading[#heading + 1] = cell(opts.age, theme.DIM, theme.font.xs, { align_v = "Center" })
    end
    -- Show a chevron only when something is hidden.
    if expanded or #summary > 60 or body_length > 80 then
        heading[#heading + 1] = expander(expanded, function()
            ui.toggle_message(id)
        end, "notification-expand-" .. tostring(id))
    end
    if not opts.standalone then
        heading[#heading + 1] = icon_button(icons.close, function()
            obelisk.notifications:invoke("dismiss", id)
        end, {
            size = theme.control.xs,
            icon_size = theme.icon.xs,
            background = "#00000000",
            background_hover = "#00000000",
            border = false,
            foreground = theme.FG,
            slot = "notification-close-" .. tostring(id),
        })
    end

    local lines = { row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = heading,
    } }

    if body_length > 0 then
        lines[#lines + 1] = cell(body, theme.DIM, theme.font.sm, {
            width = "Fill",
            wrap = "Word",
            max_lines = expanded and 0 or 2,
            -- An underlined run opens without firing the message click (ADR-0106); plain words
            -- still do. Buttons below cover links elided before their words were drawn.
            on_link = function(href)
                obelisk.applications:invoke("open_url", href)
            end,
        })
    end

    -- Inline body pictures go under the text (see `util.notification_body`); `notify-send` cannot
    -- send one.
    local images = util.notification_images(notification.body)
    if #images > 0 then
        local pictures = {}
        for _, path in ipairs(images) do
            pictures[#pictures + 1] = icon { name = path, size = theme.icon.xl }
        end
        lines[#lines + 1] = row { width = "Fill", spacing = theme.spacing.sm, children = pictures }
    end

    -- Always show the reply field when supported (ADR-0109), matching
    -- `Loader { active: hasInlineReply }`, with no Reply button. Clicking it focuses the field and
    -- gives niri's `OnDemand` layer the keyboard; focusing a `textfield` arms no click
    -- (ADR-0092 decision 7).
    if notification.has_reply then
        lines[#lines + 1] = row {
            -- `lines` is built conditionally and id-less siblings zip in order (ADR-0023), so a
            -- body arriving shifts every row after it. Two `row`s pass the kind guard, and the
            -- reply row inherits the images row's node, taking the `NodeId` the Renderer holds
            -- keyboard focus by.
            id = "notification-reply-" .. tostring(id),
            width = "Fill",
            align_v = "Center",
            spacing = theme.spacing.sm,
            children = {
                textfield {
                    width = "Fill",
                    height = theme.control.md,
                    -- Use the sender's wording, such as "Reply to Alice", or ours if absent
                    -- (ADR-0101).
                    placeholder = notification.reply_placeholder or "Reply",
                    font_size = theme.font.sm,
                    foreground = theme.FG,
                    -- Each keystroke stores text for Send and renews the 60-second hold from
                    -- ADR-0094. Typing keeps the card alive while it continues; reload still lapses
                    -- the hold.
                    on_change = function(text)
                        ui.set_reply_draft(id, text)
                        obelisk.notifications:invoke("hold_expiry", 60)
                    end,
                    on_submit = function(text)
                        ui.set_reply_draft(id, text)
                        ui.send_reply(id)
                    end,
                    -- Escape empties the field and releases the keyboard (ADR-0102), discarding the
                    -- draft.
                    on_cancel = function()
                        ui.clear_reply(id)
                    end,
                },
                icon_button(icons.send, function()
                    ui.send_reply(id)
                end, {
                    size = theme.control.md,
                    icon_size = theme.icon.sm,
                    background = theme.ACCENT_MEDIUM,
                    slot = "notification-send-" .. tostring(id),
                }),
            },
        }
    end

    -- Sender buttons exclude `has_reply`: the Supervisor lifts `"inline-reply"` into its own flag
    -- (ADR-0090), and the field above draws it.
    local buttons = {}
    for index, action in ipairs(notification.actions or {}) do
        buttons[#buttons + 1] = action_button(action.label, function()
            obelisk.notifications:invoke("invoke_action", id, action.key)
        end, string.format("notification-action-%d-%d", id, index), { icon = action.icon_name })
    end
    -- One button per distinct body link, using `applications:open_url` (ADR-0103). Underlined words
    -- open it too (ADR-0106); the button exposes links whose words three-line elision cuts off.
    for index, href in ipairs(util.notification_links(notification.body)) do
        buttons[#buttons + 1] = action_button(util.link_label(href), function()
            obelisk.applications:invoke("open_url", href)
        end, string.format("notification-link-%d-%d", id, index))
    end
    if #buttons > 0 then
        -- Push buttons to the card edges. A `row` only distributes `Start`/`Center`/`End`, so add a
        -- filling spacer per gap; a lone button has no gap and stays centred.
        local spread = {}
        for index, control in ipairs(buttons) do
            if index > 1 then
                spread[#spread + 1] = rect { width = "Fill" }
            end
            spread[#spread + 1] = control
        end
        lines[#lines + 1] = row {
            width = "Fill",
            align_h = "Center",
            spacing = theme.spacing.sm,
            children = spread,
        }
    end

    local content = column {
        width = "Fill",
        spacing = theme.spacing.sm,
        padding = not opts.standalone and {
            top = theme.spacing.sm,
            right = theme.spacing.sm,
            bottom = theme.spacing.sm,
            left = theme.spacing.sm,
        } or nil,
        children = lines,
    }

    -- Clicking invokes the sender's default action or dismisses it, as the freedesktop spec
    -- defines.
    -- `invoke_action` removes it unless the sender asked it to stay (ADR-0090).
    local hovered = hover("notification-message-" .. tostring(id))
    -- Use an `if`, not `a and nil or b`, which cannot produce nil and gives lone messages a second
    -- box.
    local ground, ring = nil, nil
    if not opts.standalone then
        -- Mirror-style message box: subtle ground and hairline, accent under the pointer. Content
        -- glass is the history card's ground, so a second card ground would disappear into it.
        ground = hovered:map(function(is_hovered)
            return is_hovered and theme.ACCENT_SUBTLE or theme.BG_SUBTLE
        end)
        ring = hovered:map(function(is_hovered)
            return is_hovered and theme.ACCENT_MEDIUM or theme.BORDER_SUBTLE
        end)
    end
    local animate = {
        translate = {
            duration = theme.notification_slide_ms,
            easing = "OutCubic",
            from = { x = theme.notification_width },
        },
        opacity = { duration = theme.notification_slide_ms, from = 0 },
        exit = {
            duration = theme.notification_slide_ms,
            easing = "OutCubic",
            translate = { x = theme.notification_width },
            opacity = 0,
        },
    }
    if ground then
        animate.background = theme.animation_ms
        animate.border_color = theme.animation_ms
    end
    return button {
        -- Named for the notification it draws. A collapsed group reuses the newest message's slot;
        -- without this id, the reply field's `NodeId` and draft can move under another summary.
        id = "notification-message-" .. tostring(id),
        width = "Fill",
        hover = hovered,
        radius = theme.radius.sm,
        background = ground,
        border_width = ring and theme.border_width or nil,
        border_color = ring,
        -- Resting pose: an exit eases from what the node holds, and an unset property has
        -- nothing to leave from.
        translate = { x = 0 },
        opacity = 1,
        -- `messageContent` slides from the right on entry and dismissal. The mirror gates the card
        -- on `_animReady`, but each message delegate uses `root._shownMessageIds`; that also keeps
        -- the hard swap animated when a collapsed group replaces its newest message. A leaving node
        -- takes no room (ADR-0150), so opacity carries the exit while the card clips travel.
        animate = animate,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            -- Keep the body inert while this message has a nonempty draft (ADR-0108, ADR-0109); the
            -- X still works.
            if ui.reply_draft_id:get() == id and (ui.reply_draft:get() or "") ~= "" then
                return
            end
            if notification.has_default_action then
                obelisk.notifications:invoke("invoke_action", id, "default")
            else
                obelisk.notifications:invoke("dismiss", id)
            end
        end,
        children = { content },
    }
end

-- How long one card waits behind the one above it before entering. Four cards landing on one frame
-- read as one block (ADR-0153). Only simultaneous arrivals use it; hidden subtrees stay frozen
-- (ADR-0124, `layout::scene::prepare_node`).
local STAGGER_MS = 60

-- The entry and exit of a whole card, which is the one thing the two scopes disagree about.
--
-- `NotificationCard.qml`'s `Behavior on x` uses paint-only `translate`, not `margin`; a `Fill`-
-- width card is laid out once at full width instead of re-wrapping as its margin changes
-- (ADR-0149).
--
-- Both edges run at `notification_slide_ms` on `OutCubic`, matching the mirror:
-- `NotificationCard.qml` takes `slideAnimDuration` from `NotificationService.animationDuration`,
-- which is `Math.round(Theme.animationDuration * 1.4)`. The bare 147 is for colour transitions.
--
-- The mirror slides inside a full-screen popup; this surface is one card wide
-- (`modules/notification/popup.lua`), so travel clips quickly and the fade makes the exit legible.
--
-- Cards below still close the gap on one frame; the engine has no move transition
-- (`docs/roadmap.md`).
local function entry_animation(scope, rank)
    local exit = {
        duration = theme.notification_slide_ms,
        easing = "OutCubic",
        translate = { x = theme.notification_width },
        opacity = 0,
    }
    if scope == "history" then
        -- A history card fades without travel. It records what happened; popup travel says an event
        -- came from outside. The mirror also leaves history `_animReady` outside the popup scope.
        return {
            opacity = { duration = theme.animation_ms, from = 0 },
            exit = exit,
        }
    end
    local delay = math.max(0, ((rank or 1) - 1)) * STAGGER_MS
    return {
        translate = {
            duration = theme.notification_slide_ms,
            easing = "OutCubic",
            delay = delay,
            from = { x = theme.notification_width },
        },
        -- Hold the opacity too, so a waiting card is invisible where it waits.
        opacity = { duration = theme.notification_slide_ms, delay = delay, from = 0 },
        exit = exit,
    }
end

-- `group` is one entry of `util.group_notifications`; `ui` is `lib/ui_state`.
-- `opts.scope` mirrors `groupScope`: popup cards use heavier glass, no clock, and edge travel;
-- history uses the lighter ground, "Wed 14:32" (`showTimestamp`), and no travel.
return function(group, ui, opts)
    opts = opts or {}
    local scope = opts.scope or "popup"
    local in_history = scope == "history"
    local items = group.items or {}
    local expanded = (ui.expanded_groups:get() or {})[group.key] or false
    local is_group = #items > 1

    local title = is_group and string.format("%s (%d)", group.app_name, #items) or group.app_name
    local header = {
        -- The application's icon stays artwork, not a glyph `cell` (`components/panel_row.lua`).
        -- `icon { name = ... }` accepts a theme name or absolute path (ADR-0054 decision 2); its
        -- plate keeps arbitrary-colour artwork off the glass.
        rect {
            width = theme.notification_app_icon,
            height = theme.notification_app_icon,
            radius = theme.radius.sm,
            background = theme.BG_SUBTLE,
            border_width = theme.border_width,
            border_color = theme.BORDER_SUBTLE,
            align_v = "Center",
            children = { icon {
                name = group.app_icon or "dialog-information",
                size = theme.item_height,
                align_h = "Center",
                align_v = "Center",
            } },
        },
        -- Bold, centred title. `bold` lives on a `TextRun` (ADR-0104), not the node.
        cell({ { text = title, bold = true } }, theme.FG, theme.font.md, {
            width = "Fill",
            align = "Center",
            align_v = "Center",
        }),
    }
    if is_group then
        header[#header + 1] = expander(expanded, function()
            ui.toggle_group(group.key)
        end, "notification-group-" .. group.key)
    end
    -- Dismiss each member because § 3.2 has neither `dismiss_all` nor `dismiss_group`. Ghost style
    -- matches `variant: "ghost"`: the glyph is the control, not a filled circle beside the title.
    header[#header + 1] = icon_button(icons.close, function()
        for _, notification in ipairs(items) do
            obelisk.notifications:invoke("dismiss", notification.id)
        end
    end, {
        size = theme.control.xs,
        icon_size = theme.icon.xs,
        background = "#00000000",
        background_hover = "#00000000",
        border = false,
        foreground = theme.FG,
        slot = "notification-group-close-" .. group.key,
    })

    local children = { row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = header,
    } }

    -- Collapsed groups show the newest and count the rest in the header; expanded groups show all.
    -- The feed caps `#items` at twenty (§ 2.7), so no runaway guard is needed.
    local shown = (is_group and not expanded) and { items[1] } or items
    for _, notification in ipairs(shown) do
        children[#children + 1] = message(notification, ui, {
            -- Use rendered count, matching the mirror's `isMultipleItems`: a collapsed group
            -- renders one card-level message, not a list entry.
            standalone = #shown == 1,
            age = in_history and util.absolute_time(notification.timestamp) or nil,
        })
    end

    return column {
        width = "Fill",
        spacing = theme.spacing.sm,
        padding = {
            top = theme.spacing.md,
            right = theme.spacing.md,
            bottom = theme.spacing.md,
            left = theme.spacing.md,
        },
        -- Resting pose. History does not move in, but its exit still slides the card from here.
        translate = { x = 0, y = 0 },
        opacity = 1,
        animate = entry_animation(scope, group.rank),
        background = in_history and theme.GLASS_CONTENT or theme.GLASS,
        -- A popup card is its own sheet and blurs the desktop behind it (ADR-0195). History sits on
        -- `panel_host`'s already-blurred card, so it does not request the same region again.
        blur = not in_history,
        radius = theme.radius.md,
        border_width = theme.border_width_medium,
        border_color = BORDER_BY_URGENCY[group.urgency] or theme.BORDER,
        children = children,
    }
end
