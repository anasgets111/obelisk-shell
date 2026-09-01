# A tray item is addressed by the name it registered

Two tray items on the same session drew wrong, for two unrelated reasons. Both are in this ADR
because both were found by the same screenshot and neither is worth its own file.

## Decision 1: the registered name is the destination, the owner is the identity

`resolve_registration` turned a well-known `service` argument into its owner with `GetNameOwner`
and addressed every later message there. That is the textbook resolution, and Slack's item was
invisible because of it. Its Chromium D-Bus code dispatches property reads on the message's
destination field:

| addressed to | `GetAll` | `Get Id` |
| --- | --- | --- |
| `org.freedesktop.StatusNotifierItem-1240273-1` | 14 properties | `"Slack_status_icon_1"` |
| `:1.659`, which owns that name | error | error |

Three attempts each way, same result. So Oblisk read no `IconName` and no `IconPixmap`,
`resolve_icon_source` answered `IconSource::None`, and the item had nothing to draw. The data was
there the whole time behind the other name, including a valid 22x22 pixmap.

`resolve_registration` returns a `ResolvedRegistration` now, carrying both names because they
answer different questions. `unique_name` is the identity: the registry key, what
`NameOwnerChanged` reports on, what the spool filename is built from. `destination` is the address
every `Get` and `GetLayout` carries, and for a well-known registration it stays the well-known
name.

The `GetNameOwner` call stays. Only the owner can answer the identity half, and dropping the
lookup would leave an item nothing can ever clean up.

This is worse D-Bus and better behaviour. A well-known name can in principle move to another owner
between the lookup and a later read, and then Oblisk talks to the new owner under the old identity.
Reaching that needs a tray app to drop its name and a second one to claim the same
`org.freedesktop.StatusNotifierItem-PID-N` while the first entry is still live, where the PID is
the first app's. Against an item that is simply never readable, this is the better failure.

## Decision 2: `foreground` on an `icon` is the value `currentColor` resolves to

Telegram publishes `org.telegram.desktop-mute-symbolic`. Tela-circle-dracula ships that artwork as
`telegram-mute-panel`, so the lookup walks `Inherits=hicolor,Adwaita,breeze` and lands on Breeze's,
which is a KDE colour-scheme file:

```xml
<style id="current-color-scheme">.ColorScheme-Text { color:#232629; }</style>
<path class="ColorScheme-Text" style="fill:currentColor" .../>
```

`#232629` is Breeze *Light*'s text colour, baked in on the assumption that the toolkit rewrites the
block at load time. Plasma does. Qt does, which is why the Quickshell bar this config mirrors draws
a white paper plane from this same file. `usvg::Options::default()` does not, so Oblisk rasterized
it as shipped: mean opaque RGB (17, 19, 20) at 18px, on a dark glass bar.

`icon` takes a `foreground` now, meaning what CSS `color` means. `rasterize_svg` rewrites the file
before `usvg` sees it, in two places: every bare `color:` declaration is repointed, and the root
`<svg>` gets a `color` attribute for files that use `currentColor` and define it nowhere. A file
holding no `currentColor` is handed back byte for byte, so a full-colour app icon is untouched and
a config can pass `foreground` to every tray item without flattening one.

The rewrite is textual. `usvg` resolves `currentColor` while building the tree and exposes no hook
before that.

`CacheKey` gains the colour. Without it the first tint drawn wins for the life of the process, and
the same icon white on the bar and dim in a popup would be one texture.

### Why not prefer the pixmap

Telegram also ships a 16x16 `IconPixmap`, so preferring it would have hidden this. ADR-0031 prefers
`IconName` and stays: upscaling a 16px bitmap to 18 to avoid recolouring a vector is the wrong
trade, and the next symbolic icon with no pixmap would draw black anyway.

## What holds this

| | |
| --- | --- |
| `well_known_registration` | asserts the identity is the owner and the address is the registered name |
| `tinted_svg` | a Breeze-shaped stylesheet, a bare `currentColor`, and a full-colour file returned unchanged |
| `rewrite_color_declarations` | `stop-color:` and `flood-color:` survive a rewrite of the `color:` between them |
| `CacheKey` | one file at two tints is two slots |

`ponytail:` `color:` is matched textually, so one inside a comment or an attribute value would be
rewritten too. No theme file this was tested against has one. The upgrade path is a real CSS pass
over the `<style>` body, which means a CSS parser this crate does not otherwise want.
