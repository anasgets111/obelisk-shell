-- Mirrors `Services/SystemInfo/ScreenRecordingService.qml`: one `gpu-screen-recorder`, started on a
-- region or a whole output, pausable, and saved with a notification offering to play it.
--
-- The mirror is 223 lines, with about 180 for reload-safe pid ownership. `session_process`
-- (ADR-0175) lets the Supervisor hold the child across reloads; `oblisk.processes` reports it, so
-- this file needs no launch script, lock file, or poll.
--
-- The config owns the argv, file name, pause arithmetic, and notification.
local store = require("lib.store")

local RECORDER = "screen-recorder"

-- `SIGINT` is how `gpu-screen-recorder` is told to finish: it writes the container's index on the
-- way out. `SIGTERM`, the default, would leave an unplayable file, and this is also the signal the
-- Supervisor uses when the session ends underneath a live recording.
local recorder = session_process { name = RECORDER, stop_signal = "INT" }

-- `_qualityPresets`: the panel's three words are not the encoder's five levels.
local QUALITY = { low = "medium", medium = "high", high = "very_high" }

-- `_audioArgs`. `default_output|default_input` is one argument: gpu-screen-recorder mixes the two
-- sources itself, which is why "Desktop + Mic" is a preset here rather than two recordings.
local AUDIO = {
    off = {},
    desktop = { "-a", "default_output", "-ac", "aac" },
    mic = { "-a", "default_output|default_input", "-ac", "aac" },
}

-- Config-side view facts. `capture_label` is what the panel calls this capture -- an output name or
-- "Region 1920x1080" -- and `output_path` the file being written; neither is anything the
-- Supervisor could know, since it was handed an argv.
local capture_label = state("recorder_label", "")
local output_path = state("recorder_path", "")

-- True between asking for a capture and the recorder answering: `slurp` is a whole subprocess of
-- user interaction, and the buttons must not offer a second start while it is up.
local starting = state("recorder_starting", false)

-- ## Pause arithmetic
--
-- `started_at` is the Supervisor's, so it survives reloads; the pause bookkeeping is this config's,
-- because pausing is not something a process reports. `paused_total` accumulates finished pauses
-- and `paused_at` timestamps an open one, zero meaning none. Elapsed time is the difference.
--
-- Both are `state`, so they survive an in-place reload and reset on a generation swap -- after
-- which paused seconds count as recorded ones. The mirror has the same hole across a Quickshell
-- restart and documents it the same way; a debounced disk write per pause is not worth closing it.
local paused_total = state("recorder_paused_total", 0)
local paused_at = state("recorder_paused_at", 0)

local recording = recorder.running:map(function(up)
    return up == true
end)
local paused = paused_at:map(function(at)
    return at > 0
end)

-- The output the capture defaults to: `WorkspaceService.focusedOutput`. Only the focused monitor
-- carries `focused_workspace` (ADR-0056 decision 4), which is how it is identified.
local monitor = oblisk.workspaces:map(function(w)
    for _, out in ipairs((w and w.outputs) or {}) do
        if out.focused_workspace ~= nil then
            return out.name
        end
    end
    return ""
end)

-- `StandardPaths.MoviesLocation`. A config has no XDG lookup, so ask the tool that owns it, once
-- per session behind the same `state` guard `lib/identity.lua` uses. `$HOME/Videos` is the
-- fallback, which is what `xdg-user-dir` itself answers when the user has no `user-dirs.dirs`.
local directory = state("recorder_directory", "")
if directory:get() == "" then
    process.run("xdg-user-dir", { "VIDEOS" }, function(line)
        local trimmed = line:match("^%s*(.-)%s*$")
        if trimmed ~= "" then
            directory:set(trimmed)
        end
    end, function()
        if directory:get() == "" then
            directory:set((os.getenv("HOME") or "") .. "/Videos")
        end
    end)
end

