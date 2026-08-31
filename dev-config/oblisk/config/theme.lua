-- Catppuccin Mocha, because a bar needs a palette and an invented one would just be a worse
-- version of a palette someone already balanced.
--
-- Its own file because every module in `shell.lua` reads these ten values and none of them owns
-- them, which is the first thing a config of any size wants to move out. Editing this file
-- recolours the bar in place: ADR-0047 points `require` at this directory and drops the module
-- cache before each re-evaluation, so a required file is re-read rather than served stale.
--
-- Neither half of that worked before Phase 26, and the failure was quiet both times. `package.path`
-- was Lua's compiled-in default, so a `require` searched `/usr/local/share/lua/5.4/` and then the
-- process's working directory, which nothing sets. An edit to a file that did resolve then reached
-- a cached copy and changed nothing on screen.
return {
    BG      = "#1e1e2eff",
    SURFACE = "#313244ff",
    -- Catppuccin surface1, one step up from SURFACE. The hover shade: a module that highlights
    -- under the pointer reads this rather than inventing its own lighter blue (docs/adr/0062).
    HOVER   = "#45475aff",
    FG      = "#cdd6f4ff",
    DIM     = "#6c7086ff",
    ACCENT  = "#89b4faff",
    GREEN   = "#a6e3a1ff",
    YELLOW  = "#f9e2afff",
    PEACH   = "#fab387ff",
    RED     = "#f38ba8ff",
    MAUVE   = "#cba6f7ff",
}
