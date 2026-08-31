-- Mirrors SysTray.qml.
--
-- The one `list` in this file, and the only node kind whose children do not exist as a literal Lua
-- table: they are generated one per `source` element (ADR-0045 decision 3). A tray is exactly that
-- shape, so this is where it belongs rather than in a synthetic fixture.
--
-- `key` is what makes reconciliation stable across pushes: without it a tray item appearing at the
-- front would renumber every sibling and reconcile each one against the wrong previous node.
-- § 2.5 populates exactly one of `icon_name` and `icon_path` per item and never both, which is
-- why one `icon` node handles both: docs/adr/0054 decision 2 makes an absolute `name` its own path,
-- so the `or` below is the whole branch. Falling back to the app's name keeps an item visible when
-- a theme has nothing under the name it reported, rather than leaving a 16px hole.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

-- No count badge. It sat beside the icons showing how many of them there were, which is a number
-- the icons already are: anyone who can see two icons does not need a "2" next to them, and on a
-- bar where every zone is fighting for width it was the one module paying for a readout that says
-- nothing the thing beside it does not.

return pill({
    list {
        -- The tray is the reason `direction` exists on `list` (§ 5.2 item 7). A `list` laid out
        -- exactly like a `column` until now, so a tray of three icons stacked downwards inside a
        -- 34px bar and all but the first were clipped away -- the no-horizontal-list ponytail
        -- `layout::scene` used to carry, met by the module it was written about.
        direction = "Horizontal",
        spacing = 6,
        align_v = "Center",
        -- `computed` over both, where `oblisk.tray:map` alone would do: `itemfn` reads
        -- `oblisk.applications` for its icon fallback below, and a `list` only rebuilds its items
        -- when its own `source` changes. Depending on tray alone would leave an item that
        -- registered before the first applications scan landed showing its name as text forever,
        -- because nothing would ask `itemfn` to run again once the entry it needed existed.
        source = computed({ oblisk.tray, oblisk.applications }, function(t)
            return (t and t.items) or {}
        end),
        itemfn = function(item)
            -- The item's own icon first, then its `.desktop` entry's, then its name as text. The
            -- middle step is the third `oblisk.applications` consumer (docs/adr/0061): an item
            -- that registers with neither an `IconName` nor an `IconPixmap` used to fall straight
            -- through to a truncated string, and its `Id` is usually the application's own name,
            -- which `util.app_entry` can match against a desktop entry.
            local entry = util.app_entry(oblisk.applications:get(), item.name or item.id)
            local art = item.icon_name or item.icon_path or (entry and entry.icon)
            if art then
                return icon { name = art, size = 16 }
            end
            return cell(util.truncate(item.name or item.id or "?", 10), theme.DIM, 11)
        end,
        key = function(item)
            return tostring(item.id)
        end,
    },
})
