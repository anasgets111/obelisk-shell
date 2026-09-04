-- The rect of the bar indicator the panel host hangs its card off. ADR-0049's amendment settles
-- where it comes from: not off the input-dispatch stack, but through the config, because `on_click`
-- receives the button's own rect (ADR-0050 decision 3) and writes it to a named `state` signal the
-- surface reads back.
--
-- Only `x` is read now that `modules/shell/panel_host.lua` is a layer surface rather than a popup:
-- it places the card itself, below the bar and clamped to the output, where `anchor_rect` plus
-- `gravity` used to. The other three fields stay because this is still the shape `on_click` hands
-- over, and narrowing it here would only move the destructuring somewhere less obvious. The initial
-- is the indicator's declared size, which no longer has to be non-zero to keep the evaluation alive
-- but is still the honest starting value.
-- The only `require` here, and it is one-way: `lib/util` is pure helpers with no node and no
-- signal in it, so it cannot reach back and there is no cycle to worry about.
local util = require("lib.util")

local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 70, height = 24 })
local settings_open = state("settings_open", false)

-- `modules/shell/panel_host.lua`'s two signals: whether the shared surface is up, and which panel
-- it is showing. One surface for every bar panel rather than one surface each, which is what
-- Quickshell's own Modules/Shell/PanelHost.qml is for -- only one of these can be on screen at a
-- time anyway, and one slot makes that true by construction rather than by five files agreeing.
local panel_open = state("panel_open", false)
local panel_kind = state("panel_kind", "")

-- ## Which notifications have already had their turn as a popup
--
-- A popup and a history entry are two presentations of one notification, and only the Supervisor's
-- feed says the notification exists: `dismiss` removes it from both, and there is no third state
-- for "stop popping this up but keep it in the list". So the config keeps that state itself, and
-- it is a view fact rather than a Supervisor one -- which of the live notifications this shell has
-- already put in front of you.
--
-- Keyed on `util.notification_key`, id *and* timestamp, so a `replaces_id` update is a new thing
-- that gets a new turn rather than inheriting the note left on the content it replaced.
--
-- Replaced wholesale rather than merged, which is what prunes it: the set becomes exactly the feed
-- as it stands, so an entry that has since expired or been dismissed is forgotten and the table
-- cannot outgrow the feed's own cap of twenty (§ 2.7).
local popup_seen = state("notification_popup_seen", {})

local function mark_popups_seen()
    local seen = {}
    local n = oblisk.notifications:get()
    for _, notification in ipairs((n and n.feed) or {}) do
        seen[util.notification_key(notification)] = true
    end
    popup_seen:set(seen)
end

-- The one place the panel host stops showing anything, and so the one place that answers a prompt
-- it was showing. `network:connect` on an unsaved secured network parks an intent in the Supervisor
-- and raises `password_ssid` (ADR-0085); closing takes the field off screen without answering it,
-- and nothing in the config could clear that intent by itself. `cancel_connect` is a no-op when
-- nothing is pending, which is what lets a generic close spend it unconditionally rather than every
-- close also clearing `connect_error`.
--
-- Here rather than at its two callers -- `panel_host`'s click-outside catcher and the toggle below
-- -- for the reason this file keeps repeating: one writer per edge. A capability call inside a
-- module named `ui_state` is the price, and it is the smaller one.
local function close_panel()
    -- Reading the history is seeing the notifications in it, so nothing in the feed is owed
    -- another turn as a popup once this closes. Marked on the way out rather than only on the way
    -- in, because anything that arrived while the panel was up was on screen the whole time.
    if panel_open:get() and panel_kind:get() == "notifications" then
        mark_popups_seen()
    end
    panel_open:set(false)
    oblisk.network:invoke("cancel_connect")
end

-- Clicking an indicator opens its panel; clicking the same one again closes it. A toggle, which is
-- what the mirror does and what this could not do until `panel_host` stopped being an `xdg_popup`
-- (ADR-0087).
--
-- Set-only was not a style choice under the grab. niri still delivered a click on the opening
-- button to us, because the bar was the popup's own parent surface and so inside the grab's tree,
-- so a toggle would have closed the panel it was opening; and it would have fought `on_dismiss` on
-- every click landing elsewhere, since that path already wrote false. Worse, clicking the network
-- indicator while the bluetooth panel was up was both edges at once, and `on_dismiss` carried no
-- token saying which popup it dismissed, so switching panels directly sometimes took a second
-- click.
--
-- None of that survives a layer surface. Nothing dismisses this behind our back and the click that
-- switches panels reaches the indicator directly, so the three cases are just the three cases: the
-- showing panel closes, a different one replaces it, and a closed host opens.
local function toggle_panel(kind, rect)
    if panel_open:get() and panel_kind:get() == kind then
        close_panel()
        return
    end
    if kind == "notifications" then
        mark_popups_seen()
    end
    popup_anchor:set(rect)
    panel_kind:set(kind)
    panel_open:set(true)
end

-- Whether `kind` is the panel currently on screen, which is `ShellUiState.isPanelOpen(kind)` in the
-- mirror and what every bar indicator binds its accent ring to. Both signals are needed:
-- `panel_kind` keeps its last value after a close, so reading it alone leaves the indicator that
-- opened the panel ringed after the panel is gone.
local function panel_showing(kind)
    return computed({ panel_open, panel_kind }, function(open, current)
        return open and current == kind
    end)
