# The icon theme resolver lives in the Renderer, and `image` is the node that draws a file

Phase 19 deferred the `icon` node on a spec conflict and nothing has settled it since. Today
`icon` is a registered node kind that parses its `size`, reserves that much layout space, and
paints nothing: `layout/paint.rs`'s arm for it is `"icon" => {}`. There is no `image` node kind at
all. Nothing in this codebase draws a single pixel from a file.

Meanwhile three capabilities already produce file paths. `dbus/shm_icons.rs` PNG-encodes and spools
to `/dev/shm/oblisk-$UID/`; tray decodes `IconPixmap` through it (ADR-0031), notifications resolve
the whole `Notify` icon precedence through it, and `mpris` reports an `album_art_path`. Three
producers, no consumer. That is why the tray on the dev config reads `TelegramDe...` as text.

## The conflict, and which half of it is real

`oblisk-idl-api-specs.md` § 5.2 item 5 gives `icon` a `name` holding a theme name
(`"audio-volume-high"`), implying the Renderer resolves it. `oblisk-supervisor-services-dbus.md`
§ 9.2 puts an off-thread XDG desktop and icon-theme lookup engine in the Supervisor, with an LRU
cache, exposed as `system:find_icon(app_id, name, fallback)`. Two resolvers, neither built.
`build-steps.md` recommended settling toward the Supervisor's.

That recommendation is reversed here, and the reason is in the IDL rather than in either of those
paragraphs. § 3.2's own row for `system:find_icon` reads "*Synchronous internal Rust lookup*
returning `string` path", and it is the only row in that table with no JSON payload beside it,
because every other row is a one-way command. The control socket carries one-way commands one way
and one-way `StateSnapshot`s the other. It has no request/response shape, no correlation id and no
reply. Putting the resolver in the Supervisor means either inventing one, or making `icon.name`
resolve a frame or more late through a snapshot.

Both are worse than the thing they avoid. A synchronous round trip on the socket blocks the render
thread, which since ADR-0039 is also the Wayland dispatch thread and the thread the config VM runs
on, and ADR-0048 removed blocking calls from that VM on exactly this reasoning. A late-resolving
`icon.name` makes every icon in a `list` a two-pass thing and puts an IPC message per distinct icon
name on a socket sized for capability pushes.

§ 9.2's "off-thread" is a performance requirement, not a placement one, and it can be met on the
Renderer's side of the boundary the day a cache miss actually costs a frame.

## Decision 1: the Renderer resolves theme names, through `freedesktop-icons`

