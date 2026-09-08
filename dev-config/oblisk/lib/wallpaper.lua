-- Wallpaper state matching `Services/Core/WallpaperService.qml`: file and fit per output, file
-- source, and two writes. No node here; drawing, picking, and the bar button live in
-- `modules/global/wallpaper.lua`, `modules/global/wallpaper_picker.lua`, and
-- `modules/bar/indicators/wallpaper_button.lua`.
-- ## Where the choice lives
-- One `wallpapers` key in `lib/store.lua`, a `{ path, fit }` table per output matching
-- `Settings.data.wallpapers`. It survives reload and reboot; ADR-0055 decision 2 replaced a
-- `state()` value that reloads forgot. ADR-0136 removed the two flattened key families after the
-- scalar-only store changed.
-- ## Where the files come from
-- `oblisk.files` (ADR-0120) watches `FOLDER` with inotify, so a dropped image reaches the picker
-- before it opens. Start watching here during evaluation because the folder is a setting, not
-- state;
-- the picker needs the list on open and the bar's right-click needs it before any picker open.
local store = require("lib.store")

local wallpaper = {}

-- `Settings.data.wallpaperFolder` default.
wallpaper.FOLDER = "/mnt/Work/1Wallpapers/Main"
-- `FolderListModel.nameFilters`; exclude `gif` because the decoder supports none and folders often
-- contain one.
wallpaper.EXTENSIONS = { "jpg", "jpeg", "png", "webp" }
-- `WallpaperService.availableModes` reduced to `image.fit` (ADR-0055 decision 3); omit `center` and
-- `tile` because the engine draws neither.
wallpaper.FITS = {
    { value = "cover",   label = "Fill" },
    { value = "contain", label = "Fit" },
    { value = "stretch", label = "Stretch" },
}
wallpaper.DEFAULT_FIT = "cover"
-- `WallpaperService.availableTransitions`, as the config's own shader files (ADR-0184). The engine
-- ships the cross-dissolve and the ability to run a fragment shader; which effects exist is this
-- config's to say, exactly as the mirror keeps its own `Shaders/frag` directory.
-- Where the effects live. `WallpaperService.qml` scans its own `Shaders/qsb` with a
-- `FolderListModel` and offers what it finds; this is that, through `oblisk.files` (ADR-0120), so a
-- `.frag` dropped in here appears in the picker without a reload. Point it anywhere: nothing in the
-- engine knows this directory exists.
wallpaper.SHADER_FOLDER = oblisk.config_dir .. "/shaders"
wallpaper.SHADER_EXTENSIONS = { "frag" }
-- The engine's own cross-dissolve, which is no file and always available.
wallpaper.NO_SHADER = "fade"
-- `WallpaperService.qml`'s duration and curve.
wallpaper.TRANSITION_MS = 1500
wallpaper.TRANSITION_EASING = "InOutCubic"

-- `AnimatedWallpaper.qml`'s `transitionParams.randomize`, by effect name: a wipe picks a side, a
-- disc and a portal pick a centre, stripes pick a count and an angle. A shader with no row here --
-- anything dropped into the folder -- runs with every uniform at zero, which is what the engine
-- does with a parameter nothing supplies. Adding a row is how a config gives it knobs.
local RANDOM_PARAMS = {
    wipe = function()
        return { direction = math.floor(math.random() * 4), softness = 0.1 }
    end,
    disc = function()
        return { center_x = math.random(), center_y = math.random(), softness = 0.1 }
    end,
    portal = function()
        return { center_x = math.random(), center_y = math.random(), softness = 0.1 }
    end,
    stripes = function()
        return { count = math.random(4, 24), angle = math.random() * 360, softness = 0.1 }
    end,
    pixelate = function()
        return { softness = 0.35 }
    end,
}

---The watched shader folder from the last `oblisk.files` push, or `nil` before the first.
---@param f FilesState|nil
---@return Folder|nil
function wallpaper.shader_folder_in(f)
    return f and f.folders and f.folders[wallpaper.SHADER_FOLDER] or nil
end

