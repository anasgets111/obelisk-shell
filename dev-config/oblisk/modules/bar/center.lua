local date_time = require("modules.bar.indicators.date_time")
local clock, date = date_time.clock, date_time.date

return row {
    width = "20%",
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = 8,
    children = { date, clock },
}
