local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

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
        window_title:set(util.truncate(window.title or "untitled", 28))
    end)
end

local window_title_module = pill({
    button {
        width = 210,
        height = 24,
        on_click = function(_, button)
            if button == "left" then
                refresh_window_title()
            else
                claim_title_request()
                window_title:set("click for the focused window")
            end
        end,
        -- `text.content` takes a signal directly (ADR-0044), so this needs no `label` wrapper: the
        -- signal already holds a string on every path above, including both failure paths.
        children = { cell(window_title, theme.DIM) },
    },
})

return window_title_module
