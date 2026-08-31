-- Mirrors ActiveWindow.qml.
--
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- The first `process.run` in this config, and the reason `json.decode` exists (docs/adr/0057,
-- build-steps.md section 6 item 2). `niri msg -j focused-window` reports something no capability
-- carries: ADR-0056 keeps window lists out of `workspaces`, so before the decoder this data was
-- unreachable from Lua no matter how many subprocesses a config spawned.
--
-- The accumulate-then-decode shape is the one every JSON-emitting subprocess needs, not a
-- flourish. `out_cb` fires once per line with the newline stripped, because the Supervisor reads
-- the child through `BufReader::lines()`, so a pretty-printed document arrives in pieces and only
-- `exit_cb` knows the buffer is whole.
local window_title = state("window_title", "click for the focused window")

-- The same budget `modules/bar/indicators/media.lua` uses, because the two share the centre zone
-- one at a time and a title that changed width when the player stopped would move the whole bar.
local TITLE_LIMIT = 44

-- Every answer arrives after the click that asked for it, and nothing cancels a request the config
-- has moved on from: `exit_cb` fires whenever the child exits, however stale its question has
-- become. Without this counter a second click, or the reset below, is silently overwritten when the
-- first click's reply lands late, and a rapid double click shows whichever answer the scheduler
-- happened to finish last rather than the newer one. Every subprocess-backed module has this
-- problem, so the demo carries the guard rather than pretending it does not.
local title_request = 0

-- Bumping the counter is what cancels the outstanding request, so the fetch and the reset both go
-- through here instead of each remembering to do it.
local function claim_title_request()
    title_request = title_request + 1
    return title_request
end

local function refresh_window_title()
    local request = claim_title_request()
    local buffer = {}
    process.run("niri", { "msg", "-j", "focused-window" }, function(line, stream)
        if stream == "stdout" then
            table.insert(buffer, line)
        end
    end, function(code)
        if request ~= title_request then
            return
        end
        if code ~= 0 then
            window_title:set(string.format("niri msg exited %s", tostring(code)))
            return
        end
        local window, err = json.decode(table.concat(buffer))
        if not window then
            -- The ambiguous case `json.decode`'s doc comment names, met in the first config to
            -- call it: a bare top-level `null` decodes cleanly to `nil`, so `if not window` cannot
            -- tell success from a parse error. `err` is the only thing that can, which is why it is
            -- read rather than dropped.
            window_title:set(err and "undecodable" or "nothing focused")
            return
        end
        window_title:set(window.title or "untitled")
    end)
end

-- The focused window's own icon, and the second `oblisk.applications` consumer (docs/adr/0061).
-- Its source is `oblisk.workspaces.active_client.class`, not the `process.run` above: that is a
-- toplevel's `app_id` (ADR-0056 decision 5), which is exactly the spelling `util.app_entry` maps
-- onto a `.desktop` entry. Before the capability existed there was nowhere for an `app_id` to
-- become an icon at all, which is the caller ADR-0054 decision 5 said would arrive one day.
--
-- So the icon tracks focus live while the title beside it waits for a click, and that split is
-- deliberate rather than an oversight: the title is this file's `process.run`/`json.decode`
-- demonstration (ADR-0057) and stays click-driven, while an icon that only updated when clicked
-- would sit there showing the wrong application.
local focused_icon = icon {
    name = computed({ oblisk.applications, oblisk.workspaces }, function(applications, workspaces)
        local client = workspaces and workspaces.active_client
        local entry = util.app_entry(applications, client and client.class)
        return (entry and entry.icon) or ""
    end),
    size = theme.icon.lg,
    align_v = "Center",
}

-- No pill. `ActiveWindow.qml` puts the icon and the title straight on the bar with no ground behind
-- them, which is what makes the centre read as a caption rather than as one more control. The
-- button is still the click target and still has no background of its own.
local window_title_module = row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        focused_icon,
        button {
            -- Content-sized, so the centre zone is its own content's width and lands on the bar's
            -- midpoint. It was a fixed 216px box with the title centred inside it, which centres
            -- nothing: the icon sat at the box's left edge and the title in its middle.
            height = theme.item_height,
            align_v = "Center",
            on_click = function(_, button)
                if button == "left" then
                    refresh_window_title()
                else
                    claim_title_request()
                    window_title:set("click for the focused window")
                end
            end,
            -- `text.content` takes a signal directly (ADR-0044), so this needs no `label` wrapper:
            -- the signal already holds a string on every path above, including both failure paths.
            -- Capped at a codepoint budget rather than elided into a box, for the reason
            -- `lib/util.lua`'s `truncate` gives: a window title is unbounded and this node has to
            -- stay its own width for the zone around it to centre.
            children = { cell(window_title:map(function(title)
                return util.truncate(title, TITLE_LIMIT)
            end), theme.FG, theme.font.sm, { align_v = "Center" }) },
        },
    },
}

return window_title_module
