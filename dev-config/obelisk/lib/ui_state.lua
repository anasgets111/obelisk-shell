-- Bar-indicator rect for the panel host. ADR-0049's amendment routes it through config, since
-- `on_click` receives the button rect (ADR-0050 decision 3) and writes the named `state` read by
-- the surface.
-- Only `x` is read now that `modules/shell/panel_host.lua` is a layer surface: it places the card
-- below the bar and clamps it to the output instead of popup `anchor_rect`/`gravity`. Keep the
-- other fields because `on_click` still supplies this shape; narrowing it only hides destructuring.
-- The initial is the indicator's declared size. The sole require is one-way: pure `lib/util` has no
-- nodes/signals and cannot create a cycle.
local util = require("lib.util")

local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 70, height = 24 })
local settings_open = state("settings_open", false)

-- `modules/shell/panel_host.lua`'s shared-surface signals: whether it is up and its current panel.
-- One surface serves every bar panel, matching Quickshell's `Modules/Shell/PanelHost.qml`; one slot
-- enforces that rather than five files coordinating.
local panel_open = state("panel_open", false)
local panel_kind = state("panel_kind", "")

-- The one modal on screen, `ShellUiState.activeModal`: `"launcher"`, `"wallpaper_picker"`,
-- `"idle_settings"` or `""`. One string rather than a boolean per modal, so two can never be open
-- at once and opening one is what closes the last (the mirror's `openModal`). Shared `state`
-- because each modal's own close and its bar button both write it, and so one compositor keybind
-- opens and closes one: `obelisk toggle modal launcher` sets it, or clears it when it already is.
local active_modal = state("modal", "")

-- ## Which notifications have already had their turn as a popup
-- Popup and history are two presentations of one Supervisor notification; `dismiss` removes both,
-- with no third "stop popping but keep listed" state. The config therefore tracks this view fact.
-- Key by `util.notification_key`, id plus timestamp, so `replaces_id` content gets a new turn.
-- Replace the set wholesale, pruning expired/dismissed entries and keeping it within the feed's cap
-- of twenty (§ 2.7).
local popup_seen = state("notification_popup_seen", {})

local function mark_popups_seen()
    local seen = {}
    local n = obelisk.notifications:get()
    for _, notification in ipairs((n and n.feed) or {}) do
        seen[util.notification_key(notification)] = true
    end
    popup_seen:set(seen)
end

-- ## Joining a network that broadcasts no name
-- The Supervisor drops empty-SSID access points from `available_networks`, so a hidden network has
-- no row to click: the join starts from a typed name instead. `NetworkPanel.qml` walks one sheet
-- through three steps -- name, then a wait, then the password -- and these are the three signals it
-- is drawn from.
--
-- `hidden_prompt` is the mirror's `isHiddenTarget`: the flow is running. `hidden_draft` is what is
-- in the name field this instant, kept because the Next button needs the text a `textfield` only
-- ever hands to `on_change` (ADR-0092 decision 5). `hidden_ssid` is the mirror's `targetSsid`: the
-- name once it has been submitted, which titles the rest of the sheet and is what a Retry
-- reconnects to. None of them is a secret; the password half never passes through Lua at all.
local hidden_prompt = state("network_hidden_prompt", false)
local hidden_draft = state("network_hidden_draft", "")
local hidden_ssid = state("network_hidden_ssid", "")

-- Which step of the credential sheet is on screen, `""` for none: the mirror's
-- `ssidMode`/`waitingMode`/`passwordMode` as one string, because they are points on one path and
-- never two at once. `panel_host` reads it for keyboard focus, and the panel reads it for drawing.
--
-- A password prompt outranks the hidden steps because it answers a plain click on a secured row,
-- where no name was typed. `password_ssid` names whichever network is being asked about (§ 2.5).
--
-- The end of a hidden join is *read*, not latched. A `computed` may not have side effects
-- (ADR-0021), so `n.ssid` reaching the typed name finishes the sheet instead of an
-- `onConnectSucceeded` callback.
--
-- `connect_error` is checked last. `begin_connect` and `request_password` clear it on each fresh
-- attempt, but only once the Supervisor has answered, so an old error cannot outvote the attempt in
-- flight.
local credential_step = computed({ hidden_prompt, hidden_ssid, obelisk.network }, function(active, name, n)
    if n and n.password_ssid ~= nil then
        return "password"
    end
    if not active then
        return ""
    end
    if name == "" then
        return "name"
    end
    if n and n.ssid == name then
        return ""
    end
    if n and n.connecting_ssid ~= name and n.connect_error ~= nil then
        return "failed"
    end
    return "waiting"
end)

-- Whether the sheet on screen belongs to a hidden join, which is what stands in for the access
-- point list while it runs (`visible: !root.isHiddenTarget` in the mirror). A password asked for a
-- row that *is* listed leaves the list alone, since that row is the thing being asked about.
local hidden_join = computed({ hidden_prompt, credential_step }, function(active, step)
    return active and step ~= ""
end)

-- Every way out of the sheet, from Escape to closing the panel. `cancel_connect` clears a parked
-- intent and is a no-op otherwise, so this is safe on every closing edge, including ones where
-- nothing was pending.
local function clear_network_prompts()
    hidden_prompt:set(false)
    hidden_draft:set("")
    hidden_ssid:set("")
    obelisk.network:invoke("cancel_connect")
end

-- The sheet's Cancel, which also stops a join already in flight (`abort_connect`). Closing the
-- panel stays `clear_network_prompts` alone, so a join survives the panel going away.
local function cancel_network_join()
    obelisk.network:invoke("abort_connect")
    clear_network_prompts()
end

local function open_hidden_prompt()
    clear_network_prompts()
    hidden_prompt:set(true)
end

-- Discovery runs while the bluetooth panel shows, as `BluetoothPanel.qml`'s `shouldDiscover`. The
-- literal kind, like `"notifications"` below: requiring the panel here would be a cycle.
local function set_bluetooth_discovery(on)
    local b = obelisk.bluetooth:get()
    if b ~= nil and b.enabled and b.discovering ~= on then
        obelisk.bluetooth:invoke(on and "start_discovery" or "stop_discovery")
    end
end

-- The MAC whose codec list is open in the bluetooth panel, or `""` for none. It is the mirror's
-- `showCodecFor`, cleared when the panel goes away as the mirror clears it on close.
local bluetooth_codec_for = state("bluetooth_codec_for", "")

local function leave_bluetooth_panel()
    if panel_open:get() and panel_kind:get() == "bluetooth" then
        set_bluetooth_discovery(false)
        bluetooth_codec_for:set("")
    end
end

-- The panel host's single close path, including prompts. `network:connect` on an unsaved secured
-- network parks intent and raises `password_ssid` (ADR-0085); `cancel_connect` clears it and
-- is a no-op otherwise, so generic close cannot clear `connect_error` accidentally.
-- Keep it here rather than in `panel_host`'s click-outside catcher and the toggle.
-- One writer per edge costs one capability call in `ui_state`.
local function close_panel()
    -- Reading history counts as seeing its notifications. Mark on the way out, not only in, so
    -- arrivals while the panel was open do not get another popup turn.
    if panel_open:get() and panel_kind:get() == "notifications" then
        mark_popups_seen()
    end
    leave_bluetooth_panel()
    panel_open:set(false)
    clear_network_prompts()
end

-- Clicking an indicator opens its panel; clicking it again closes it, matching the mirror after
-- `panel_host` stopped being an `xdg_popup` (ADR-0087).
-- Set-only was required under the old grab: niri delivered the opening-button click because the bar
-- was the popup's parent inside the grab tree, so toggling closed the panel just opened. It also
-- fought `on_dismiss`, which already wrote false; switching panels hit both edges, and `on_dismiss`
-- had no token identifying the popup, sometimes requiring a second click.
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
    -- Switching panels ends the network panel's prompts as surely as closing does. Left standing,
    -- a pending password would keep this surface `Exclusive` over a panel that has no field in it.
    clear_network_prompts()
    leave_bluetooth_panel()
    -- A panel and a modal never share the screen (`openPanel` clears `activeModal`).
    active_modal:set("")
    popup_anchor:set(rect)
    panel_kind:set(kind)
    panel_open:set(true)
    if kind == "bluetooth" then
        set_bluetooth_discovery(true)
    end
end

-- Whether `kind` is on screen, matching `ShellUiState.isPanelOpen(kind)` and indicator rings.
-- Both signals matter: `panel_kind` survives close, so reading it alone leaves the former indicator
-- ringed.
local function panel_showing(kind)
    return computed({ panel_open, panel_kind }, function(open, current)
        return open and current == kind
    end)
end

local function modal_showing(kind)
    return active_modal:map(function(current)
        return current == kind
    end)
end

-- Opening a modal closes any panel, as the mirror's `openModal` clears `activePanelId`; nothing
-- else about the panel changes, so a re-open lands where it was.
local function open_modal(kind)
    if panel_open:get() then
        close_panel()
    end
    active_modal:set(kind)
end

-- Closes `kind` only if it is the one showing, so a modal's own close cannot dismiss a later one.
local function close_modal(kind)
    if active_modal:get() == kind then
        active_modal:set("")
    end
end

local function toggle_modal(kind)
    if active_modal:get() == kind then
        close_modal(kind)
    else
        open_modal(kind)
    end
end

local launcher_open = modal_showing("launcher")
local wallpaper_picker_open = modal_showing("wallpaper_picker")
local idle_settings_open = modal_showing("idle_settings")

-- ## The notification card's own state
-- Three view signals say which card/group is open; the Supervisor neither knows nor should know.
-- Keep them here because the card is drawn in both `modules/notification/popup.lua` and
-- `modules/bar/panels/notification_history.lua`, and expansion must match between them.
-- Use tables, not one signal per group: application keys appear only when notifications arrive, and
-- Minting registry entries at resolve time would grow the session. Table `initial` is not edited
-- on reload (ADR-0044 decision 5), so open state survives config saves.
local expanded_groups = state("notification_expanded_groups", {})
local expanded_messages = state("notification_expanded_messages", {})

-- Reply draft: id (`0` means none) and text. One slot matches the Renderer's plain-field buffer.
-- The id keeps Send honest, so Send on A with B's draft sends nothing (ADR-0109).
-- Every inline-reply card draws `Loader { active: hasInlineReply }`; no open state remains.
-- No Reply button remains.
local reply_draft_id = state("notification_reply_draft_id", 0)
local reply_draft = state("notification_reply_draft", "")

-- Toggle one table key. Copy first so identity comparison does not mutate the previous value during
-- resolve that may roll back.
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
-- `keyboard_interactivity` to this and its hover (ADR-0109), so click-to-focus compositors do not
-- drop the keyboard when the pointer leaves mid-sentence. Pure, so it can be `computed`.
local reply_pending = computed({ reply_draft_id, reply_draft, obelisk.notifications }, function(id, text, n)
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
    obelisk.notifications:invoke("reply", id, text)
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
    set_bluetooth_discovery = set_bluetooth_discovery,
    bluetooth_codec_for = bluetooth_codec_for,
    hidden_prompt = hidden_prompt,
    hidden_draft = hidden_draft,
    hidden_ssid = hidden_ssid,
    credential_step = credential_step,
    hidden_join = hidden_join,
    open_hidden_prompt = open_hidden_prompt,
    clear_network_prompts = clear_network_prompts,
    cancel_network_join = cancel_network_join,
    panel_showing = panel_showing,
    launcher_open = launcher_open,
    wallpaper_picker_open = wallpaper_picker_open,
    idle_settings_open = idle_settings_open,
    active_modal = active_modal,
    modal_showing = modal_showing,
    toggle_modal = toggle_modal,
    close_modal = close_modal,
}
