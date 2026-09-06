-- Bar-indicator rect for the panel host. ADR-0049's amendment routes it through config, since
-- `on_click` receives the button rect (ADR-0050 decision 3) and writes the named `state` read by
-- the surface.
-- Only `x` is read now that `modules/shell/panel_host.lua` is a layer surface: it places the card
-- below the bar and clamps it to the output instead of using popup `anchor_rect`/`gravity`. Keep
-- the
-- other fields because `on_click` still supplies this shape; narrowing it only hides destructuring.
-- The initial is the indicator's declared size, no longer needed to keep evaluation alive but still
-- honest.
-- The sole require is one-way: pure `lib/util` has no nodes/signals and cannot create a cycle.
local util = require("lib.util")

local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 70, height = 24 })
local settings_open = state("settings_open", false)

-- `modules/shell/panel_host.lua`'s shared-surface signals: whether it is up and which panel it
-- shows.
-- One surface serves every bar panel, matching Quickshell's `Modules/Shell/PanelHost.qml`; only one
-- can be on screen, and one slot enforces that rather than five files coordinating.
local panel_open = state("panel_open", false)
local panel_kind = state("panel_kind", "")

-- ## Which notifications have already had their turn as a popup
-- Popup and history are two presentations of one Supervisor notification; `dismiss` removes both,
-- with no third "stop popping but keep listed" state. The config therefore tracks this view fact.
-- Key by `util.notification_key`, id plus timestamp, so `replaces_id` content gets a new turn.
-- Replace the set wholesale, pruning expired/dismissed entries and keeping it within the feed's cap
-- of twenty (§ 2.7).
local popup_seen = state("notification_popup_seen", {})

local function mark_popups_seen()
    local seen = {}
    local n = oblisk.notifications:get()
    for _, notification in ipairs((n and n.feed) or {}) do
        seen[util.notification_key(notification)] = true
    end
    popup_seen:set(seen)
end

-- The panel host's single close path, including prompts. `network:connect` on an unsaved secured
-- network parks intent and raises `password_ssid` (ADR-0085); closing hides the field, while
-- `cancel_connect` clears that intent and is a no-op otherwise, so generic close cannot clear
-- `connect_error` accidentally.
-- Keep it here rather than in `panel_host`'s click-outside catcher and the toggle: one writer per
-- edge costs one capability call in `ui_state`.
local function close_panel()
    -- Reading history counts as seeing its notifications. Mark on the way out, not only in, so
    -- arrivals while the panel was open do not get another popup turn.
    if panel_open:get() and panel_kind:get() == "notifications" then
        mark_popups_seen()
    end
    panel_open:set(false)
    oblisk.network:invoke("cancel_connect")
end

-- Clicking an indicator opens its panel; clicking it again closes it, matching the mirror after
-- `panel_host` stopped being an `xdg_popup` (ADR-0087).
-- Set-only was required under the old grab: niri delivered the opening-button click because the bar
-- was the popup's parent inside the grab tree, so toggling closed the panel just opened. It also
-- fought `on_dismiss`, which already wrote false; switching network while bluetooth was open hit
-- both edges, and `on_dismiss` had no token identifying the popup, sometimes requiring a second
-- click.
-- A layer surface removes those cases: nothing dismisses it behind our back and switching clicks
-- reach the indicator directly. The showing panel closes, a different one replaces it, or a closed
-- host opens.
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

-- Whether `kind` is on screen, matching `ShellUiState.isPanelOpen(kind)` and indicator accent
-- rings.
-- Both signals matter: `panel_kind` survives close, so reading it alone leaves the former indicator
-- ringed.
local function panel_showing(kind)
    return computed({ panel_open, panel_kind }, function(open, current)
        return open and current == kind
    end)
end

-- App launcher visibility. Like `settings_open`, keep it in shared `state` because
-- `modules/global/launcher.lua`'s close and `modules/bar/indicators/launcher_button.lua`'s open
-- both write it.
local launcher_open = state("launcher_open", false)

-- Wallpaper picker visibility; both the bar button and picker close write it.
local wallpaper_picker_open = state("wallpaper_picker_open", false)

-- Idle settings modal visibility; the bar circle's right click opens it and the modal header closes
-- it.
local idle_settings_open = state("idle_settings_open", false)

-- ## The notification card's own state
-- Three view signals say which card/group is open; the Supervisor neither knows nor should know.
-- Keep them here because the card is drawn in both `modules/notification/popup.lua` and
-- `modules/bar/panels/notification_history.lua`, and expansion must match between them.
-- Use tables, not one signal per group: application keys appear only when notifications arrive, and
-- minting registry entries at resolve time would grow for the session. Table `initial` is not an
-- edit
-- on reload (ADR-0044 decision 5), so open state survives config saves.
local expanded_groups = state("notification_expanded_groups", {})
local expanded_messages = state("notification_expanded_messages", {})

-- Reply draft: notification id (`0` means none) and text. One slot matches the Renderer, which
-- holds
-- one plain-field buffer; the id keeps Send honest, so Send on A with B's draft sends nothing
-- (ADR-0109). Every inline-reply card draws its field
-- (`Loader { active: hasInlineReply }`); no open
-- state or Reply button remains.
local reply_draft_id = state("notification_reply_draft_id", 0)
local reply_draft = state("notification_reply_draft", "")

-- Toggle one table key. Identity comparison requires a fresh copy and prevents mutating the
-- previous
-- value during a resolve that may roll back.
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

-- Store each keystroke (`textfield.on_change`, ADR-0092 decision 5), stamped with its card.
local function set_reply_draft(id, text)
    reply_draft_id:set(id)
    reply_draft:set(text or "")
end

-- Clear `id`'s draft; Escape's `on_cancel` and successful send both use this.
local function clear_reply(id)
    if reply_draft_id:get() == id then
        reply_draft_id:set(0)
        reply_draft:set("")
    end
end

-- Whether nonempty draft text belongs to a notification still in the feed. A surface binds
-- `keyboard_interactivity` to this and its hover (ADR-0109): click-to-focus compositors must not
-- drop
-- the keyboard when the pointer leaves mid-sentence. Pure, so it can be `computed`.
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

-- Send the draft and close the field. Empty is a no-op because `notifications:reply` removes the
-- notification regardless of text, losing the card while sending the sender nothing.
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
    idle_settings_open = idle_settings_open,
}
