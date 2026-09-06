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
    { value = "cover", label = "Fill" },
    { value = "contain", label = "Fit" },
    { value = "stretch", label = "Stretch" },
}
wallpaper.DEFAULT_FIT = "cover"
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
