-- Phase 21 item 1's live proof: a `button` whose `on_click` changes what a `text` paints. The
-- counter is a `state` signal (ADR-0044 decision 5), so the handler's `:set()` is what marks the
-- scene dirty. The name is what survives an in-place reload: edit a colour in `config/theme.lua`
-- while this runs and the count keeps going instead of resetting, because `state("clicks", 0)`
-- finds the signal it built last time and ignores the new initial.
local clicks = state("clicks", 0)

-- The anchor rect the `popup` hangs from. ADR-0049's amendment settles where it comes from: not off
-- the input-dispatch stack, but through the config, because `on_click` receives the button's own
-- rect (ADR-0050 decision 3) and writes it to a named `state` signal the popup reads back. The
-- initial is the button's declared size, because `anchor_rect` must be non-zero before anything has
-- ever been clicked or the whole evaluation fails (§ 6.3).
local popup_anchor = state("popup_anchor", { x = 0, y = 0, width = 90, height = 24 })
local settings_open = state("settings_open", false)

-- Not toggled the way `settings_open` is, and the reason is measured rather than assumed. Under an
-- `xdg_popup` grab, niri still delivers a click on this button to us, because the bar is the
-- popup's own parent surface and so inside the grab's tree, so a toggle would close it. What a
-- toggle would also do is fight `on_dismiss` on every click landing elsewhere, since that path
-- already writes false. One writer per edge.
local menu_open = state("menu_open", false)

return {
    clicks = clicks,
    popup_anchor = popup_anchor,
    settings_open = settings_open,
    menu_open = menu_open,
}