local function setting(key, fallback)
    local value = store.screen_recorder:get()
    if type(value) ~= "table" or value[key] == nil then
        return fallback
    end
    return value[key]
end

local function set_setting(key, value)
    local current = store.screen_recorder:get()
    local next_settings = {}
    if type(current) == "table" then
        for k, v in pairs(current) do
            next_settings[k] = v
        end
    end
    next_settings[key] = value
    store:set("screen_recorder", next_settings)
end

local function format_elapsed(seconds)
    seconds = math.max(0, math.floor(seconds))
    local hours = math.floor(seconds / 3600)
    local minutes = math.floor(seconds / 60) % 60
    if hours > 0 then
        return string.format("%d:%02d:%02d", hours, minutes, seconds % 60)
    end
    return string.format("%d:%02d", minutes, seconds % 60)
end

local function elapsed_of(now, began, banked, open_since)
    if began == nil or began <= 0 then
        return 0
    end
    local held = banked + (open_since > 0 and (now - open_since) or 0)
    return math.max(0, now - began - held)
end

local elapsed_text = computed(
    { oblisk.system, recorder.started_at, paused_total, paused_at, recording },
    function(s, began, banked, open_since, up)
        if not up then
            return ""
        end
        return format_elapsed(elapsed_of((s and s.time) or os.time(), began, banked, open_since))
    end
)

local function detached(cmd, args)
    process.run(cmd, args, function() end, function() end)
end