---Effect names from one `oblisk.files` push: the built-in first, then a name per `.frag`.
---`WallpaperService.qml` strips `wp_` and `.frag.qsb`; these files carry no affix to strip.
---@param f FilesState|nil
---@return string[]
function wallpaper.effects_in(f)
    local names = { wallpaper.NO_SHADER }
    local folder = wallpaper.shader_folder_in(f)
    for _, entry in ipairs(folder and folder.entries or {}) do
        names[#names + 1] = (entry.name:gsub("%.frag$", ""))
    end
    return names
end

---`WallpaperService.validate`: the stored effect if the folder still holds it, else the built-in.
---@param stored string|nil
---@param available string[]
---@return string
function wallpaper.effect_in(stored, available)
    for _, name in ipairs(available) do
        if name == stored then
            return name
        end
    end
    return wallpaper.NO_SHADER
end

---Every effect the folder offers right now.
function wallpaper.effects()
    return oblisk.files:map(wallpaper.effects_in)
end

---The chosen effect, as a name.
function wallpaper.effect()
    return computed({ store.wallpaper_transition, oblisk.files }, function(stored, f)
        return wallpaper.effect_in(stored, wallpaper.effects_in(f))
    end)
end

---`WallpaperService.setWallpaperTransition`.
---@param name string
function wallpaper.set_effect(name)
    if type(name) == "string" and name ~= "" and store.wallpaper_transition:get() ~= name then
        store:set("wallpaper_transition", name)
    end
end

---The `transition` table for the wallpaper `image`, as a signal.
---
---Depends on the stored wallpapers as well as the effect, so the parameters are drawn again on
---every wallpaper change the way `randomize` is called per change. A run already under way keeps
---the parameters it started with, because the engine copies the spec when it starts.
function wallpaper.transition()
    return computed({ wallpaper.effect(), store.wallpapers }, function(effect, _w)
        if effect == wallpaper.NO_SHADER then
            return { duration = wallpaper.TRANSITION_MS, easing = wallpaper.TRANSITION_EASING }
        end
        local params = RANDOM_PARAMS[effect]
        return {
            duration = wallpaper.TRANSITION_MS,
            easing = wallpaper.TRANSITION_EASING,
            shader = wallpaper.SHADER_FOLDER .. "/" .. effect .. ".frag",
            params = params and params() or nil,
        }
    end)
end

-- `Settings.defaultWallpaper`: file shipped beside `shell.lua` (ADR-0055 decision 5).
wallpaper.DEFAULT = oblisk.config_dir .. "/wallpaper.svg"

local function is_fit(value)
    for _, fit in ipairs(wallpaper.FITS) do
        if fit.value == value then
            return true
        end
    end
    return false
end

---Path for `output` from one stored `wallpapers` table. Pure for one `computed` over every screen.
---@param w table|nil The `wallpapers` table, or `nil` before the first push.
---@param output string
---@return string
function wallpaper.path_in(w, output)
    local stored = w and w[output] and w[output].path
    if type(stored) == "string" and stored ~= "" then
        return stored
    end
    return wallpaper.DEFAULT
end

---@param w table|nil
---@param output string
---@return string
function wallpaper.fit_in(w, output)
    local stored = w and w[output] and w[output].fit
    if type(stored) == "string" and is_fit(stored) then
        return stored
    end
    return wallpaper.DEFAULT_FIT
end

---Stored table for callers building a `computed` over several outputs.
function wallpaper.all()
    return store.wallpapers
end

---`path_in` as a signal for one panel output's `image.source`.
---@param output string
function wallpaper.path_of(output)
    return store.wallpapers:map(function(w)
        return wallpaper.path_in(w, output)
    end)
end

---@param output string
function wallpaper.fit_of(output)
    return store.wallpapers:map(function(w)
        return wallpaper.fit_in(w, output)
    end)
end

---Merge `changes` into one output and store a copied whole table. Mutating the signal's last-pushed
---table would change `computed` input without marking the scene dirty.
---@param output string
---@param changes table
local function write(output, changes)
    local stored = store.wallpapers:get() or {}
    local merged = {}
    for name, entry in pairs(stored) do
        merged[name] = entry
    end
    local entry = {}
    for key, value in pairs(merged[output] or {}) do
        entry[key] = value
    end
    for key, value in pairs(changes) do
        entry[key] = value
    end
    merged[output] = entry
    store:set("wallpapers", merged)
end

---`WallpaperService.setWallpaper`; skip unchanged writes because every write pushes.
---@param output string
---@param path string
function wallpaper.set(output, path)
    if path == "" or wallpaper.path_in(store.wallpapers:get(), output) == path then
        return
    end
    write(output, { path = path })
end

---`WallpaperService.setModePref`.
---@param output string
---@param fit string
function wallpaper.set_fit(output, fit)
    if not is_fit(fit) or wallpaper.fit_in(store.wallpapers:get(), output) == fit then
        return
    end
    write(output, { fit = fit })
end

---Watched folder from the last `oblisk.files` push, or `nil` before the first.
---@param f FilesState|nil
---@return Folder|nil
function wallpaper.folder_in(f)
    return f and f.folders and f.folders[wallpaper.FOLDER] or nil
end

---Connector names of every screen, `WallpaperService.monitors`.
---@return string[]
function wallpaper.outputs()
    local names = {}
    for _, screen in ipairs(oblisk.screens:get() or {}) do
        if screen.name and screen.name ~= "" then
            names[#names + 1] = screen.name
        end
    end
    return names
end

---`WallpaperService.randomizeAllMonitors`: one folder draw per screen. No-op before the listing
---lands or when it is empty.
function wallpaper.randomize_all()
    local folder = wallpaper.folder_in(oblisk.files:get())
    local entries = folder and folder.entries or {}
    if #entries == 0 then
        return
    end
    for _, output in ipairs(wallpaper.outputs()) do
        wallpaper.set(output, entries[math.random(#entries)].path)
    end
end

oblisk.files:invoke("watch", wallpaper.FOLDER, wallpaper.EXTENSIONS)
-- The same call for the effects, for the same reason: the picker needs the list before it opens.
oblisk.files:invoke("watch", wallpaper.SHADER_FOLDER, wallpaper.SHADER_EXTENSIONS)

return wallpaper
