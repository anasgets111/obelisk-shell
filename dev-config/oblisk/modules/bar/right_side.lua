-- Mirrors RightSide.qml, including the order: the status indicators, then the tray, then the clock
-- last against the edge.
--
-- There is no brightness module, which is also true of the config this mirrors. It was on the bar
-- until the zone ran out of room for it; the level and its two controls live in
-- `modules/bar/panels/power_menu.lua` now, where a panel has the width for a labelled control and
-- the bar does not.
--
-- `privacy` is on the left instead, which RightSide.qml would not do. The tray is why: it is the
-- one module here with no width of its own, and once it laid out horizontally six registered items
-- put this zone 61px past its edge. Privacy is the module that moves most cheaply, because it is an
-- alert rather than a readout -- it belongs beside `rescue`, which is the same shape.
local volume_module = require("modules.bar.indicators.volume")
local network_module = require("modules.bar.indicators.network")
local bluetooth_module = require("modules.bar.indicators.bluetooth")
local tray_module = require("modules.bar.indicators.sys_tray")
local date_time = require("modules.bar.indicators.date_time")
local pill = require("components.pill")

local clock_pill = pill({ date_time.date, date_time.clock })
clock_pill.hover = hover(date_time.slot)

return row {
    width = "41%",
    height = "Fill",
    align_h = "End",
    align_v = "Center",
    spacing = 6,
    children = {
        volume_module,
        network_module,
        bluetooth_module,
        tray_module,
        -- The clock pill declares the hover region its tooltip reads, so the whole pill is the
        -- trigger rather than either cell inside it.
        clock_pill,
    },
}
