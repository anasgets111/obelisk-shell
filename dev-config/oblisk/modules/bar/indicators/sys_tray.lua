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

return pill({ list {
    source = oblisk.tray:map(function(t)
        return (t and t.items) or {}
    end),
    itemfn = function(item)
        local art = item.icon_name or item.icon_path
        if art then
            return icon { name = art, size = 16 }
        end
        return cell(util.truncate(item.name or item.id or "?", 10), theme.DIM, 11)
    end,
    key = function(item)
        return tostring(item.id)
    end,
} })
