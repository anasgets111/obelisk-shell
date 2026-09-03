-- One application's notifications as a single card: `Modules/Notification/NotificationCard.qml`.
-- Drawn in two places, which is the whole reason it is a component -- the popup stack
-- (`modules/notification/popup.lua`) and the history panel
-- (`modules/bar/panels/notification_history.lua`) show the same card and differ only in ground
-- colour and in whether an old notification is worth a dismiss button of its own.
--
-- ## Why this is built in Lua rather than bound
--
-- Everything here that decides *structure* -- how many messages a group shows, whether a reply row
-- exists, how many action buttons -- is read at build time rather than bound to a signal, because
-- `children` takes an array and not a `Bound`. That is fine and is not a workaround: a `list`'s
-- `itemfn` runs for every element on every pass (`layout::node::spec`'s own note), so this function
-- re-runs whenever anything it reads moves, and a `state:set` marks the scene dirty (ADR-0044).
-- The properties that decide *appearance* -- `max_lines` on an expanding body, a hover ground --
-- are bound in the ordinary way, since those change without changing the tree.
--
-- ## What it needs from Rust
--
-- Almost all of it landed for this card. `wrap`/`max_lines` on a summary and a body (ADR-0089),
-- `actions` and `invoke_action` for the buttons (ADR-0090), `image_path` beside `app_icon` so a
-- chat avatar and the messenger's own logo can both be drawn (ADR-0091), a `textfield` that reads
-- the keyboard (ADR-0092), `timestamp` for the age (ADR-0093), and `hold_expiry` plus `on_hover`
-- so a card does not vanish while it is being read or replied to (ADR-0094, ADR-0095).
--
-- The body's spans are drawn as they arrive -- bold, italic, underlined, a link in the accent
-- (ADR-0104) -- with a button per link that opens it (ADR-0103). What it still does without is
-- animation: a group expands and a card leaves without one.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local icon_button = require("components.icon_button")

-- A labelled button rather than a glyph circle, which is what `icon_button` gives and what an
-- action is not: `["archive"] = "Archive"` is a word the sender chose and the whole point is that
-- the user reads it before pressing it. Local rather than a component, on this repo's own bar --
-- one call site is a local, two in agreement are a component.
--
-- `icon_name` is the theme icon a sender that set `action-icons` named through the key (ADR-0090),
-- drawn beside the label, or alone when the sender sent no label -- a media notification's
-- prev/play/next is three glyphs, not three words.
local function action_button(label, on_activate, slot, icon_name)
    local hovered = hover(slot)
    local children = {}
    if icon_name then
        children[#children + 1] = icon { name = icon_name, size = theme.icon.sm, align_v = "Center" }
    end
    if label and label ~= "" then
        children[#children + 1] = cell(label, theme.FG, theme.font.sm, { align = "Center", align_v = "Center" })
    end
    return button {
        height = theme.control.md,
        align_v = "Center",
        radius = theme.radius.md,
        hover = hovered,
        background = hovered:map(function(is_hovered)
            return is_hovered and theme.ACCENT_LIGHT or theme.ACCENT_SUBTLE
        end),
        border_width = theme.border_width,
        border_color = theme.ACCENT_MEDIUM,
        padding = { left = theme.spacing.md, right = theme.spacing.md },
        on_click = function(_, mouse_button)
            if mouse_button == "left" then
                on_activate()
            end
        end,
        -- A `button` stacks its children; the row is what puts a glyph beside a word.
        children = { row { height = "Fill", align_v = "Center", spacing = theme.spacing.xs, children = children } },
    }
end

-- The card's border by urgency, the mirror's `_urgencyConfig` colours at its border opacity: a
-- low-priority card fades into the glass, a normal one carries the accent, a critical one is red.
-- Read off the group's newest notification, which is the one the card leads with.
local BORDER_BY_URGENCY = {
    low = theme.BORDER,
    normal = theme.ACCENT_MEDIUM,
    critical = theme.with_opacity(theme.RED, 0.6),
}

-- The chevron that opens and closes something, pointing the way it will move. Two callers (a group
-- and a message) and the same three properties, so it is one local rather than twice five lines.
local function expander(is_open, on_activate, slot)
    return icon_button(is_open and icons.chevron_up or icons.chevron_down, on_activate, {
        size = theme.control.xs,
        icon_size = theme.icon.xs,
        background = theme.BORDER_SUBTLE,
        slot = slot,
    })
end

-- One notification inside its group's card.
--
-- `standalone` is the group-of-one case, and it changes two things rather than being a style flag:
-- a lone message needs no dismiss button (the card's own header close does exactly the same thing)
-- and no ground of its own (there is nothing to tell it apart from). `NotificationCard.qml` spends
-- its `isMultipleItems` on the same two.
local function message(notification, ui, opts)
    local id = notification.id
    local expanded = (ui.expanded_messages:get() or {})[tostring(id)] or false
    local body = util.notification_body(notification.body, theme.ACCENT)
    local body_length = util.runs_length(body)
    local summary = notification.summary or ""

    -- The heading line: the sender's attached picture, the summary, the age, and the two controls.
    local heading = {}
    -- `image_path`, not `app_icon`: this is what the sender attached to *this* message -- an
    -- avatar, album art, a screenshot -- and the application's own mark is already in the card
    -- header above. Drawing both is the point of ADR-0091 having split them.
    if notification.image_path then
        heading[#heading + 1] = icon {
            name = notification.image_path,
            size = theme.icon.xl,
            align_v = "Center",
        }
    end
    heading[#heading + 1] = cell(summary, theme.FG, theme.font.md, {
        width = "Fill",
        -- Centred on a card of one, left-aligned as one message of several: the mirror's
        -- `horizontalAlignment` switch on `isMultipleItems`. A lone summary is the card's title; a
        -- message in a list is an entry.
        align = opts.standalone and "Center" or "Start",
        align_v = "Center",
        wrap = "Word",
        -- `0` is "no limit" (ADR-0089), so expanding is one property rather than two trees.
        max_lines = expanded and 0 or 2,
    })
    -- Only where the caller asked for one (`showTimestamp`): the history, where a card is about
    -- when. A popup is about now, and "now" beside every fresh summary said nothing.
    if opts.age then
        heading[#heading + 1] = cell(opts.age, theme.TEXT_OFF, theme.font.xs, { align_v = "Center" })
    end
    -- Only when there is something hidden to show. A chevron on a one-line notification is a
    -- control that visibly does nothing, which is worse than no control.
    if expanded or #summary > 60 or body_length > 80 then
        heading[#heading + 1] = expander(expanded, function()
            ui.toggle_message(id)
        end, "notification-expand-" .. tostring(id))
    end
    if not opts.standalone then
        heading[#heading + 1] = icon_button(icons.close, function()
            oblisk.notifications:invoke("dismiss", id)
        end, {
            size = theme.control.xs,
            icon_size = theme.icon.xs,
            background = theme.BORDER_SUBTLE,
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
        -- `DIM`, the mirror's `textInactiveColor`, not `TEXT_OFF`: a body is secondary to its
        -- summary, not switched off, and at 35% alpha it read as the latter.
        lines[#lines + 1] = cell(body, theme.DIM, theme.font.sm, {
            width = "Fill",
            wrap = "Word",
            max_lines = expanded and 0 or 2,
            -- A press on an underlined run opens it and does not also fire the message's own
            -- click (ADR-0106); a press on the plain words still does. The link buttons below
            -- remain for a link the elide cut off before its words were drawn.
            on_link = function(href)
                oblisk.applications:invoke("open_url", href)
            end,
        })
    end

    -- Pictures the body carried inline, under the text rather than in it (see
    -- `util.notification_body`). Rare -- `notify-send` cannot send one -- and small when they come.
    local images = util.notification_images(notification.body)
    if #images > 0 then
        local pictures = {}
        for _, path in ipairs(images) do
            pictures[#pictures + 1] = icon { name = path, size = theme.icon.xl }
        end
        lines[#lines + 1] = row { width = "Fill", spacing = theme.spacing.sm, children = pictures }
    end

    -- The reply field, on every notification that takes one, always (ADR-0109): the mirror's
    -- `Loader { active: hasInlineReply }`, with no Reply button in front of it. A click into the
    -- field is what focuses it, and on niri that same click is what gives the surface the keyboard
    -- (an `OnDemand` layer surface is focused on click, not on the mode flip). It is inside the
    -- message's own `button` and that is safe: a press that focuses a `textfield` arms no click
    -- (ADR-0092 decision 7), which is the rule that exists so replying to a notification does not
    -- fire the notification's default action and take the card away mid-sentence.
    if notification.has_reply then
        lines[#lines + 1] = row {
            width = "Fill",
            align_v = "Center",
            spacing = theme.spacing.sm,
            children = {
                textfield {
                    width = "Fill",
                    height = theme.control.md,
                    -- The sender's own wording where it gave one -- "Reply to Alice" -- and
                    -- ours where it did not (ADR-0101).
                    placeholder = notification.reply_placeholder or "Reply",
                    font_size = theme.font.sm,
                    foreground = theme.FG,
                    -- Every keystroke does two things. It banks the text, since nothing else can
                    -- read the field back and the Send button needs it; and it re-places a short
                    -- expiry hold, which is the shape ADR-0094 was built for -- typing keeps the
                    -- card alive for exactly as long as typing lasts, and the hold lapses on its
                    -- own if the shell reloads mid-sentence.
                    on_change = function(text)
                        ui.set_reply_draft(id, text)
                        oblisk.notifications:invoke("hold_expiry", 60)
                    end,
                    on_submit = function(text)
                        ui.set_reply_draft(id, text)
                        ui.send_reply(id)
                    end,
                    -- Escape: the field is emptied and lets go of the keyboard (ADR-0102); the
                    -- draft it banked goes with it.
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

    -- The sender's own buttons. `has_reply` is not among them -- the Supervisor lifts the
    -- `"inline-reply"` key out into its own flag (ADR-0090) and the field above is what it draws.
    local buttons = {}
    for index, action in ipairs(notification.actions or {}) do
        buttons[#buttons + 1] = action_button(action.label, function()
            oblisk.notifications:invoke("invoke_action", id, action.key)
        end, string.format("notification-action-%d-%d", id, index), action.icon_name)
    end
    -- One button per distinct link in the body, opened by the desktop's own handler
    -- (`applications:open_url`, ADR-0103). The underlined words open it too (ADR-0106); the button
    -- is for a link whose words the three-line elide cut off, and is the one control that says
    -- where the link goes before it is pressed.
    for index, href in ipairs(util.notification_links(notification.body)) do
        buttons[#buttons + 1] = action_button(util.link_label(href), function()
            oblisk.applications:invoke("open_url", href)
        end, string.format("notification-link-%d-%d", id, index))
    end
    if #buttons > 0 then
        lines[#lines + 1] = row {
            width = "Fill",
            -- Centred under the message (the mirror's `Layout.alignment: Qt.AlignHCenter`); a
            -- row's own `align_h` is its main-axis distribution.
            align_h = "Center",
            spacing = theme.spacing.sm,
            children = buttons,
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

    -- Clicking the message activates the sender's default action where it offered one, and
    -- dismisses where it did not. Both are what the freedesktop spec means by activating a
    -- notification, and `invoke_action` removes it afterwards on its own unless the sender asked
    -- to stay (ADR-0090), so the two paths agree on what the card does next.
    local hovered = hover("notification-message-" .. tostring(id))
    -- An `if`, not `a and nil or b`: that idiom cannot produce nil, so every lone message wore the
    -- group ground and drew a second box inside the card.
    local ground = nil
    if not opts.standalone then
        ground = hovered:map(function(is_hovered)
            return is_hovered and theme.GLASS_HOVER or theme.GLASS_CONTENT
        end)
    end
    return button {
        width = "Fill",
        hover = hovered,
        radius = theme.radius.sm,
        background = ground,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            -- Not while a reply to this message is half-typed (ADR-0108, ADR-0109). The field is
            -- one row of a card whose whole face otherwise dismisses, and a click that misses the
            -- field by a few pixels took the card and the draft with it. While a draft is pending
            -- the body is inert; the X is still there for someone who meant it.
            if ui.reply_draft_id:get() == id and (ui.reply_draft:get() or "") ~= "" then
                return
            end
            if notification.has_default_action then
                oblisk.notifications:invoke("invoke_action", id, "default")
            else
                oblisk.notifications:invoke("dismiss", id)
            end
        end,
        children = { content },
    }
end

-- `group` is one entry of `util.group_notifications`; `ui` is `lib/ui_state`.
--
-- `opts.background` and `opts.show_time` are what the two call sites disagree on: a popup floats
-- over whatever is behind it and wants the heavier glass and no clock, since it is about now; a
-- card inside an already-glassy panel wants the lighter one and "Wed 14:32", since a history is
-- about when (the mirror's `showTimestamp`). Everything else about the two is the same card,
-- which is the point.
return function(group, ui, opts)
    opts = opts or {}
    local items = group.items or {}
    local expanded = (ui.expanded_groups:get() or {})[group.key] or false
    local is_group = #items > 1

    local title = is_group and string.format("%s (%d)", group.app_name, #items) or group.app_name
    local header = {
        -- The application's own icon, never recoloured, which is why it is an `icon` node and not a
        -- glyph `cell` (`components/panel_row.lua` draws the same distinction). `app_icon` is
        -- usually a theme name and occasionally an absolute path; `icon { name = ... }` takes
        -- either (ADR-0054 decision 2). On a plate of its own, as the mirror draws it: artwork
        -- that arrives in any colour sits better on a ground than on the glass directly.
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
        -- Bold and centred, the card's title rather than a caption. One `TextRun` because `bold`
        -- lives on a run (ADR-0104), not on the node.
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
    -- Dismisses the whole group, one call per member: § 3.2 has no `dismiss_all` and no
    -- `dismiss_group`. Iterating `items` is safe because it is this pass's own array -- nothing
    -- pushes a new feed until this returns.
    -- A ghost: no ground and no ring, the mirror's `variant: "ghost"`. The glyph is the control;
    -- a filled circle beside a bold title was a second thing to look at.
    header[#header + 1] = icon_button(icons.close, function()
        for _, notification in ipairs(items) do
            oblisk.notifications:invoke("dismiss", notification.id)
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

    -- Collapsed, a group shows its newest and says how many more there are in the header count.
    -- Expanded, it shows all of them. `#items` is capped by the feed itself at twenty (§ 2.7), so
    -- there is no runaway case to guard here.
    local shown = (is_group and not expanded) and { items[1] } or items
    for _, notification in ipairs(shown) do
        children[#children + 1] = message(notification, ui, {
            standalone = not is_group,
            age = opts.show_time and util.absolute_time(notification.timestamp) or nil,
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
        background = opts.background or theme.GLASS,
        radius = theme.radius.md,
        border_width = theme.border_width_medium,
        border_color = BORDER_BY_URGENCY[group.urgency] or theme.BORDER,
        children = children,
    }
end