-- `_launchRecorder`. The file name is the launch time, so two captures in one session cannot
-- collide, and the extension follows the container the panel chose.
local function launch(capture_args, label)
    local container = setting("container", "mp4")
    local dir = directory:get()
    if dir == "" then
        dir = (os.getenv("HOME") or "") .. "/Videos"
    end
    local path = string.format("%s/%s.%s", dir:gsub("/$", ""), os.date("%Y%m%d_%H%M%S"), container)

    local args = {}
    local function append(list)
        for _, value in ipairs(list) do
            args[#args + 1] = value
        end
    end
    append(capture_args)
    append({ "-o", path })
    append({ "-q", QUALITY[setting("quality", "high")] or "very_high" })
    -- `math.floor` before `tostring`: a frame rate that made a round trip through JSON can come
    -- back as a float, and gpu-screen-recorder refuses `-f 60.0`.
    append({ "-f", tostring(math.floor(tonumber(setting("fps", 60)) or 60)) })
    append(AUDIO[setting("audio", "desktop")] or AUDIO.desktop)
    append({ "-cursor", "yes" })

    capture_label:set(label)
    output_path:set(path)
    paused_total:set(0)
    paused_at:set(0)
    starting:set(true)
    recorder:start("gpu-screen-recorder", args)
end

-- `startRecording(mode)`. `"selection"` puts `slurp` on screen first; its stdout is the region and
-- a non-zero exit is the user pressing Escape, which is a cancel rather than a failure.
local function start(mode)
    if recording:get() or starting:get() then
        return
    end
    if mode ~= "selection" then
        local output = monitor:get()
        if output == "" then
            return
        end
        launch({ "-w", output }, output)
        return
    end

    starting:set(true)
    local region = ""
    process.run("slurp", { "-f", "%wx%h+%x+%y" }, function(line, stream)
        if stream == "stdout" then
            region = region .. line
        end
    end, function(code)
        starting:set(false)
        local selected = region:match("^%s*(.-)%s*$")
        if code ~= 0 or selected == "" or recording:get() then
            return
        end
        -- `-w <WxH+X+Y>` rather than the mirror's `-w region -region <WxH+X+Y>`. The installed
        -- gpu-screen-recorder deprecates the second form -- "use -w with region directly instead"
        -- and on this version it also fails: a live region capture logged
        -- `gsr_encoder_receive_packets: failed to write frame index 1 to muxer, Invalid argument`
        -- and wrote nothing, while the same geometry through `-w` records cleanly.
        launch({ "-w", selected }, string.format("Region %s", selected:match("^[^+]*")))
    end)
end

local function stop()
    if not recording:get() then
        return
    end
    recorder:signal("INT")
end

-- `togglePause` is `SIGUSR2` either way; which edge it was is this config's bookkeeping, since the
-- recorder reports only that it is still up.
local function toggle_pause()
    if not recording:get() then
        return
    end
    recorder:signal("USR2")
    local open_since = paused_at:get()
    if open_since > 0 then
        paused_total:set(paused_total:get() + (os.time() - open_since))
        paused_at:set(0)
    else
        paused_at:set(os.time())
    end
end

local function toggle()
    if recording:get() then
        stop()
    else
        start()
    end
end

-- Notify on every end, not only a requested one: a recorder that died on its own still
-- wrote a file; saying nothing loses the capture.
--
-- Exit status decides the notification. `gpu-screen-recorder` answers `SIGINT` by writing the
-- container's index and exiting 0, so zero means there is a file worth offering. Anything else is a
-- refusal -- a codec it cannot open or an audio device that is not there -- and it happens fast
-- enough that the mirror's unconditional "Recording saved" lands on a missing file.
-- Verified: a bad `-a` argument produced "Recording saved · 0:00" over nothing.
--
-- `-A default=Play` arms clicking the popup itself and is hidden from the action row, so one button
-- is drawn; `notify-send` then blocks until the popup expires and prints the chosen key. That
-- process outliving a reload is fine -- it is a `process.run` child with nothing to finish.
local function announce_saved(finished_at, began, exit_code)
    local path = output_path:get()
    if path == "" then
        return
    end
    local name = path:match("[^/]+$") or path

    if exit_code ~= nil and exit_code ~= 0 then
        process.run("notify-send", {
            "-a", "Screen Recorder",
            "-i", "media-record",
            "-u", "critical",
            "Recording failed",
            string.format("gpu-screen-recorder exited %d; see the shell's log", exit_code),
        }, function() end, function() end)
        return
    end

    local duration = format_elapsed(elapsed_of(finished_at, began, paused_total:get(), paused_at:get()))
    local chosen = ""
    process.run("notify-send", {
        "-a", "Screen Recorder",
        "-i", "media-record",
        "-t", "5000",
        "-e",
        "-A", "default=Play",
        "-A", "play=Play",
        "Recording saved",
        string.format("%s · %s", duration, name),
    }, function(line, stream)
        if stream == "stdout" then
            chosen = chosen .. line
        end
    end, function()
        local key = chosen:match("^%s*(.-)%s*$")
        if key == "default" or key == "play" then
            detached("xdg-open", { path })
        end
    end)
end

local function session_of(payload)
    return ((payload or {}).sessions or {})[RECORDER]
end

-- One handler for both edges the Supervisor can report. `starting` is cleared by whichever arrives:
-- the recorder came up, or it never did and `start_error` says why.
oblisk.processes:on_change(function(current, previous)
    local was = session_of(previous)
    local now = session_of(current)
    if now == nil then
        return
    end
    if now.running then
        starting:set(false)
        return
    end
    if now.start_error ~= "" then
        starting:set(false)
    end
    if was ~= nil and was.running then
        announce_saved(os.time(), was.started_at, now.exit_code)
        paused_total:set(0)
        paused_at:set(0)
    end
end)

return {
    recording = recording,
    paused = paused,
    starting = starting,
    elapsed_text = elapsed_text,
    capture_label = capture_label,
    output_path = output_path,
    start_error = recorder.start_error,
    monitor = monitor,
    directory = directory,
    setting = setting,
    set_setting = set_setting,
    start = start,
    stop = stop,
    toggle_pause = toggle_pause,
    toggle = toggle,
    open_directory = function()
        local dir = directory:get()
        if dir ~= "" then
            detached("xdg-open", { dir })
        end
    end,
}
