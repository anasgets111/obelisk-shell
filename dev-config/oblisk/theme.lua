-- Catppuccin Mocha, because a bar needs a palette and an invented one would just be a worse
-- version of a palette someone already balanced.
--
-- Its own file because every module in `shell.lua` reads these ten values and none of them owns
-- them, which is the first thing a config of any size wants to move out. Editing this file
-- recolours the bar in place: ADR-0047 points `require` at this directory and drops the module
-- cache before each re-evaluation, so a required file is re-read rather than served stale.
return {
    BG      = "#1e1e2eff",
    SURFACE = "#313244ff",
    FG      = "#cdd6f4ff",
    DIM     = "#6c7086ff",
    ACCENT  = "#89b4faff",
    GREEN   = "#a6e3a1ff",
    YELLOW  = "#f9e2afff",
    PEACH   = "#fab387ff",
    RED     = "#f38ba8ff",
    MAUVE   = "#cba6f7ff",
}
