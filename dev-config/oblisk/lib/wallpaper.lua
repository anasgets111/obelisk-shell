-- What `Services/Core/WallpaperService.qml` is: which file each output shows, how it fits, where
-- the files come from, and the two writes that change them. No node here; the panel that draws the
-- file is `modules/global/wallpaper.lua`, the picker is `modules/global/wallpaper_picker.lua`, the
-- bar button is `modules/bar/indicators/wallpaper_button.lua`.
--
-- ## Where the choice lives
--
-- In `state.json`, through `system:write_state`, one key per output: `wallpaper.<output>` is the
-- path and `wallpaper_fit.<output>` the fit. Scalars, because that is what `write_state` takes
-- (§ 3.2), and per output rather than one table because a table is not a scalar. So the choice
-- survives a reload and a reboot, which ADR-0055 decision 2 called the open hole: a `state()`
-- signal held it before, and a reload forgot it.
--
-- ## Where the files come from
--
-- `oblisk.files` (ADR-0120) follows `FOLDER` and hands back its image files, kept current through
-- inotify, so a file dropped into the folder is in the picker before it is opened. Watched from
-- here, at evaluation, because the folder is a setting and not a state: the picker reads the list
-- when it opens and the bar's right-click needs it without the picker ever having opened.
local wallpaper = {}

-- `Settings.data.wallpaperFolder`'s default, verbatim.
wallpaper.FOLDER = "/mnt/Work/1Wallpapers/Main"
-- `FolderListModel.nameFilters`. `gif` is out: the decoder takes none of it, and a folder often has one.
wallpaper.EXTENSIONS = { "jpg", "jpeg", "png", "webp" }
-- `WallpaperService.availableModes` reduced to what `image.fit` has (ADR-0055 decision 3): no
-- `center` and no `tile`, since the engine draws neither.
wallpaper.FITS = {
    { value = "cover", label = "Fill" },
    { value = "contain", label = "Fit" },
    { value = "stretch", label = "Stretch" },
}
wallpaper.DEFAULT_FIT = "cover"
-- `Settings.defaultWallpaper`: the file shipped beside `shell.lua` (ADR-0055 decision 5).
wallpaper.DEFAULT = oblisk.config_dir .. "/wallpaper.svg"

local function path_key(output)
    return "wallpaper." .. output
end

local function fit_key(output)
    return "wallpaper_fit." .. output
end

local function is_fit(value)
    for _, fit in ipairs(wallpaper.FITS) do
        if fit.value == value then
            return true
        end
    end
    return false
end

---The file `output` shows, read off one `oblisk.system` payload. Pure, so the picker can ask the
---same question of every screen inside one `computed`.
---@param s SystemState|nil
---@param output string
---@return string
function wallpaper.path_in(s, output)
    local stored = s and s.state and s.state[path_key(output)]
    if type(stored) == "string" and stored ~= "" then
        return stored
    end
    return wallpaper.DEFAULT
end

---@param s SystemState|nil
---@param output string
---@return string
function wallpaper.fit_in(s, output)
    local stored = s and s.state and s.state[fit_key(output)]
    if type(stored) == "string" and is_fit(stored) then
        return stored
    end
    return wallpaper.DEFAULT_FIT
end

---A signal of `path_in` for one output, for the panel's `image.source`.
---@param output string
function wallpaper.path_of(output)
    return oblisk.system:map(function(s)
        return wallpaper.path_in(s, output)
    end)
end

---@param output string
function wallpaper.fit_of(output)
    return oblisk.system:map(function(s)
        return wallpaper.fit_in(s, output)
    end)
end

---`WallpaperService.setWallpaper`: a write only when it changes something, since every
---`write_state` rewrites the file and pushes.
---@param output string
---@param path string
function wallpaper.set(output, path)
    if path == "" or wallpaper.path_in(oblisk.system:get(), output) == path then
        return
    end
    oblisk.system:invoke("write_state", path_key(output), path)
end

---`WallpaperService.setModePref`.
---@param output string
---@param fit string
function wallpaper.set_fit(output, fit)
    if not is_fit(fit) or wallpaper.fit_in(oblisk.system:get(), output) == fit then
        return
    end
    oblisk.system:invoke("write_state", fit_key(output), fit)
end

---The watched folder as `oblisk.files` last pushed it, or `nil` before the first push.
---@param f FilesState|nil
---@return Folder|nil
function wallpaper.folder_in(f)
    return f and f.folders and f.folders[wallpaper.FOLDER] or nil
end

---The connector names of every screen, `WallpaperService.monitors`.
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

---`WallpaperService.randomizeAllMonitors`: a different draw per screen, from the folder as it
---stands. Nothing happens before the listing lands or when it is empty.
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