One crate (`freedesktop-icons` 0.4, ashell's choice and the one `build-steps.md` already names),
called synchronously with an in-process cache. This makes § 3.2's signature true as written instead
of true only after a protocol is invented for it.

## Decision 2: a `name` that is an absolute path is used as that path

`icon { name = "/dev/shm/oblisk-1000/tray/telegram.png" }` draws that file. `icon { name =
"audio-volume-high" }` goes through the theme lookup. This is not a special case invented here: the
`Icon=` key in every `.desktop` file on the system has worked this way since the desktop entry spec
was written, so a config author already knows the rule and no third property is needed to express
it.

What it buys is that § 2.5's tray, which populates exactly one of `icon_name` and `icon_path` and
never both, collapses to `icon { name = item.icon_name or item.icon_path }` rather than a branch
between two node kinds inside a `list`.

## Decision 3: `image` is a second node kind, and `icon` is that node plus the resolver

`icon` is square (§ 5.2 item 5's `size` is "bounding box diameter") and takes a theme name. Album
art is neither: it has an aspect ratio and it arrives as a path. A wallpaper (ADR-0055) is neither
twice over. So `image` takes `source` (a path), the base § 5.1 `width`/`height`, and a `fit`.

They are not the same node with the resolver bolted on optionally, and they are not two copies of
one another: `icon` is `image` with the resolver in front and the size collapsed to one number.
Adding `image` is a departure from § 5.2, which lists eight node kinds and no image among them.
Recorded as such rather than filed under `icon`, because calling a 3840x2160 wallpaper an icon to
stay inside the letter of a list would be the worse lie.

## Decision 4: SVG rasterizes through `resvg`, raster decodes through what femtovg already has

femtovg's default `image-loading` feature already pulls `image` 0.25 and re-exports it, so PNG and
JPEG cost nothing new. SVG costs `resvg`, and it is not optional: Adwaita ships its icons as
scalable SVG, so an SVG-blind resolver returns broken icons on a stock GNOME install. `resvg` is
cheaper here than its dependency list suggests, since `fontdb` and `rustybuzz` are already in the
tree behind `cosmic-text`.

The cache is keyed on the resolved path together with the integer pixel size, not on the path
alone. An SVG rasterized for a 12px box is a different texture from the same file rasterized for a
24px box, and keying on the path alone would serve the first one blurry to the second forever.

## Decision 5: `system:find_icon`'s `app_id` half is not built

§ 9.2 describes two lookups behind one function: a theme-name lookup, and an `app_id` to
`.desktop`-file to `Icon=` lookup. Only the first has a caller. `freedesktop-icons` does the first
and does not do the second, which would be a `/usr/share/applications` scan of its own.

Not built, and no Lua-facing `find_icon` is exposed either, because with `icon.name` resolving
theme names a config has nothing left to ask the function for. It goes in the day something needs
an icon for a window that is not already telling us its icon.

## What this costs, named rather than absorbed

**A cache miss does disk I/O and SVG rasterization on the render thread.** First paint of an icon
walks the theme index, reads the file and, for an SVG, rasterizes it, all inside the frame. Steady
state is a hash lookup. The upgrade path is a worker thread that resolves and rasterizes, with the
scene's dirty flag (ADR-0044) as the wake-up, which is the same shape `text::shaping` already has
and is exactly § 9.2's "off-thread" met on this side of the boundary. Carrying a `ponytail:`
comment, not built now.

**The cache dies with the generation.** The Renderer is swapped as an OS process on every reload
(the whole reason the process boundary exists), so every icon is resolved and re-uploaded from
cold after each config edit. That is correct and it is also the cost: an in-Supervisor cache would
have survived. Worth naming because it is the one genuine point in favour of § 9.2's placement.

**The cache is bounded by count, not by bytes.** A hard cap with oldest-out eviction, which is
FIFO rather than the LRU § 9.2 asks for. A bar's icon set is small and static; a long-lived
notification history with distinct album art is what would push against the cap. LRU is the
upgrade, and the reason it is not the starting point is that FIFO is ten lines and needs no
dependency. Byte-accounted eviction is what ADR-0043's memory budget will eventually want, and
neither is worth building before an image cache exists to measure.

## Amendment: the cache key carries the file's revision, and ADR-0031 predicted why

Decision 4 said the key is the resolved path together with the pixel size. That is wrong for a
raster file, and the review that found it traced the exact path: `dbus/shm_icons.rs::write_png`
overwrites `/dev/shm/oblisk-$UID/tray/{name}.png` in place on every `NewIcon`, same filename, new
bytes. With a path-only key an app that changes its tray icon (a badge appearing, a mute toggling, a
connection state) keeps the pixels it had at first paint for the life of the Renderer process. No
error, no log line, just an icon that stops telling the truth.

ADR-0031 chose that spooling behaviour deliberately and deferred the consumer-side fix as
Speculative Generality, on the explicit grounds that "the renderer has no scene graph or
icon-loading path at all as of this ADR", with the upgrade path named as "Renderer-side texture
cache-busting, only once the renderer's actual icon-loading mechanism exists and is shown to need
it". This is that mechanism, and this is it being shown to need it. The deferral was correct and it
came due on the first commit that could have exercised it.

The key gains the file's modification time and length. Both rather than either: tmpfs carries
nanosecond timestamps so mtime alone suffices in practice, and length is free and covers a
filesystem that rounds. Not a content hash, which would mean reading the file to decide whether to
read the file.

The cost is one `stat` per image node per frame, on a hit as well as a miss, because there is no
other way to ask whether the bytes changed. On a bar with ten icons at 60Hz that is six hundred
stats a second against tmpfs, and it is still cheaper than the thing it replaces.

It also fixes something that was not the reason for it. A missing file took the negative cache and
would have stayed there forever; its version now changes the moment it appears, so the lookup is
new and the file is picked up. A wallpaper written after the config referring to it is the case
that needs this.

## Amendment: eviction cannot free a texture during the frame that evicted it

femtovg batches a frame's draw calls and resolves an `ImageId` to a texture at `flush`, not at
`fill_path`. Deleting on eviction therefore unbinds a texture an already-recorded command still
names, and femtovg answers a missing id with default paint parameters rather than an error, so the
symptom is one silently blank image per eviction in any frame that drew more than `CACHE_CAPACITY`
distinct images. Never a crash, never a log line.

Eviction now queues the id and `paint_tree` frees the queue before it walks anything, which is the
one point in the cycle where the previous flush has happened and the current frame has recorded
nothing. That also took the canvas out of `ImageCache::insert`, which is what made the capacity
bound testable without a GL context.