end

-- Whether the app launcher window is up. A `state` signal rather than a bar-button toggle inline
-- (`settings_open`'s own shape): `modules/global/launcher.lua`'s close button and
-- `modules/bar/indicators/launcher_button.lua`'s open button both need to write it, the same
-- reason `settings_open` lives here instead of inside `settings.lua`.
local launcher_open = state("launcher_open", false)

-- Whether the wallpaper picker is up, on `launcher_open`'s terms: the bar button and the picker's
-- own close both write it.
local wallpaper_picker_open = state("wallpaper_picker_open", false)

-- ## The notification card's own state
--
-- Three signals, all of them "which of these is open", none of them anything the Supervisor knows
-- or should: expansion is a view of the feed, not a fact about it. Here rather than in
-- `components/notification_card.lua` for the reason every other block in this file is here -- the
-- card is drawn in two places (`modules/notification/popup.lua` and
-- `modules/bar/panels/notification_history.lua`) and a group left expanded in one should still be
-- expanded in the other.
--
-- Tables rather than a signal per group, because a group's key is an application name and so is not
-- known until a notification from it arrives: `state(name, initial)` is a registry keyed by name,
-- and minting one per app at resolve time would grow it for the life of the session. A table
-- `initial` also never counts as an edit on a reload (ADR-0044 decision 5), so what is open
-- survives a config save, which is what you want while writing this file.
local expanded_groups = state("notification_expanded_groups", {})
local expanded_messages = state("notification_expanded_messages", {})

-- The reply draft: which card it belongs to, by notification id (`0` for none), and its text.
-- One slot, not one per card, and that is not a shortcut: the Renderer holds one plain-field
-- buffer at a time, so there is only ever one live draft. The id is what keeps a Send button
-- honest -- pressing Send on card A with a draft typed into card B sends nothing (ADR-0109).
-- Every inline-reply card draws its field (the mirror's `Loader { active: hasInlineReply }`); there
-- is no "open" state any more, and no Reply button to open it.
local reply_draft_id = state("notification_reply_draft_id", 0)
local reply_draft = state("notification_reply_draft", "")

-- Toggling one key of a table signal. `set` compares by identity for a table, so a fresh copy is
-- both what makes the change land and what keeps the previous value from being mutated under a
-- resolve that may yet be rolled back.
local function toggle_key(signal, key)
    local next_open = {}
    for k, open in pairs(signal:get() or {}) do
        next_open[k] = open
    end
    next_open[key] = not next_open[key]
    signal:set(next_open)
end

local function toggle_group(key)
    toggle_key(expanded_groups, key)
end

local function toggle_message(id)
    toggle_key(expanded_messages, tostring(id))
end

-- Every keystroke lands here (`textfield`'s `on_change`, ADR-0092 decision 5): the whole text,
-- stamped with the card it was typed into.
local function set_reply_draft(id, text)
    reply_draft_id:set(id)
    reply_draft:set(text or "")
end

-- Forgets the draft if it is `id`'s. Escape's `on_cancel` and a successful send both end here.
local function clear_reply(id)
    if reply_draft_id:get() == id then
        reply_draft_id:set(0)
        reply_draft:set("")
    end
end

-- Whether a draft is pending on a card still in the feed: non-empty text, typed into a
-- notification that is still there. A surface binds its `keyboard_interactivity` to this
-- alongside its own hover (ADR-0109): under a click-to-focus compositor the keyboard must not be
-- dropped mid-sentence because the pointer wandered off the card, and a pending draft is the one
-- signal that says a sentence is in progress. Pure, so it can be a `computed`.
local reply_pending = computed({ reply_draft_id, reply_draft, oblisk.notifications }, function(id, text, n)
    if id == 0 or text == nil or text == "" then
        return false
    end
    for _, notification in ipairs((n and n.feed) or {}) do
        if notification.id == id then
            return true
        end
    end
    return false
end)

-- Sends what is in the draft and closes the field. A no-op on an empty draft rather than sending
-- one: `notifications:reply` removes the notification whatever the text was, so an empty send is a
-- card the user loses and a message the sender gets nothing from.
local function send_reply(id)
    local text = reply_draft:get()
    if reply_draft_id:get() ~= id or text == nil or text == "" then
        return
    end
    oblisk.notifications:invoke("reply", id, text)
    clear_reply(id)
end

return {
    popup_anchor = popup_anchor,
    popup_seen = popup_seen,
    expanded_groups = expanded_groups,
    expanded_messages = expanded_messages,
    reply_draft_id = reply_draft_id,
    reply_draft = reply_draft,
    reply_pending = reply_pending,
    toggle_group = toggle_group,
    toggle_message = toggle_message,
    set_reply_draft = set_reply_draft,
    clear_reply = clear_reply,
    send_reply = send_reply,
    settings_open = settings_open,
    panel_open = panel_open,
    panel_kind = panel_kind,
    toggle_panel = toggle_panel,
    close_panel = close_panel,
    panel_showing = panel_showing,
    launcher_open = launcher_open,
    wallpaper_picker_open = wallpaper_picker_open,
}
