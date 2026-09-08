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
wallpaper.TRANSITIONS = { "fade", "wipe", "disc", "portal", "stripes", "pixelate" }
-- `WallpaperService.qml`'s duration and curve.
wallpaper.TRANSITION_MS = 1500
wallpaper.TRANSITION_EASING = "InOutCubic"

---`AnimatedWallpaper.qml`'s `transitionParams.randomize`: a wipe picks a side, a disc and a portal
---pick a centre, stripes pick a count and an angle. Called per change, so no two are identical.
---@param effect string One of `TRANSITIONS`.
---@return table|nil params, string|nil shader
local function shader_for(effect)
    if effect == "wipe" then
        return { direction = math.floor(math.random() * 4), softness = 0.1 }, "wipe"
    elseif effect == "disc" or effect == "portal" then
        return { center_x = math.random(), center_y = math.random(), softness = 0.1 }, effect
    elseif effect == "stripes" then
        return { count = math.random(4, 24), angle = math.random() * 360, softness = 0.1 }, "stripes"
    elseif effect == "pixelate" then
        return { softness = 0.35 }, "pixelate"
    end
    -- `fade`, and anything unrecognised: the engine's built-in cross-dissolve, which needs neither.
    return nil, nil
end

---The `transition` table for one wallpaper `image`, or `nil` for no transition at all.
---@param effect string|nil
function wallpaper.transition_for(effect)
    if effect == "none" then
        return nil
    end
    local params, shader = shader_for(effect or "fade")
    return {
        duration = wallpaper.TRANSITION_MS,
        easing = wallpaper.TRANSITION_EASING,
        shader = shader and (oblisk.config_dir .. "/shaders/" .. shader .. ".frag") or nil,
        params = params,
    }
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

return wallpaper
