# Decisions

One entry per decision, numbered in the order they were taken. Numbers are permanent: 1,233
comments across the Rust tree cite them as `ADR-0044`, and 406 of those cite a specific
`decision N` inside an entry, so neither the entry numbers nor the decision numbers inside them
may be reused or renumbered.

These were 81 separate files running 8,635 lines, median 99 lines for one decision. The format
they were written to (`.agents/skills/domain-modeling/ADR-FORMAT.md`) says an ADR can be a single
paragraph and that the value is in recording *that* a decision was made and *why*. They had grown
into essays, so they were compressed back to the decision. Git holds the long versions.

An entry records what a reader cannot get from the code: the alternative that was rejected, the
constraint that is not visible at the call site, and the measured number that cost real work to
produce. It does not restate what the code does.

To add one, take the next number and write a paragraph. Do not edit an existing entry to match
what shipped later. Add a line saying which entry superseded it, and leave the record alone.

The early entries name numbered build phases. Those came from a roadmap that no longer exists,
because every phase in it was built. Read a phase number as a date, not as a pointer.
`docs/roadmap.md` holds what is still open.

## 0001. Reload strategy splits on topology change

Config edits split into two reload paths instead of always doing a full generation swap. A value
change (anything not touching a top-level `surface` node's set, layer, anchor, or monitor) does an
in-place reload: a Lua VM reset and rerun inside the current generation, no new process, no
generation ID bump. A topology change still goes through the full generation swap (candidate spawn,
presentation evidence, promote, reap).

Rejected: a single generation-swap path for everything, for uniformity. It makes every
keystroke-to-save cycle pay Wayland rebinding and a multi-output presentation-feedback round trip,
the exact stutter the swap protocol exists to avoid.

Rejected: dropping the process boundary and reconciling one long-lived Wayland connection against a
rebuilt scene tree, matching Quickshell's per-reload `QQmlEngine` with a stable-id-matched
`Reloadable` protocol that lets components reuse their native handle across the rebuild. Quickshell
can do this safely because QML's JS layer runs under a tracing GC, so the old engine's object graph
is just abandoned and reclaimed. Rust has no GC: reusing a native surface or texture handle while
reconciling old and new scene trees means unsafe code, `Rc<RefCell<>>` runtime panics, or manual
arena and generation tagging. The process boundary buys memory safety instead, since killing a
process is a kernel-guaranteed single free. In-place reload stays safe without a GC for a different
reason: it only ever drops the whole `mlua::Lua` VM atomically via RAII, never reconciling a live
native object graph. State the Supervisor holds on a generation's behalf, like idle-threshold
registrations, cannot rely on that drop alone (ADR-0006).

Superseded in part by ADR-0044: the VM is not reset.

## 0002. Wallpaper surface skips transitions on reload and queues overlapping sets

The wallpaper surface (ADR-0007) paints its current texture directly on candidate first-frame and on
in-place reload, with no shader transition. Transitions run only in response to a live
`wallpaper:set()` call. If called again while a transition is in flight, the new target queues and
replays once the current transition finishes, rather than interrupting the running shader or
stomping the in-flight texture. The second texture buffer used for a transition is allocated only
for the transition's duration and released right after.

Adopted directly from a working implementation (Quickshell `AnimatedWallpaper.qml`) rather than
re-derived: `Component.onCompleted` sets the image source with no shader involved, the transition
`Loader` is active only while a transition runs, and overlapping `changeWallpaper()` calls queue via
`pendingUrl` instead of racing.

Amendment (ADR-0055): wallpaper is now an `image` node on a config-declared `Background` panel, and
it paints a new texture directly with no transition. That is this ADR's first-frame and reload
branch, built as written. The transition branch is unbuilt and stays specified: there is no
animation model in the engine at all, so there is no shader for the queue to arbitrate between and
no second buffer to release. `wallpaper:set()` as a capability call is retracted by ADR-0055
decision 1; every mention of it above means the config writing to the `state()` signal bound to
`image.source`.

## 0003. Authority transfers per output, not per process

`oblisk-supervisor-services-dbus.md` §15.4 framed promotion as one atomic event gated on
presentation evidence from every connected display. Authority instead transfers per (generation,
output): each output moves to the candidate independently, the moment its own presentation evidence
lands, with no barrier on its siblings. The Supervisor reaps a generation once it owns zero outputs,
which can happen immediately or minutes later if one output stays asleep.

A single sleeping (DPMS-off) or slow-to-wake output under whole-process authority would stall
promotion everywhere, or force an arbitrary global deadline to choose between leaking the old
generation forever or blind-promoting the candidate onto an output it never proved it could paint,
the black-frame failure PBA exists to prevent. Per-output authority also matches the existing
surface model: a `surface` with `monitor = "All"` already binds one distinct `wl_surface` per
output.

Rejected: whole-process authority with a deadline that force-promotes every output, including a
stalled one. Promoting onto an output that never proved it could paint reintroduces the black-frame
risk PBA eliminates. The two failure modes stay handled separately regardless: a dead candidate
process (SIGCHLD before any output promotes) aborts the whole candidate; a merely-slow output on a
live candidate just waits, indefinitely if needed, with the candidate's null-buffer already staged
there.

## 0004. Revision is tracked per capability, fed by inbound state pushes

The Renderer's IPC receive loop keeps one revision counter per capability, bumped on every inbound
`StateSnapshot` (`shared::StateSnapshot { revision, payload }`) regardless of whether any Lua code
ever reads the corresponding signal. A write envelope's `expected_revision` reads that counter
directly, matching the envelope's existing capability-scoped `expected_revision` field: one counter
per capability, not per signal, no new wire format needed.

Write calls take bare unwrapped values (`audio:set_volume(vol)`), not `Signal` handles, so
`expected_revision` cannot be read off the write's own arguments the way `generation_id` rides the
mlua wrapper.

Rejected: stamping a per-capability "last observed revision" table on every `Signal:get()` call
instead. A zero-argument write like `audio:toggle_mute()` has no `:get()` in its own callback to
stamp anything, leaving the table's freshness dependent on unrelated reads elsewhere in the frame.

Checked Quickshell and Noctalia for prior art: neither has anything resembling optimistic
concurrency on writes, because neither ever has more than one writer active on a capability at once.
This mechanism exists because Oblisk allows generation N and N+1 to both hold write paths open
during a swap.

## 0005. Secure textfield submit targets a capability action, never Lua

Typed characters in a `mask_character`-bearing `textfield` must never reach Lua VM space, but the
reference lock screen needs `on_submit = function(text) ... end` and Wi-Fi password entry needs the
same secret-input primitive. `textfield` gets a `secure_submit = { capability, action }` property.
When `mask_character` is set and `secure_submit` is present, `on_submit` fires with no argument; the
Renderer's IPC layer attaches the native input buffer to that specific capability/action envelope
server-side, never surfacing it as a Lua value. If `mask_character` is set with no `secure_submit`,
the value is simply unreadable from Lua: security by default rather than by author discipline. The
buffer is zeroed explicitly right after the IPC send completes, not solely via a `Drop` impl, since
`Drop` timing under a panic or early return isn't something to bet a plaintext password on (adopted
from Noctalia).

Checked both prior-art projects: Quickshell's PAM binding (`PamContext::respond(const QString&)`)
hands the typed password to QML as a plain string, no boundary at all. Noctalia sidesteps the
problem structurally, its lock screen is a fixed native widget with no scriptable callback, so the
password never has a scripting layer to leak into. That dodge does not transfer to Oblisk: Lua has
to author its own Wi-Fi password field, so the primitive cannot be made non-scriptable to solve
this.

Amendment (ADR-0009): the original decision only protected the Lua boundary; it never told the
compositor or IME the field is sensitive. A `mask_character`-bearing `textfield` now also sets
`purpose = password` and the sensitive-data hint on its underlying `zwp_text_input_v3` object
(`wp-text-input-v3`'s `content_type` purpose/hint fields), so a well-behaved IME skips logging,
autocorrect, and clipboard-history capture for it, in addition to the Lua-boundary protection above.

## 0006. In-place reload explicitly resets Supervisor-held registrations

In-place reload sends one explicit IPC message before rerunning the script: capability `"renderer"`,
action `"reset_registrations"`. The Supervisor drops every registration tied to that `generation_id`
before the fresh top-level run repopulates them, an unconditional clear then rebuild, no
dedup-by-key needed on either side.

In-place reload (ADR-0001) resets the Renderer's Lua VM and reruns `shell.lua`, which reissues every
`idle:register_threshold(...)` call. That registration state lives in the Supervisor, a separate
process, so nothing about the Renderer's VM reset is visible there: a second `register` call reads
as a new threshold, not a replacement, leaking duplicate `ext_idle_notification_v1` listeners on
every value-change reload.

Both prior-art projects solve the same-process version of this for free. Quickshell's `IdleMonitor`
extends `PostReloadHook`, so every reload constructs a fresh object and the old one's destructor
tears down its listener. Noctalia's `IdleManager::reload(config)` calls `clearBehaviors()`
unconditionally before rebuilding every behavior from the new config, no diffing. Neither trick
crosses a process boundary, because in both projects the registration and the reload live in the
same process.

## 0007. Wallpaper gets its own Background-layer surface

Two static surfaces originally covered `main_bar` (layer `Top`) and `overlay_canvas` (layer
`Overlay`), with no surface for wallpaper rendering. A third static surface, `wallpaper_layer`
(`Background` layer, non-exclusive, one per monitor), is added, owned separately from the two UI
surfaces. See the Wallpaper surface term in `CONTEXT.md` and ADR-0002 for its reload and transition
behavior.

Rejected: painting wallpaper inside `overlay_canvas`, since that surface already spans the whole
screen. `Overlay` is the topmost layer in wlr-layer-shell stacking, above every application window,
by protocol definition, not by z-order the shell controls. Wallpaper content drawn there would cover
the desktop instead of sitting behind it, and no input-region trick fixes that: the problem is paint
order, not click routing.

Amendment (ADR-0038): the paint-order reasoning stands unchanged and wallpaper still gets its own
`Background`-layer surface. What changed is who declares it: `shell.lua`, not a Rust-owned role.
Read "a third static surface" as "a third surface in the default config." That this ADR had to add a
role to a supposedly fixed set is the evidence ADR-0038 builds on.

Amendment (ADR-0055): there is no `wallpaper` capability and no wallpaper-specific Rust code of any
kind, a `Background` panel holding an `image` node is the whole feature. The layer choice this ADR
argued for survives verbatim.

## 0008. Depend on smithay-client-toolkit instead of hand-dispatching Wayland protocols

The renderer depends on `smithay-client-toolkit` (SCTK) directly and uses its `wlr_layer`,
`presentation_time`, `session_lock`, `foreign_toplevel_list`, `output`, and `seat` modules, rather
than hand-rolling registry binding, layer-shell surface creation, and presentation-feedback dispatch
on the same low-level crates SCTK is already built on.

Checked SCTK's source directly (`Smithay/client-toolkit`): `src/shell/wlr_layer` wraps
`zwlr_layer_shell_v1`/`zwlr_layer_surface_v1`, `src/presentation_time.rs` wraps
`wp_presentation`/`wp_presentation_feedback` with a typed `feedback()` call (the evidence mechanism
ADR-0003 depends on), `src/session_lock` wraps `ext_session_lock_v1`, `src/foreign_toplevel_list.rs`
and `src/output.rs` cover active-window and output tracking, `src/seat` bundles keyboard/pointer
handling with `xkbcommon`. That covers nearly every Wayland-facing protocol the Renderer needs,
already typed and delegate-dispatched, maintained by the same team that maintains `wayland-client`
itself.

Raw `wayland-protocols` stays only for `wp-text-input-v3` (the `textfield` primitive's IME binding),
which SCTK does not wrap. This follows the project's own rule: if an installed dependency already
solves it, use it rather than reimplementing it on top of the same lower-level crate.

## 0009. Text-input service owns text-input-v3, textfield nodes never touch the protocol

The renderer gets one `TextInputService` bound to the seat that owns the `Dispatch<ZwpTextInputV3,
D>` impl and the raw event sequence. Scene nodes never see the protocol.

`textfield` (IDL §5.2) is IME-aware and maps to `wp-text-input-v3`, but no maintained crate wraps it
beyond `wayland-protocols`' raw generated bindings (gated behind `unstable`). Followed Noctalia's
shape (`src/wayland/text_input_service.h`, `src/ui/text_input_client.h`): each `textfield` scene
node implements the equivalent of `TextInputClient`, reporting state (surrounding text, cursor,
purpose, sensitive/hidden flag) and receiving edits as a batched diff (`commitText`, `preeditText`,
delete-before/after lengths), not raw keystrokes. A diff is the only representation that handles
real IME composition (e.g. CJK input) correctly. `on_change` fires from the edit diff, effectively
on the protocol's `done` event, not per keystroke.

Composes with SCTK (ADR-0008) on one event loop: SCTK's `calloop` feature already drives one
`wayland_client::EventQueue`, and the app's top-level state struct implements `Dispatch<T, D>` for
SCTK's delegated types via `delegate_*!` macros. The hand-written `Dispatch<ZwpTextInputV3, D>` for
`TextInputService` slots into that same struct and queue;
`zwp_text_input_manager_v3::get_text_input(seat)` uses the `wl_seat` SCTK's `seat` module already
provides. One dispatch table, no bolt-on thread.

`oblisk-idl-api-specs.md` §5.2's wording is corrected to match.

## 0010. Supervisor owns idle-notify and lock authority, with its own Wayland connection

The Supervisor binds `ext_idle_notifier_v1` and `ext_session_lock_v1` on its own Wayland connection,
separate from the Renderer's.

The session-lock half is superseded by ADR-0042; the idle-notify half stands. `ext-session-lock-v1`
requires the compositor to keep the session locked when the lock client dies, so a Renderer crash
cannot fail open, and a locked session hides every non-lock surface (an `overlay_canvas` lock UI is
invisible while locked, and lock surfaces cannot be shared across processes). The Renderer now holds
the lock and paints it, not the Supervisor.

`ext_session_lock_v1` and its `ext_session_lock_surface_v1` objects are bound to whichever process's
connection created them and cannot cross a process boundary. Locking is a security boundary, not a
visual one: the process that holds "is the screen locked" must be durable enough that a Renderer
crash never drops coverage, even for one frame. The Supervisor uses `smithay-client-toolkit`'s
`session_lock` module for the lock and hand-dispatches `ext_idle_notifier_v1` against raw
`wayland-protocols` (no SCTK wrapper exists for it). On lock, it commits a minimal fallback surface
itself, a solid-color `wl_shm` buffer with a pre-rasterized "locked" indicator, no GLES3/EGL context
in the privileged process; the full Lua-styled lock UI still renders through the Renderer while it
is alive, but the Supervisor's fallback is authoritative and repaints immediately if the Renderer's
surface drops.

This generalizes: the Supervisor owns anything a Renderer crash or reload cannot be allowed to
interrupt (authority handles, data-stream backends), never presentation.

Rejected: keeping session-lock Renderer-owned and accepting the crash window as rare and
low-severity, because a security boundary that fails open on a GPU driver panic or Lua VM crash is a
bug, not an acceptable trade-off.

## 0011. Renderer loads GL function pointers via glow, not the `gl` crate

The renderer depends on `glow` directly for GLES3 function-pointer loading, not the `gl` crate.

The renderer targets `EGL_OPENGL_ES3_BIT`/GLES3 contexts. The `gl` crate (`brendanzab/gl-rs`,
published as `gl` on crates.io) hardcodes `Registry::new(Api::Gl, (4, 5), Profile::Core, ...)` in
its `build.rs`, generating desktop OpenGL 4.5 Core bindings with no feature flag to select
`Api::Gles2`/`Api::Gles3`. It cannot produce GLES3 bindings at all, regardless of configuration.

`femtovg`, already a dependency, pulls in `glow` transitively for its own OpenGL backend and
constructs that backend from a `glow::Context`. `glow` loads function pointers generically via any
`get_proc_address`-shaped closure and supports desktop GL, GLES, and WebGL from one crate, so it
both solves the loading requirement and is what the rendering engine hands a context to directly.

The renderer depends on `glow` directly (not just transitively through femtovg) and loads it via
`khronos_egl::Instance::get_proc_address` after context creation. `gl` is dropped from
`renderer/Cargo.toml` entirely, since nothing in the dependency tree can make it produce GLES
bindings.

## 0012. FemtoVG's glyph atlas is used as-is, not bridged from cosmic-text's shaper

cosmic-text's async shaping wrapper and FemtoVG's glyph atlas stay two separate, independently
correct pieces, not a single bridged pipeline.

The plan called for cosmic-text's shaped glyphs to flow into a 2048x2048 GPU texture atlas inside
FemtoVG. FemtoVG 0.26.0's glyph-atlas-filling path (`GlyphAtlas::render_atlas`,
`femtovg/src/text.rs`) is `pub(crate)` and only accepts `PositionedGlyph`s produced by FemtoVG's own
internal shaper (`rustybuzz`/`swash` via its own `Font`/`FontFaceRef` types through
`Canvas::add_font_mem` plus `fill_text`/`measure_text`). There is no public entry point to hand it
glyphs shaped by anything else. Its atlas pages are also a private fixed `TEXTURE_SIZE = 512`
constant that grows by adding further 512x512 pages, not by expanding one fixed 2048x2048 texture;
that exact number isn't configurable through any public API.

Decision: cosmic-text's async shaping wrapper (`renderer/src/text/shaping.rs`) and FemtoVG's atlas
(`renderer/src/text/atlas.rs`) stay separate for this milestone. cosmic-text's `ShapingHandle` is
the off-thread measurement/shaping primitive, useful on its own to a future layout engine that needs
string width before laying out a `Text` node. FemtoVG's `Canvas`/`TextPainter` handles GPU
rasterization through its own public font/text API, using its own internal shaper and atlas as
designed. `ShapingHandle::default_font_bytes()` gives both systems the same font, found once
off-thread via cosmic-text's `fontdb` discovery, so they agree on what font is measured and drawn
without agreeing on how it is shaped internally.

Rejected: reimplementing atlas packing on top of FemtoVG's lower-level
`draw_glyph_commands`/`GlyphDrawCommands` to bridge in cosmic-text's shaped runs, because that means
hand-rolling UV/kerning bookkeeping FemtoVG's own private atlas code already does correctly.

Rejected for now: a hand-rolled 2048x2048 atlas using FemtoVG's public
`create_image_empty`/`update_image` to upload cosmic-text/swash-rasterized bitmaps via an
image-pattern fill, which would hit the spec's literal 2048x2048 number. Held off because there is
no scene-graph `Text` node yet to drive a packer's eviction policy against; revisit once a real
`Text`/`Icon` node needs atlas control finer than FemtoVG's own, e.g. cross-surface glyph sharing or
eviction tuned to actual working set.

## 0013. Polkit agent registration uses zbus_polkit for the Authority proxy, hand-writes the AuthenticationAgent side

`supervisor/src/dbus/polkit.rs` uses the `zbus_polkit` crate for the polkit `Authority` proxy, and
hand-writes the `AuthenticationAgent` D-Bus interface polkitd calls back into.

The wire method is `org.freedesktop.PolicyKit1.Authority.RegisterAuthenticationAgent`, signature
`(subject: (sa{sv}), locale: s, object_path: s) -> ()`, at `/org/freedesktop/PolicyKit1/Authority`
on `org.freedesktop.PolicyKit1` (verified against polkit's own source,
`src/polkit/polkitauthority.c` and `data/org.freedesktop.PolicyKit1.Authority.xml`).
`RegisterAuthenticationAgentWithOptions` also exists but is unused here. `Subject`'s `subject_kind`
is one of `unix-session` (`session-id: s`), `unix-process` (`pid: u`, `start-time: t`), or
`system-bus-name` (`name: s`). The callback interface,
`org.freedesktop.PolicyKit1.AuthenticationAgent`, requires `BeginAuthentication(action_id: s,
message: s, icon_name: s, details: a{ss}, cookie: s, identities: a(sa{sv})) -> ()` and
`CancelAuthentication(cookie: s) -> ()`.

`zbus_polkit` (MIT, `dbus2` org, same org as `zbus`) ships an `Authority` `#[zbus::proxy]` trait and
matching `Subject`/`Identity` types whose
`register_authentication_agent`/`unregister_authentication_agent` signatures match the verified wire
signature exactly; pinned at `zbus_polkit = "5.0.0"` against this workspace's `zbus = "5.19.0"`,
`default-features = false, features = ["tokio"]`. No maintained MIT/Apache crate exists for the
`AuthenticationAgent` side, so it is hand-written as a `#[zbus::interface(name =
"org.freedesktop.PolicyKit1.AuthenticationAgent")]` impl.

`current_session_subject()` builds a `unix-session` `Subject` from `$XDG_SESSION_ID` rather than a
`unix-process` subject built from this process's own pid, because `pam_systemd` sets that variable
for every real session; a logind round-trip to resolve a session id from a pid is the
general-purpose upgrade path once the crate needs a logind client for some other reason.

zbus 5.x renamed the interface macro from zbus 3.x's `#[dbus_interface]` to `#[zbus::interface]`;
this crate's test module uses the corrected name.

Rejected: `zbus-polkit-agent`, which wraps agent-hosting scaffolding, because its GPL-3.0-or-later
license doesn't match the workspace's MIT/Apache-2.0 dependencies (SCTK, zbus, femtovg), not worth a
license mismatch for a handful of lines.

## 0014. SecureBuffer uses the zeroize crate, not secrecy

`shared::SecureBuffer` (`shared/src/secure_buffer.rs`) wraps a `Vec<u8>` with `#[derive(Zeroize,
ZeroizeOnDrop)]`, not `secrecy`.

ADR-0005 requires zeroing the buffer explicitly right after the one sanctioned read (serializing it
into an outgoing IPC envelope), with `Drop` only as a backup for the panic/early-return case.
`secrecy`'s `SecretBox<T>`/`SecretString` zeroize automatically on `Drop` but have no public method
to zero on demand while the value is still alive; the only way to force an early zero is to drop it.
`zeroize`'s `Zeroize` trait gives a `.zeroize()` method callable at any point, and `ZeroizeOnDrop`
adds the `Drop` backup, covering both halves of ADR-0005's shape from one pair of derives with no
extra wrapper type.

`push_str` appends one edit diff's UTF-8 bytes (matching ADR-0009's diff-based edit model, not
per-keystroke). `expose_secret()` is the one sanctioned read; `.zeroize()` is what a caller invokes
right after that read crosses the IPC boundary, with `Drop` zeroizing again as backup.

Lives in `shared`, not `supervisor` or `renderer`, because the secret originates in the Renderer
(`textfield` keystrokes) and is read in the Supervisor (PAM); the type must be constructible and
readable from both sides of the IPC boundary it crosses. Neither crate is wired to construct or
consume one yet (ADR-0015).

`Vec<u8>::zeroize()` is best-effort: it zeros initialized elements and the Vec's spare capacity, not
just the logical length via `.clear()`, which matters because `.clear()` alone would leave plaintext
bytes on the heap. Both scrub points only touch the buffer's current backing allocation, so a naive
`Vec::extend_from_slice` growth during `push_str` would allocate a new block, copy old bytes over,
and free the old block without zeroing it, leaking a plaintext prefix neither `.zeroize()` nor
`Drop` could reach afterward. `push_str` manages growth itself: allocate the new block, copy bytes
across, zeroize the old `Vec` in place, then drop it.

## 0015. Polkit's PAM conversation and textfield/IPC wiring are deferred, not built

The full polkit authorization path (privileged action triggers a request, Supervisor pushes
challenge metadata over IPC to a Lua dialog, user types into a `secure_submit` `textfield`, the
resulting `SecureBuffer` feeds a PAM conversation that answers polkitd) is not built yet. Two links
are missing and building them now would mean guessing at interfaces nothing else in the codebase
defines: no PAM crate is in the dependency tree, and driving a real `pam_conv` (prompting, reading
the response, replying to polkitd via `AuthenticationAgentResponse2`) is a separate concern from the
D-Bus handshake itself; and neither the `textfield` scene node nor the Unix socket server that would
carry IPC traffic exists yet (both are later phases), so there is nothing on either end of
ADR-0005's envelope-attachment design to attach to.

`supervisor/src/dbus/polkit.rs`'s `AuthenticationAgent::begin_authentication` is a real,
dispatchable D-Bus method, correctly typed and wired into `supervisor::main` against the live
session bus, that forwards the parsed `BeginAuthenticationCall` over an `mpsc` channel instead of
driving PAM. `main()` drains that channel with a log line (`eprintln!`) as a placeholder until the
event loop exists to push it to the Renderer. `cancel_authentication` is a no-op past dispatching
correctly, since there is no in-flight PAM conversation to cancel yet. `shared::SecureBuffer`
(ADR-0014) is built and tested but has no `textfield` write site or PAM read site yet.

Upgrade path, in order: (a) the Unix socket server and event loop give `main()` something real to
push challenges through instead of `eprintln!`; (b) the scene graph's `textfield` node and its
`secure_submit` wiring (ADR-0005) give the Renderer a place to fill a `SecureBuffer` from keystrokes
and send it back; (c) a PAM crate (likely `pam-client` or hand-rolled FFI against `libpam`,
unresearched) replaces the channel-forward with a real conversation, consuming the `SecureBuffer`
and calling `zbus_polkit`'s `Authority::authentication_agent_response2`.

Not built: PAM conversation driving, `textfield`/IPC envelope attachment. Does not contradict
ADR-0005 or ADR-0009, both of which describe the target shape once the scene graph and IPC layer
exist.

## 0016. Per-app audio stream PID uses `application.process.id`, not `sec.pid`

Decided which PipeWire property correctly identifies the process that owns an audio stream node,
needed for the per-app mixer.

`sec.pid` (`PW_KEY_SEC_PID`, wire name `pipewire.sec.pid`) lives on the `Client` object, not the
stream `Node`, and for any app routed through the `pipewire-pulse` compatibility shim (most
PulseAudio-API apps: browsers, `speech-dispatcher`, etc.) it reports `pipewire-pulse`'s own pid, not
the app's. `node.client-id` is not a real PipeWire property key; the real link from node to client
is `client.id`. `application.process.id` (`PW_KEY_APP_PROCESS_ID`) is set directly on the stream
node by the client library for both native and PulseAudio-compat clients, and matched the real
owning process in every case checked against `pw-dump` and `/proc/{pid}/comm`.

`application.process.id` is absent from a `pipewire-pulse`-routed stream's first `global` event;
`pipewire-pulse` adds it moments later as a `PROPS` property change on the node's `info` event.
`supervisor/src/audio/mixer.rs`'s `on_global` filters on `media.class` alone at `global` time, binds
the node unconditionally on a match, then runs the full `media.class` + `application.process.id`
parse inside the bound node's `info` callback (fires once on bind, again on every later property
push). No separate `Client` bind or `client.id` lookup is needed.

## 0017. `audio.apps` push to Lua is deferred, not built

Phase 6 builds a real, dispatchable PipeWire registry listener for per-app audio streams, but does
not push the result anywhere Lua can read it: no IPC socket server or Lua VM exists yet, the same
gap ADR-0015 hit for polkit's challenge metadata.

`supervisor/src/audio/mixer.rs::run` forwards each updated `Vec<AppStream>` snapshot over an
unbounded `mpsc` channel; `supervisor::main` drains it via `tokio::select!` and logs it
(`eprintln!`) as the ceiling, mirroring the pattern ADR-0015 already established.

Not built: `audio:set_app_volume`, `audio:set_app_muted`, master `audio.volume`/`audio.muted`,
default sink/source routing, BlueZ codec control. `AppStream` carries no `volume`/`muted` fields
since nothing populates them.

Since built: the socket, Lua VM, and the audio push into Lua as a real signal, in ADR-0022 (Phase
11's minimal end-to-end slice uses the mixer's already-real data as the first payload pushed
through). Generalized to per-capability push in ADR-0037.

## 0018. Process-group spawn/reap primitives land without `process.run`, a registry, or the PBA orchestrator

Phase 7 ships only the two low-level primitives its spec body actually asks for. Building
`process.run`'s Lua binding, a process registry, stdout/stderr streaming to Lua, or the PBA
generation-swap orchestrator now would mean guessing at interfaces nothing else in the codebase
defines yet, the same situation ADR-0015 and ADR-0017 hit for polkit and `audio.apps`.

1. **`spawn_group_leader(cmd, args) -> io::Result<Child>`.** Spawns as the leader of a new,
   independent process group. Uses `tokio::process::Command`'s safe `process_group(0)` builder
   instead of the spec's literal `unsafe { .pre_exec(setpgid) }` snippet: passing `0` uses the
   child's own pid as the PGID, confirmed in vendored tokio 1.53.1 source, so no `unsafe` block is
   needed.
2. **`reap_process_group(child, grace) -> io::Result<ReapOutcome>`.** `SIGTERM` to the whole group
   via `nix::sys::signal::killpg`, waits up to a caller-supplied `grace: Duration`
   (`DEFAULT_REAP_GRACE` names the value for the eventual real caller, not the spec's hardcoded
   100ms), escalates to `SIGKILL` on the whole group if it hasn't exited.
   `ReapOutcome::ExitedCleanly`/`Escalated` makes the escalation decision observable in the return
   value.

Both ship `#[allow(dead_code)]`, declared in `main.rs` but not wired into its runtime. Tested
against real OS process/process-group behavior, no mocks: differing pgid from the test process,
clean `SIGTERM` reap, escalation on a `SIGTERM`-ignoring child, and a grandchild backgrounded into
the same group also reaped.

Since built: `process.run`'s Lua binding and a real process registry, in ADR-0026.
`reap_process_group` gets its first real caller in ADR-0025's PBA orchestrator wiring.

## 0019. PBA control-socket transport and Lua AST evaluation are deferred, not built

Phase 8 ships the Presentation-Before-Authority generation-swap ordering and gating contract only,
not the surrounding system `oblisk-supervisor-services-dbus.md` § 15.1-15.4 describes. Deferred,
each for the same reason as ADR-0015/0017/0018 (no consumer or transport exists yet to build
against): the real Unix control-socket wire transport; Lua AST evaluation (no working Lua VM exists
anywhere yet); Renderer-side null-buffer Wayland commit and `wp_presentation_feedback`;
NetworkManager/BlueZ state-hydration payload content; true per-`(generation, output)` evidence
fan-out (ADR-0003 already specifies the target shape); the `§ 15.4` "Swap" messages (input
deselection on `N`, promotion signal to `N+1`); wiring `reload.rs` into `main.rs`; and the `inotify`
config-watch trigger.

1. **Real process lifecycle.** `run_pba` calls `process::spawn_group_leader` (Overlapping Spawn) and
   `process::reap_process_group`, both on the success path (reaping generation `N` after evidence
   verification) and on every failure path (aborting the Candidate). First real caller of ADR-0018's
   primitives.
2. **`CandidateLink`, the IPC-boundary trait.** Four methods, one per `§ 15.2-15.3` operation:
   `push_state_snapshot` (state hydration), `recv_ready_signal` (null-buffer staging),
   `send_activate_draw` (activate draw), `recv_presentation_evidence` (evidence verification).
   Reuses `shared::StateSnapshot` for hydration. Does not reuse `shared::CommandEnvelope` for
   `ActivateDraw`: that envelope (`oblisk-idl-api-specs.md` § 7.2) is a generation-guarded wrapper
   around a Lua-initiated write action traveling Renderer to Supervisor, the opposite direction and
   shape from a Supervisor-issued activation nonce, so `ActivateDraw` carries a plain `u64` nonce
   instead.
3. **Failure semantics**, not spelled out by § 15's happy path: any failure before presentation
   evidence is verified, a `CandidateLink` error or a ready/evidence deadline expiring, aborts the
   Candidate and leaves generation `N` untouched and authoritative. `N` is reaped only after
   evidence verification succeeds, never before.

Tested with a fake `CandidateLink` driving handshake timing (immediate success, delayed success,
hung calls, link error) against real short-lived child processes for both generations, so the reap
primitives run for real. Ten tests cover full-success promotion, `N` staying observably alive
mid-verification, a hang at each of the four handshake steps aborting cleanly and tagged with the
correct `Stage`, and an aborted Candidate's process group confirmed reaped via `/proc`.

`reload.rs` only orchestrates the generation-swap path; it has no opinion on and does not touch
ADR-0001's in-place (value-change) reload path.

Since built: the real control-socket transport and `CandidateLink` production implementation
(`SocketCandidateLink`), in ADR-0025. Lua evaluates `shell.lua` for real starting ADR-0023, driven
from a real production caller by ADR-0024's Watcher.

## 0020. Control-socket transport ships without dispatch, PBA wiring, or `process.run` streaming

Phase 9 builds a real Unix control-socket transport and connection-identity handshake, explicitly
deferring everything downstream of "a frame arrived": the command-dispatch routing table
(`oblisk-idl-api-specs.md` § 3.2's ~30 write commands, still decoded and forwarded to an aggregated
`eprintln!` channel, the same ceiling ADR-0015/0017 already hit); `CandidateLink`'s real
implementation (ADR-0019's trait still has no production implementer, though this phase's framing is
the primitive it will use); `process.run`'s line-streaming (called out in the phase's own text as "a
different transport concern"); any handshake deadline (an idle client blocks only its own connection
task, not the accept loop); and real generation-ID assignment (`renderer/src/socket.rs` reads
`OBLISK_GENERATION_ID` from the environment, defaulting to `0`, since nothing yet spawns a Renderer
with a real one). Reconnection, backoff, and auth are not built either, unnecessary for a local
`AF_UNIX` socket restricted by filesystem permissions.

1. **`shared::framing`.** Generic, transport-agnostic: a 4-byte big-endian length prefix ahead of a
   JSON payload, generic over `AsyncRead`/`AsyncWrite` so a real `UnixStream` and an in-memory
   `tokio::io::duplex` pair exercise the same code path in tests. Enforces `MAX_FRAME_LEN` (16 MiB)
   against the length prefix before allocating a payload buffer, closing the DoS a `u32` length
   prefix invites on a socket ADR-0005 already slates to carry secure textfield submissions.
   `shared::ConnectionHandshake` (`{ generation_id: u32 }`) is the one new wire type, sent first on
   every connection.
2. **`supervisor/src/socket.rs`.** Binds `$XDG_RUNTIME_DIR/oblisk-shell.sock`, never `/tmp`
   (world-writable). Clears a stale socket file left by an unclean prior shutdown before binding,
   otherwise `bind` fails with `AddrInUse` on every restart after a crash. Accepts unboundedly many
   simultaneous connections, since generation `N` and Candidate `N+1` are both connected during a
   swap, registering each by `generation_id` in a `GenerationRegistry` so a later caller can address
   a specific generation directly (`GenerationRegistry::send_to`).
3. **`renderer/src/socket.rs`.** Connects as the client on its own OS thread with a dedicated
   current-thread tokio runtime, since the main thread is occupied by `wayland::run()`'s blocking
   dispatch loop. Holds the connection open indefinitely after the handshake; reading a real payload
   back is later work.

Tested against real I/O throughout: framing round-trips over `tokio::io::duplex`; the Supervisor's
listener binds and accepts over a real `UnixListener` in a `tempfile::tempdir()` path with two
simultaneous connections registered by distinct `generation_id`s; the Renderer's client connects to
a real listener and its handshake decodes correctly.

Since built: the command-dispatch routing table, in ADR-0037 (per-module dispatch replacing the
aggregated log channel). `CandidateLink`'s production implementation and PBA wiring, in ADR-0025.
`process.run`'s line-streaming, in ADR-0026.

## 0021. Lua loader ships without retained-scene reconciliation or signal memoization

Phase 10 builds a real `mlua` VM instantiation and a loader evaluating `shell.lua` into a node tree
and surface topology, deferring: full retained-scene reconciliation (`deserialize_lua_table`
converts one Lua table into one `VirtualNode` shallowly, never recursing into `children`, never
matching fresh nodes against a previous tree by identity); write-command dispatch back through Phase
9's socket (`on_click`/`on_change` are stored as opaque Lua values, nothing calls them or routes
them); `textfield`'s `secure_submit` wiring to `SecureBuffer`; computed-signal memoization and
invalidation (nothing yet pushes a new value into an existing `Signal`, so there is no staleness to
track); per-field node-property schema validation; real `oblisk.*` system-signal population beyond
hand-constructed test values; `list`'s reconciliation-aware repeater semantics and `button`'s real
input dispatch; and a production call site for `Loader` in `main.rs`.

1. **The type-marshalling boundary** (`marshal.rs`). `check_number`/`check_integer`/`check_string`
   enforce what the spec's type table constrains beyond `mlua`'s automatic mapping: finite `f64`
   (NaN/Inf rejected), `i64`/`u64` within `[-2^53+1, 2^53-1]`, a 64KB `String` cap.
2. **`Signal`/`computed`** (`signal.rs`). A `Signal` is `Direct` (a plain pushed value) or
   `Computed` (a Lua closure plus dependency `Signal`s, re-run on every `get()`);
   `computed(dependencies, fn)` passes each dependency's current value positionally, not the
   `Signal` handle. The 5ms CPU cap is real: `Lua::set_interrupt` requires `mlua`'s `luau` feature,
   unavailable since this workspace builds against `lua54`, so the cap uses `Lua::set_hook` with an
   every-1000-instruction counter hook that errors once elapsed time exceeds budget.
   `Lua::set_hook`/`remove_hook` occupy one unstacked slot per Lua thread, and `call_with_cpu_cap`
   is reentrant (a `computed` body can read a second `Signal` before returning), so a naive
   install-before/remove-after-every-call version let an inner call's `remove_hook` strip an outer
   call's still-active cap; the fix is a deadline stack in `Lua::app_data`, installed only on the
   0->1 depth transition, removed only on 1->0, always checking the innermost deadline so a finished
   inner call hands enforcement back to the outer one.
3. **Node constructors and `VirtualNode`** (`nodes.rs`).
   `rect`/`row`/`column`/`text`/`icon`/`button`/`list`/`textfield`/`surface` tag their props table
   with a `kind` field and return it unmodified; `deserialize_lua_table` converts one such table
   into a `VirtualNode`, pulling `kind` out and copying every other key into `properties` as a raw
   `mlua::Value`.
4. **`Loader`.** `Loader::evaluate(source)` requires the top-level return to be a `surface` node or
   a non-empty array of them, returning `LoadOutput { surfaces: Vec<VirtualNode> }`. Because
   `deserialize_lua_table` never recurses, each surface's own topology fields sit directly in its
   `properties` bag.

Since built: retained-scene reconciliation, in ADR-0023's layout engine. `Loader` gets a production
call site starting ADR-0022's minimal end-to-end slice. Signal memoization was later decided
against, not built: ADR-0044 decision 3 keeps every signal re-resolving on every read, deliberately.

## 0022. Minimal end-to-end slice ships one ad hoc signal, not the `oblisk.*` tree

Phase 11 wires Phase 9's socket to Phase 10's loader using the audio mixer's real data as the
payload, without changing either phase's existing shape.

Three pieces make the connection. `Signal::new_live` adds a third
`SignalKind::Live(Rc<RefCell<Value>>)` variant alongside `Direct`/`Computed`; only `Live` can be
overwritten after construction, through `LiveSignalHandle::set`. It uses `Rc<RefCell<_>>`, not
`Arc<Mutex<_>>`, because a `Loader` stays confined to one OS thread (the socket-client thread),
matching `supervisor/src/audio/mixer.rs`'s own `Rc<RefCell<MixerState>>` convention; no cross-thread
`Send` bound is needed. `Loader::set_global` and `Loader::to_lua_value` expose the Loader's existing
global-registration and JSON-to-Lua conversion internals to a caller outside the module, additively.
`handle_snapshot`/`receive_loop` in `renderer/src/socket.rs` replace the socket's previous no-op
hold: each `StateSnapshot` frame is converted to a Lua value, pushed into a live signal registered
as the global `audio`, and re-evaluates the hardcoded `PROOF_OF_WIRING_SHELL` script through the
real `Loader::evaluate` path, so this proves the production API composes end to end. A frame that
fails to decode ends the loop, unlike the Supervisor's inbound `CommandEnvelope` loop, which
tolerates bad frames, because this connection has exactly one sender and one message shape.

Not built: the full `oblisk.*` signal tree and per-capability namespacing (only one global, `audio`,
exists); `expected_revision`/staleness rejection (ADR-0004) (last-write-wins is safe only because
the single ordered Unix-socket connection cannot reorder frames); real generation-ID assignment
(both sides agree on hardcoded `0` by coincidence of shared defaults, not a real handshake);
reconnection (a dropped socket ends the thread with no retry, per ADR-0020's own
no-accept-loop-restart ceiling); a real `shell.lua` file (still a hardcoded Rust string re-evaluated
on every push); anything downstream of `LoadOutput` beyond logging each surface; any capability
besides `audio::mixer`; write-command dispatch back through the socket (Renderer -> Supervisor stays
push-only; the Supervisor is the listener, the Renderer connects as client).

## 0023. Layout engine ships a stacking model, not a full constraint solver

Phase 12 built `renderer/src/layout/`: typed, validated property parsing (`node.rs`) and one
recursive function, `resolve_and_reconcile` (`scene.rs`), doing the
constraint-down/size-up/position-down passes together per node in a single recursion rather than
three separate tree walks.

`rect`(with children)/`button`/`surface`(with `child`) got no real arrangement formula. Each
resolves every child against the full content box independently, positioned by its own
`align_h`/`align_v`, with no spare-space distribution across siblings; children can overlap. Chosen
because it is well-defined and covers the common real shape without inventing a second
axis-distribution concept the spec never specified. `row`/`column` alone got the spec's explicit
intrinsic-size formulas. Reconciliation matched fresh nodes to retained ones by position within each
parent's children list, reusing a `NodeId` when the kind at that position matched; top-level
surfaces were the exception, keyed by their own `id`. Removed subtrees tore down child-first into a
`retiring` bag rather than dropping immediately. A `Fill`/`Percent` child of a `Content`-sized
row/column resolved to zero in that axis (children got a `0.0` budget when the parent's own size
wasn't yet known); this was deliberate and matches what CSS answers, not a missing pass.

Rejected: a full two-pass constraint solver, because the single-recursion stacking model was
well-defined and covered the shapes needed at the time.

Not built in this phase: `list`/`textfield` support, a confirmed `Percent` literal syntax, a live
`wl_region` push, real per-output pixel dimensions, a real GPU resource behind the retained-scene
lease, a shared `ShapingHandle` (a second one was spawned instead, duplicating startup cost),
anything downstream of a resolved tree (the paint pipeline), and a recursion-depth limit.

Amended by ADR-0044: `Signal`-valued geometry properties, originally rejected outright, are now
resolved at layout time, and a live push marks the scene dirty instead of triggering a
re-evaluation.

Amended by ADR-0045: positional child matching lost node identity whenever a config inserted a node
above an existing sibling. Nodes may now carry a parent-scoped `id`; identified children pair before
the rest fall back to positional matching, and the deferred `list` support gains a `key`
requirement.

Superseded by ADR-0077: taffy owns the layout math.

## 0024. In-place reload: the Renderer self-diffs topology, the Supervisor only dispatches

Phase 13 wires a Supervisor-side `inotify` watcher on `~/.config/oblisk/` that, on a debounced edit,
asks the current generation's loader to re-evaluate and report its new surface topology. The
Renderer classifies its own re-evaluation as `Unchanged`, `TopologyChanged`, or `Failed`; the
Supervisor only dispatches on that verdict.

The Renderer classifies, not the Supervisor, because it already holds both the old (applied) and new
(freshly evaluated) topology in one process. Shipping a topology DTO across the wire so the
Supervisor can redo a comparison it cannot do more cheaply would be pure duplication. The Supervisor
still owns the swap-vs-in-place dispatch decision and its execution, just not the classification.

Wire protocol: `ReevaluateRequest { sequence }` (Supervisor -> Renderer),
`ReevaluateReport::{Unchanged, TopologyChanged, Failed} { sequence, .. }` and `ApplyPendingReload {
sequence }` (Renderer -> Supervisor) round-trip once per debounced edit, correlated by `sequence`.
Both ends guard on `sequence`: the Renderer applies only a matching `ApplyPendingReload`, and the
Supervisor's `is_current_reload` check acts on an `Unchanged` report only if its `sequence` still
matches the most recently sent `Reevaluate`, closing a race where a superseded report could fire
`reset_registrations`/`ApplyPendingReload` after a newer edit had already landed. A
`TopologyChanged` verdict never stashes a pending scene: a topology-changed generation must not
mutate its own scene, that is a swap's job, not the in-place path's.

`applied_topology` is `Option<Vec<SurfaceTopology>>`, not a bare `Vec`. `None` means nothing applied
yet, safe to apply the next evaluation; `Some(vec![])` is a real zero-surface generation. Without
the distinction, a failed startup evaluation left the shell permanently blank: every later fix still
diffed against the empty topology as `TopologyChanged`, which is never applied, instead of
`Unchanged`.

The debounce window is a fixed 200ms (`RELOAD_DEBOUNCE`), tracked as an absolute deadline
(`Option<Instant>`), not a relative sleep recomputed each loop iteration, because the relative
version let an unrelated directory event push the deadline back. The watcher watches the config
directory, not `shell.lua`'s own inode, because an atomic-save editor unlinks and recreates the file
rather than writing in place; it filters to `shell.lua` by name, so nothing else in the directory
triggers a reload.

Rejected: reusing `shared::CommandEnvelope`'s shape for the new messages.
`SupervisorFrame`/`RendererFrame` use a hand-rolled adjacently-tagged enum instead, because
`CommandEnvelope` guards a Lua-initiated write action traveling Renderer -> Supervisor, not an
engine-internal control message traveling either direction.

Not built: a generation swap on `TopologyChanged` (logs and stops; `run_pba` is built and tested but
has no runtime caller yet, per ADR-0019); `reset_registrations` doing anything (a real, called,
currently-empty function, since no capability yet registers anything against a `generation_id`;
ADR-0006 also requires the reset run before the fresh evaluation repopulates registrations, which
this round trip cannot honor because the reset decision depends on the evaluation's own verdict, so
it needs the same pending/apply staging the Scene already has once a real registration capability
exists to stage against); the full `oblisk.*` signal tree (only an ad-hoc `rescue` global exists,
mirroring ADR-0022's `audio`); a startup-failure fallback scene (first boot has no prior scene to
roll back to, unlike a later re-evaluation); real multi-generation bookkeeping (hardcoded to
generation `0`, per ADR-0020's ceiling); resolving `layer`/`anchor`/`monitor` against real Wayland
outputs.

## 0025. PBA orchestrator wired with atomic per-candidate promotion, not true per-output streaming

Phase 14 gives `reload::run_pba` and its `CandidateLink` trait (ADR-0019) their first real
transport, per-output evidence collection, and a real Renderer-side
null-buffer/`ActivateDraw`/`wp_presentation_feedback` handshake.

Real wire types (`shared/src/lib.rs`): `ActivateDraw { nonce }`, `ReadySignal { surfaces }`,
`PresentationEvidence { nonce, surface_id }`, `DeselectInput { surface_id }`, `PromoteGeneration {
surface_id }`, adjacently tagged per ADR-0024's convention. `SocketCandidateLink`
(`supervisor/src/reload_link.rs`) borrows the Supervisor's shared `inbound_frames` channel and a
cloned `GenerationRegistry` for the duration of one in-flight handshake; a frame from the wrong
`generation_id` or an unrelated type/nonce is logged and dropped, not routed elsewhere.

Promotion stays atomic per candidate, not streamed per output. `drive_handshake` collects evidence
per `surface_id` (the wire-level granularity ADR-0019 item 5 asked for) but still gates the Swap
(`DeselectInput`/`PromoteGeneration`) on all expected surface_ids reporting within one shared
`evidence_timeout`. `PbaOutcome::promoted_surfaces` is always either every expected surface_id or
the operation fails entirely; there is no partial-success shape. Streaming promotion per output
while the whole candidate stays abortable on a global timeout is actively unsafe: if 2 of 3 surfaces
promote and the 3rd times out, aborting the candidate now would black out the 2 already-transferred
surfaces, exactly the failure PBA exists to prevent. `run_pba` stops right after evidence
verification, dropping the `superseded: &mut Child` parameter; the caller (`main.rs`) sends the Swap
messages, then reaps `superseded` directly, since § 15.4's ordering (Input Deselection, then
Candidate Promotion, then Reap) needs two different connections and `CandidateLink` is scoped to
only the candidate's.

Timings: `ready_timeout: 2s`, `evidence_timeout: 3s`, `reap_grace` reuses the existing
`DEFAULT_REAP_GRACE` (100ms).

Renderer candidate mode (`renderer/src/wayland/mod.rs`): on first configure, a candidate commits a
null buffer instead of binding EGL, and sends the full `ReadySignal` surface_id list once every
tracked surface is null-buffered. `activate_draw` requests `wp_presentation_feedback` immediately
before `swap_buffers`, so the request associates with that commit.

Rejected: hand-written `Dispatch<WpPresentation, _>`/`Dispatch<WpPresentationFeedback, _>` impls, as
the phase's own draft spec assumed. `smithay-client-toolkit` 0.21.1 already ships a tested
`presentation_time` module (`PresentationTimeState`, `PresentationTimeHandler`); `App` implements
that trait directly and correlates evidence itself, since the module's `feedback()` does not accept
custom user data. `wp_presentation_feedback` has no `destroy` request: both `presented` and
`discarded` are protocol-marked `type="destructor"`, so the object invalidates automatically once
either fires.

Not built: Scene-to-GPU rendering (`activate_draw` still draws Phase 3/4's static proof content, not
Lua-authored `Scene` output; no later phase owns this gap either); a topology-driven arbitrary
surface set (`surface_id` names today's fixed
`main_bar`/`overlay_canvas`/one-`wallpaper_layer`-per-output set, not ADR-0003's full per-monitor
model); real effects for `DeselectInput`/`PromoteGeneration` (logged and dropped, no per-surface
input-region/focus machinery exists yet); concurrent PBA handshakes (the main loop blocks
synchronously for one handshake, deliberate while swaps stay rare and nothing else is
capability-routed over the socket, per ADR-0020); a distinct fast-fail for
`wp_presentation_feedback`'s `discarded` event (the existing `evidence_timeout` already catches a
surface that never presents); a packaging/install-path story (`current_exe()`'s sibling-binary
assumption stands in); real NetworkManager/BlueZ state-hydration content (still reuses the last
PipeWire `StateSnapshot` or an empty one, per ADR-0019 item 4's already-open gap).

Closes ADR-0019 items 1, 3, 6, and 7 in full; item 5 (true multi-output fan-out) only partially,
gated by the atomic-promotion decision above; item 4 (NetworkManager/BlueZ hydration) remains open.

## 0026. `process.run`'s Lua binding and piped stream registry ship; `textfield`/PAM stay deferred

Phase 15 implements only `process.run`'s Lua binding and non-blocking stdout/stderr piping, closing
ADR-0018 items 1-2. `textfield`'s `secure_submit` and a real PAM conversation stay out of scope:
`textfield` has no scene node yet (`ensure_supported_kind` rejects it as
`LayoutError::UnsupportedNodeKind`, per ADR-0023) and no `wp-text-input-v3` crate in the dependency
tree; a PAM crate choice is still unresearched and needs its own spike and ADR before being wired
in.

The Lua `Loader` and `dispatch_loop` share one thread (the socket-client thread), so a `process.run`
closure needs no cross-thread bridge, unlike Phase 14's Wayland-thread work, only an in-thread
`tokio::sync::mpsc` queue to hand its request to `dispatch_loop`'s `select!`. The wire protocol
reuses `CommandEnvelope` (`capability: "process"`, `action: "run"`/`"kill"`) rather than a new
request type. `CommandEnvelope.id` is assigned by the Renderer, a monotonic counter scoped to one
`ProcessRegistry`, because `process.run` must return a `ProcessHandle` to Lua synchronously, before
any socket round trip can complete.

The Supervisor-side registry is a channel-actor, not a shared mutex: `processes:
HashMap<(generation_id, id), Child>` lives as a plain local in `main()`, mutated only from
`main()`'s own `select!` arms, matching every other spawn-tracking piece of state in this codebase.
A new `spawn_group_leader_piped` primitive is added alongside `spawn_group_leader` rather than
changing it, since the boot Renderer spawn and every PBA candidate rely on inherited stdio.

`ProcessStream` (`Stdout`/`Stderr`) is a real enum on the wire; `ProcessOutputLine { id, stream,
line }` and `ProcessExited { id, code: Option<i32> }` join `SupervisorFrame`, `code` absent exactly
when `ExitStatus::code()` itself would be `None` (signal death, or never spawned). Four Supervisor
functions carry the logic: `spawn_and_register_process` (spawns, takes the piped streams off the
`Child` before registering it); `stream_process_output` (a detached task reading both streams line
by line, reporting completion once both hit EOF, without knowing the exit code);
`kill_registered_process` (calls `reap_process_group`, this phase's promised real caller per
ADR-0018); `reap_exited_process` (calls `child.wait()` inline once streams have closed). Both a
missing registry entry on kill and a naturally-exited process are silent no-ops, the same
`ESRCH`-as-success tolerance ADR-0018 established. On generation supersede, every `processes` entry
for the superseded `generation_id` is reaped without sending `ProcessExited`, since that
generation's connection is already torn down.

Callback convention: `out_cb(line, stream)` with `stream` as the Lua string `"stdout"`/`"stderr"`
(Lua has no enums, matching how other IDL fields already cross as strings); `exit_cb(code)` as a Lua
integer or `nil`.

Two correctness fixes made during review: `stream_process_output`'s EOF does not mean the process
exited (a daemonizing child can close stdio while continuing to run), and the original inline
`child.wait()` inside `main()`'s single top-level `select!` would wedge the entire Supervisor, every
inbound command and reload included, for as long as such a process kept running. Fixed by splitting
completion handling into a sync `take_exited_process` and a detached `wait_and_report_exit` task. A
malformed `process.run` command and `KillOutcome::ReapFailed` both previously leaked the
Renderer-side pending callback pair forever by never sending `ProcessExited`; both now send
`ProcessExited { code: None }`.

Rejected: the claim "no `Arc<Mutex<...>>` anywhere in this codebase" (both in this ADR's draft and
in doc comments) is false, `socket::GenerationRegistry`'s own `connections` map already is one,
predating this phase. Narrowed to: no spawn-tracking state uses a shared mutex.

Not automated: a real Supervisor and Renderer as two separate processes spawning a third
`process.run`ed process together over a real socket, per ADR-0024/0025's own established ceiling.

## 0027. Textfield wire shape: secure submit frame and text-input bridge

An ordinary `textfield` uses `zwp_text_input_v3`. A masked `secure_submit` field reads `wl_keyboard`
directly instead and never binds text-input at all.

1. **Correction to ADR-0026.** ADR-0026 claimed no `wp-text-input-v3` crate exists in this
   dependency tree. It is wrong: `renderer/Cargo.toml`'s `wayland-protocols` unstable feature
   already gates `text_input::zv3`. Only the `TextInputService`/scene-node code was missing, not the
   Cargo dependency.
2. **Seat binding.** One `wl_seat`, bound via SCTK's `seat` module (ADR-0009). No multi-seat support
   exists anywhere in the codebase.
3. **`on_submit` trigger, ordinary fields.** Triggered by `zwp_text_input_v3`'s protocol-native
   `ACTION_SUBMIT` event, not a separate `wl_keyboard` listener. This is IME-correct: it works with
   CJK composition, which raw keystroke detection does not.
4. **Secret wire shape.** The secret crosses the supervisor/renderer boundary as a distinguished
   `RendererFrame::SecureSubmit { generation_id, capability, action, secret: Vec<u8> }`, never
   through `CommandEnvelope::arguments`. `arguments` is generic `serde_json::Value`; routing a
   `SecureBuffer` through it would leave a plaintext copy `.zeroize()` can never reach, undermining
   ADR-0014. `SecureSubmit` is built once from `SecureBuffer::expose_secret()`, sent, and the source
   buffer is zeroized immediately after (ADR-0005).
5. **Cross-thread bridge.** `on_change`/`on_submit` reuse the `std::sync::mpsc` (Wayland thread) to
   `tokio::sync::mpsc` (socket thread) bridge already proven for `process.run` (ADR-0026), carrying
   one keyed edit-diff struct: commit text, preedit text, delete-before/after lengths, and a
   `submit` bool folded into the same diff rather than a separate event kind.

Amendment: text-input-v3 needs a compositor-side IME bound. With none running, `commit_string` never
arrives, so a password field built this way is unusable on a bare session, which keeps a lock screen
that depends on it locked on purpose (ADR-0042, `ext-session-lock-v1`). So `secure_submit` reads
`wl_keyboard` directly and the text-input binding is dropped for that field kind entirely, both
because an IME must not see a password's candidate text, and because a masked field has no
composition to be correct about: ADR-0005 already makes its value unreadable from Lua and its
`on_submit` argument-free.

Not built: `TextInputService`, the `textfield` scene-node kind, the seat binding,
`RendererFrame::SecureSubmit` and its supervisor-side dispatch, and the cross-thread channel pair.
This ADR records the wire shape; implementation is a later pass.

## 0028. PAM: nonstick, a re-exec worker subprocess, one-shot protocol

PAM authentication runs in a re-exec'd worker subprocess, driven by the `nonstick` crate, answering
every PAM prompt with one password already known before the worker is spawned.

1. **Crate: `nonstick`, not `pam-client`.** ADR-0015 named `pam-client` as a candidate, but its last
   release was July 2022; `nonstick` covers both the PAM-application and PAM-module directions, is
   actively maintained, and its application-side `Conversation` trait's `masked_prompt()` returns
   `PamResult<OsString>` programmatically with no terminal I/O. `masked_prompt()`'s `OsString`
   return is not zeroizable, PAM's own C `char*` boundary, not something any crate can avoid; the
   `OsString` is built from `SecureBuffer`'s bytes at the last possible moment inside the
   `Conversation` impl, and the source buffer is zeroized immediately after (ADR-0005, ADR-0014).
   This system has no `/etc/pam.d/polkit-1`, so PAM falls back to the `"login"` service, matching
   both Quickshell's and Noctalia's own default.
2. **Isolation: a re-exec worker subprocess, not `fork()`, not a third binary crate.** Quickshell
   and Noctalia both isolate the PAM conversation in a child process because PAM has no way to abort
   a running module except by aborting the process (fingerprint scanners and hardware keys don't
   abort otherwise). Both use bare `fork()`, unsafe to copy here: `supervisor` runs a multi-threaded
   tokio runtime plus a raw `audio::mixer` OS thread, and `fork()` in a multi-threaded process only
   duplicates the calling thread, so locks held by other threads stay locked forever. Instead,
   `supervisor` re-execs its own binary via `std::env::current_exe()` (same resolution as
   `renderer_binary_path()`) with `OBLISK_PAM_WORKER=1`, branching `main()` into a minimal PAM-only
   path before any D-Bus/tokio/audio setup runs. No new binary crate is added; this reuses
   `process::spawn_group_leader`/`reap_process_group` as a fourth real caller (ADR-0018's promised
   upgrade path).
3. **Protocol: one-shot, not interactive.** Quickshell's protocol is bidirectional and live,
   relaying each PAM prompt back and waiting for an answer, built for conversations where answers
   aren't known upfront. Oblisk's flow matches Noctalia's model instead: per ADR-0027, the password
   is fully captured client-side and crosses as one complete `RendererFrame::SecureSubmit` frame
   before the Supervisor spawns anything PAM-related, so there is no live prompt to relay. The
   password is written to the worker's stdin once and the pipe closed immediately after;
   `nonstick`'s `Conversation::masked_prompt()` answers every PAM message with that same value
   inside the worker. The worker reports exactly one outcome frame over stdout when the conversation
   ends, no request/response round trips, and does not reuse `RendererFrame`/`SupervisorFrame`
   (wrong domain: those are Supervisor-Renderer wire types, this is Supervisor-to-its-own-worker).
   The outcome uses an exit-code taxonomy, not a bare bool: `{Success, StartFailed, AuthFailed,
   MaxTries, PamError, OtherError}`.

Still open: `begin_authentication` currently discards `_identities: Vec<(String, HashMap<String,
OwnedValue>)>`; `authentication_agent_response2(uid, cookie, identity)` needs a real uid/`Identity`
parsed from that list, left to the implementation pass rather than designed here.

Not built: the `OBLISK_PAM_WORKER` branch in `main()`, the worker's `pam_start`/`Conversation` loop,
the one-shot stdin/stdout framing (reusing `shared::framing::write_json_frame`/`read_json_frame`
rather than hand-rolling binary framing), and the `identities` parsing into
`authentication_agent_response2`.

## 0029. NetworkManager: capability-tagged state snapshot and secure connect flow

NetworkManager state pushes ride a newly capability-tagged `StateSnapshot`, D-Bus access uses an
existing crate rather than a hand-written proxy, and Wi-Fi passwords travel through `secure_submit`,
not a plain IDL argument.

1. **Capability tagging.** `shared::StateSnapshot` gains `capability: String`; `main.rs`'s single
   `audio_revision: u32` becomes `revisions: HashMap<String, u32>`; `apply_state_snapshot` looks up
   or lazily creates the matching Lua signal by `capability` instead of hardcoding `audio`.
   `payload: serde_json::Value` stays untyped, no per-capability payload struct, until a second
   capability's shape actually needs distinguishing beyond its name. Closes ADR-0022 item 1 and
   matches `CONTEXT.md`'s glossary definition of Revision as a capability's own state-version
   counter.
2. **D-Bus access: `rusty_network_manager`, not a hand-written proxy.** It exports
   `NetworkManagerProxy`, `DeviceProxy`, `WiredProxy`, `WirelessProxy`, `AccessPointProxy`,
   `SettingsProxy`, `SettingsConnectionProxy`, `ConnectionProxy`, covering the required interface
   set, is MIT-licensed, and its `zbus = "5.11.0"` requirement is compatible with this workspace's
   `zbus 5.19.0`. Adopted per ADR-0013's rule to reuse a maintained proxy crate before hand-writing
   one.
3. **Listener architecture: async in the existing `select!`, not a dedicated thread.**
   `audio::mixer` needs its own `std::thread::spawn` because PipeWire's client API is
   callback-driven; NetworkManager is D-Bus-native, so its `PropertiesChanged`/AP-added/AP-removed
   signal streams merge into `main.rs`'s top-level `tokio::select!` instead, following
   `dbus::polkit`'s precedent. Write actions (`scan`, `connect`, `forget`) are `tokio::spawn`ed
   rather than awaited inline: ADR-0028 found that an inline-awaited call with no ceiling inside
   this same `select!` can wedge the whole Supervisor if the far end hangs, and none of these three
   actions need to hand a synchronous result back through the calling envelope.
4. **Password via `secure_submit`, not the IDL's literal `connect(ssid, pwd, hid)`.** The IDL's
   plain-argument signature contradicts ADR-0005, which names Wi-Fi password entry as
   `secure_submit`'s own motivating case. `network:connect(ssid, hidden)` (a normal
   `CommandEnvelope`, no password) stashes a single-slot pending connect intent, mirroring
   `pending_challenge` from ADR-0028; a `textfield`'s `secure_submit = "network.connect"`
   (capability `"network"`, action `"connect"`) always follows with the password bytes. An empty
   secret means an open network: skip `802-11-wireless-security` and call
   `AddAndActivateConnection2` with the minimal dict; a non-empty secret populates `wpa-psk`. One
   code path serves both scanned and hidden networks, because a hidden network's AP security flags
   are never broadcast and so can't be inspected the way a scanned network's can; the Lua widget
   always renders a password field, left empty for open networks. `docs/oblisk-idl-api-specs.md`
   §2.5's signature is corrected to `connect(ssid, hidden)`.
5. **Ethernet toggle.** No NetworkManager method fabricates a carrier connection, since link carrier
   is hardware-detected. `set_ethernet_enabled(false)` calls `Device.Disconnect()` on every type-1
   device. `set_ethernet_enabled(true)` looks for the device's existing auto-connect profile and
   calls `ActivateConnection` on it if one exists; a missing profile is a no-op, not an error.
6. **Push cadence: no debounce.** A completed scan can fire a burst of AP-discovery signals. The
   `NetworkState` accumulator (mirrors `audio::mixer`'s `Rc<RefCell<MixerState>>`) rebuilds and
   pushes a fresh `StateSnapshot` on every relevant event; `revision` already makes intermediate
   pushes harmless. Debounce is added later only if a real scan burst proves chatty enough to
   matter.

Still open: the exact `NetworkState` struct shape, `forget()` looping over every matching connection
profile (not just the first), and `RequestScan`'s options dict (empty by default). Left to the
implementation pass.

## 0030. BlueZ controller: hand-written proxies, Just-Works-only pairing, deferred codec control

The BlueZ controller (`oblisk.bluetooth`) hand-writes its D-Bus proxies, enforces Just-Works-only
pairing through its own agent, and defers PipeWire audio codec control to a later ADR.

1. **Proxy crate: hand-written, not `bluer`.** `bluer` depends on the
   `dbus`/`dbus-tokio`/`dbus-crossroads` family, not `zbus`; adopting it would mean two D-Bus client
   stacks in one process. The only zbus-based alternatives (`blues`, `bluebus`) are unmaintained or
   unreviewed. Same rung of ADR-0013's ladder as `dbus::polkit`: hand-written
   `#[zbus::interface]`/proxy types against `Adapter1`, `Device1`, `Battery1`, `Agent1`,
   `AgentManager1`, and `org.freedesktop.DBus.ObjectManager`.
2. **Pairing is Just-Works-only, enforced by our own `Agent1`.** We register `Agent1` with
   `AgentManager1` using capability `"NoInputNoOutput"` (forces Just Works for any SSP-capable peer)
   and call `RequestDefaultAgent` at controller construction, not lazily on first `pair()`, so any
   pairing attempt on the machine hits our policy instead of BlueZ's undocumented default agent.
   `RequestPinCode`/`RequestPasskey`/`DisplayPinCode` return `org.bluez.Error.Rejected`: legacy
   PIN-only devices cannot pair, intentional, since the IDL's `pair(mac)` takes no PIN/passkey
   argument and defines no `secure_submit` target for bluetooth.
   `RequestConfirmation`/`DisplayPasskey`/`AuthorizeService`/`RequestAuthorization` auto-accept
   unconditionally, since there is no UI to ask a human and refusing would break `connect()` for
   already-trusted or Just-Works devices. `Cancel`/`Release` are no-ops.
3. **`set_audio_codec(mac, codec)` is deferred.** Codec switching needs a live PipeWire `Device`
   proxy and `SPA_PARAM_Profile` switching (the spec's claim of `SPA_PARAM_Route` is wrong, verified
   against PipeWire's `bluez5-device.c`: codecs are enumerable Profiles like `a2dp-sink-ldac`,
   switched via `Device.SetParam`/`SPA_PARAM_Profile`). `audio::mixer`'s PipeWire thread has only
   ever pushed `StateSnapshot` out; ADR-0017 deferred `set_app_volume`/`set_app_muted` for the same
   reason, no inbound channel exists. Building it here would design that channel under `bluetooth`'s
   name instead of the `audio` capability that owns it, so it waits for `audio`'s own
   inbound-command design.
4. **Device tracking is a dynamic per-object registry, not a scan-and-replace list.** BlueZ's
   `Device1` set is unbounded and changes live via `ObjectManager` `InterfacesAdded`/`Removed`, and
   `Battery1` can appear or disappear independently on an already-tracked device.
   `HashMap<OwnedObjectPath, DeviceEntry>` keyed by object path, not MAC (path is what
   `ObjectManager` events give natively; MAC is derived only for signal output and path lookup).
   Hydrated once via `ObjectManager.GetManagedObjects()` at startup. Each `InterfacesAdded` carrying
   `Device1` spawns one forwarder task, the same shape as ADR-0029's `spawn_wifi_signal_forwarder`
   instantiated per device, listening to `PropertiesChanged` (`Connected`, `Paired`, `Name`, plus
   `Battery1.Percentage` if present); its `JoinHandle` is stored and aborted on `InterfacesRemoved`.
5. **Categorization parses `Class` ourselves, not BlueZ's `Icon`.** `Icon` is BlueZ's own derivation
   of `Class`/`Appearance` and comes back empty whenever `Class == 0`, common for BLE peripherals
   before GAP data is read. Bit layout: bits 8-12 Major Device Class, bits 2-7 Minor. Major
   `0x01`→`"computer"`, `0x02`→`"phone"`, `0x04` (Audio/Video) minor `0x01`/`0x02`→`"headset"`,
   minor `0x06`→`"headphones"`, other Audio/Video minors→`"generic"` (a wrong specific guess, e.g. a
   car kit shown as headphones, is worse than neutral). Major `0x05` (Peripheral) minor top-2-bits
   `01`→`"keyboard"`, `10`→`"mouse"`, `11` (combo)→`"keyboard"`. Everything else→`"generic"`.
6. **Push cadence: no debounce**, matching ADR-0029. BlueZ property-change volume (human-scale
   pairing/connect events, infrequent battery ticks) doesn't approach a rate where debounce pays for
   itself.
7. **`discovered_devices` clears on `start_discovery()`, not on `stop()`.** Matches NetworkManager's
   scan-replace semantics for a fresh session; the last snapshot stays visible after
   `stop_discovery()` so the UI doesn't blank immediately. No cap: discovery sessions are short and
   human-driven.
8. **Single adapter, first one found.** Same single-well-known-device assumption ADR-0029 makes for
   Wi-Fi; the IDL exposes one flat `oblisk.bluetooth` signal with no adapter selector.

`StateSnapshot{capability: "bluetooth"}` needs no new plumbing in `shared`/`main.rs`/`socket.rs`,
ADR-0029 already generalized the capability-tagging path.

Not built: `set_audio_codec` (needs `audio`'s inbound PipeWire command channel, which also unblocks
`set_app_volume`/`set_app_muted`), a PIN/passkey UI flow for legacy-device pairing, and
multi-adapter support.

## 0031. Tray controller: hand-written SNI/DBusMenu host, IconName preference, no cache-busting

The tray controller hand-writes its own StatusNotifierWatcher/Item/DBusMenu proxies, prefers a tray
item's `IconName` over decoding its pixmap, and skips PNG cache-busting on icon updates.

1. **No crate reuse, hand-write the Watcher, Item, and DBusMenu proxies.** `system-tray` (the only
   real candidate) always couples Watcher+Host registration with no way to run one without the
   other, has a source-verified bug in `IconPixmap::from_array` (reads width and height from the
   same field index twice, silently squaring every non-square pixmap's height), and its
   `IconPixmap.pixels`/`MenuItem.icon_data` come back as raw undecoded bytes anyway, so it would not
   save the security-critical bounds-checking/PNG-encoding work this controller must do regardless.
   Same rung of ADR-0013's ladder as `dbus::polkit`/`dbus::bluetooth`: hand-written proxy/interface
   types against `org.kde.StatusNotifierWatcher`, `org.kde.StatusNotifierItem`, and
   `com.canonical.dbusmenu`, including DBusMenu layout parsing (four D-Bus calls, one recursive
   struct).
2. **Watcher/Host registration.** `RequestName("org.kde.StatusNotifierWatcher")` with no
   `ReplaceExisting`/`DoNotQueue`; `NameTaken` is treated as success, deferring to a real
   desktop-environment session already running one. Either way,
   `RegisterStatusNotifierHost(our_unique_name)` is called against whichever process owns the name,
   working standalone and coexisting with Plasma/GNOME on the same session bus.
3. **Registry keys on the resolved D-Bus unique name, never the caller-supplied `service` string.**
   The `service` argument (an object path or a bus name) is resolved once to a unique name (`:N.M`,
   spec-guaranteed to contain only digits/colons/dots), which becomes both the registry key and,
   with the leading `:` stripped, the PNG spool filename component. Using the raw `service` string
   for either would be a path-traversal/filename-injection risk from any process on the session bus.
   Spool path is `/dev/shm/oblisk-$UID/tray/{sanitized_unique_name}.png`, fixing a spec
   inconsistency where the example paths omitted `$UID` and would collide across users on a shared
   machine.
4. **Icon source: prefer `IconName`, decode `IconPixmap` only as fallback.** IDL §5.2's `icon` node
   already takes a raw theme name string and resolves it renderer-side, so pushing `icon_name`
   through skips the bounds-check/decode/PNG-spool pipeline entirely. When `IconPixmap` is the only
   source, take the largest available pixmap capped at 128px, with no assumed display-size floor:
   Lua's `icon` node `size` field owns display size, not the D-Bus layer, and downscaling a large
   source always beats upscaling a small one.
5. **Menu tree: eager top-level fetch, `AboutToShow`-driven per-submenu refresh.** `GetLayout` is
   fetched in full on item registration and re-fetched on `LayoutUpdated`, so `tray.items[].menu`
   has no first-open latency. But some real DBusMenu apps (NetworkManager's applet menu is the
   canonical example) leave a submenu's children empty until `AboutToShow(id)` fires, so a new write
   command `tray:menu_will_show(id, submenu_id)` fires `AboutToShow` and re-fetches that submenu's
   layout before Lua renders it.
6. **Click semantics enforced in Rust, not left to Lua discipline.** `tray:activate(id, x, y)` calls
   `Activate(x, y)` only when `item_is_menu` is false; when true it no-ops, per SNI's own documented
   semantics, enforced once centrally rather than trusting every `shell.lua` author to gate on
   `item_is_menu`. New `tray:activate_menu_item(id, menu_item_id)` calls DBusMenu's
   `Event(menu_item_id, "clicked", ...)`.
7. **`SecondaryActivate`/`ContextMenu`/`Scroll` deferred, not built.** No known real consumer needs
   them; modern tray items overwhelmingly expose a `Menu` for right-click instead of `ContextMenu(x,
   y)`.
8. **PNG encoding: the `png` crate, not `image`.** Pure Rust, encode-only, minimal dependency tree;
   `image` is already a transitive dependency but only carries decode/conversion machinery this
   controller never touches, matching the same minimal-over-already-present preference as the
   `bluer` disqualification in ADR-0030.

`StateSnapshot{capability: "tray"}` needs no new plumbing in `shared`/`main.rs`/`socket.rs`,
ADR-0029 already generalized the capability-tagging path.

Amendment: cache-busting was deferred here and built later, in the Renderer, by ADR-0054. A
path-keyed texture cache served an app's first tray icon forever, because the spool path is
overwritten in place on every `NewIcon` with no revision suffix. This ADR's decision is unchanged,
the spool still overwrites in place; the fix is entirely on the Renderer side, which keys its cache
on the file's modification time and length as well as its path.

## 0032. Idle capability splits transport but keeps one controller

`oblisk.idle` gets one controller (`dbus::idle` module) shared by two backends, Wayland idle-notify
and D-Bus logind inhibit, rather than splitting ownership between them.

Notify allocates one `ext_idle_notification_v1` listener per distinct threshold duration, not per
registration, fanned out through a `HashMap<Duration, Vec<registration>>`; two callers registering
the same duration share a listener. There is no unregister command: idle threshold cleanup reuses
the existing `reset_registrations` (ADR-0006), which already clears a generation's registrations on
reload. Notify uses `get_idle_notification`, not `get_input_idle_notification`, because nothing
needs presence-sensor exclusion. A new `SupervisorFrame::IdleEvent { generation_id, threshold_sec,
state }` wire variant (`state: "idled" | "resumed"`) carries the event straight to the registered
Lua callback, bypassing `StateSnapshot`'s revision-polling path, since idle is event-shaped rather
than pollable state.

Inhibit uses `org.freedesktop.login1.Manager.Inhibit(what="idle", who="oblisk", why=reason,
mode="block") -> fd` on the Supervisor's existing system-bus connection, not the Wayland
`idle-inhibit-unstable-v1` protocol, which needs a `wl_surface` the Supervisor does not own. Lock
lifetime is fd lifetime: released automatically if the holding process dies, so a Supervisor crash
cannot leak a stuck inhibit. `what` is scoped to `"idle"` only, governing auto-suspend, not sleep,
shutdown, or lid-switch. Inhibit is refcounted per generation rather than a boolean:
`inhibit(reason)` opens the fd on 0->1, `release_inhibit()` closes it on 1->0, so two concurrent
callers cannot clobber each other; `reset_registrations` zeros the count too.

Cargo: `supervisor`'s `wayland-protocols` dependency gets the `"staging"` feature, needed to expose
`ext::idle_notify::v1`.

Graceful degradation matches every other controller: if `ext_idle_notifier_v1` isn't advertised, or
the Supervisor's dedicated Wayland connection fails to establish, log once and construct an inert
controller where `register_threshold` silently no-ops. Inhibit has no equivalent degrade path; it
rides the system-bus connection already required for NetworkManager, BlueZ, and polkit, so only
per-request `Inhibit` failures are possible.

Rejected: `zwp_idle_inhibit_manager_v1`, because it would split inhibit's owner from notify's for no
gain and require a `wl_surface` the Supervisor doesn't hold.

## 0033. Notifications advertises a real capability set, with Lua-configured sound and DND

The notifications server advertises the real freedesktop capability set it implements, parses body
markup into structured spans instead of stripping it, and gives Lua control over per-urgency sound
and do-not-disturb.

`GetCapabilities` returns 10 strings: `action-icons`, `actions`, `body`, `body-hyperlinks`,
`body-images`, `body-markup`, `icon-static`, `persistence`, `sound`, `inline-reply`. `icon-multi` is
excluded because the base `Notify()` signature has no wire mechanism for multiple icon sizes.
`inline-reply` is a KDE extension riding the `x-kde-reply` hint.

Body markup is allowlist-parsed into `Vec<NotificationSpan>`, not stripped to plain text: only
`<b>`, `<i>`, `<u>`, `<a href="URL">`, `<img src="PATH" alt="ALT">` are accepted (text runs carry
`{text, bold, italic, underline, href}`, image runs carry `{image_path}`); everything else,
including scripts and style tags, is rejected as before. `<img src>`, `image-path`, and action-icon
names resolve through one new path-trust validator: a sender-supplied path is accepted only as
absolute, under `/usr/share/icons`, `/usr/share/pixmaps`, `$HOME/.local/share/icons`, or
`$HOME/.icons`, confirmed to exist and be a regular file under the same size cap as `image-data`. A
bare theme name degrades to no icon rather than resolving it; full XDG theme-name resolution is a
separate, unbuilt `system:find_icon` IDL row. Span rendering (the FemtoVG/cosmic-text side) is out
of scope this round: the supervisor delivers correct span data, the renderer draws it later.

`ActionInvoked` emits the real 2-argument base-spec signature, `ActionInvoked(id: u32, action_key:
string)`, not a 3-argument variant, since a 3-arg signal would break every real D-Bus client's
introspection assumptions. Reply text rides inside `action_key` as `"inline-reply::<text>"`,
confirmed against Noctalia's source; a bare `"inline-reply"` with no `::` is logged as malformed and
not acted on.

Sound resolves in order per `Notify()` call: `hints["suppress-sound"] == true` forces silence; else
a valid `hints["sound-file"]` (through the same path-trust validator) plays; else the urgency tier's
`notifications:set_sound(urgency, path)` registration plays; else nothing. `hints["sound-name"]` is
not honored, the same missing theme-resolution gap as icon names. Playback is a one-shot PipeWire
stream triggered over an internal Rust channel, no Lua/wire round-trip.

`notifications:set_dnd(bool)` toggles one Supervisor-held global boolean, not per-generation state.
It gates sound playback only; `notifications.feed` keeps receiving everything, since there is no
popup/toast concept to suppress. Critical urgency bypasses DND for sound and also ignores
`expire_timeout`, persisting until explicitly dismissed. DND state lives in Supervisor memory only
this round, pushed to Lua as `notifications.dnd: boolean` in the same `StateSnapshot` as the feed;
it does not survive a full Supervisor restart.

Wire shape reuses `StateSnapshot`, no new `SupervisorFrame` variant: unlike idle (ADR-0032's
`IdleEvent`, genuinely edge-triggered), every notifications mutation changes the feed list or `dnd`
flag directly, so a fresh `StateSnapshot` push after each mutation is enough.

SHM icon storage uses `/dev/shm/oblisk-$UID/notifications/notif-{id}.png`, the same `$UID` fix
ADR-0031 established for tray. `notifications.feed`'s 20-item view is a truncation over a 100-item
backing FIFO, kept so `dismiss(id)`/`reply(id, text)` still resolve items scrolled out of the
visible window. A `replaces_id` update with no fresh image clears and deletes the old spooled icon
file; a FIFO eviction past the 100-item cap deletes the evicted item's file in the same step.

New write rows: `notifications:reply(id, text)`, `notifications:set_sound(urgency, path)`,
`notifications:set_dnd(enabled)`, alongside the existing `notifications:dismiss(id)`. Read shape
gains `urgency: "low"|"normal"|"critical"`, `has_reply: boolean`, and a span-array `body`.

## 0034. Keyboard backlight, locks, layout, camera privacy, and Arch update checking

Five hardware-fact capabilities the user asked for by name, following the proven
D-Bus/hardware-controller pattern rather than waiting on spec docs. `oblisk.keyboard` (already
declared) grows to hold `backlight_pct`, `caps_lock`/`num_lock`/`scroll_lock`, and
`active_layout`/`active_layout_index`/`layout_count`, domain-split like every existing capability
rather than lumped into one bucket. New `oblisk.privacy` (`camera_users: table`) and
`oblisk.updates` (`count`, `packages`, progress fields). No new "adapter" trait unifies the five
mechanisms (D-Bus, PipeWire, sysfs+evdev, subprocess+timer, compositor socket): they already share
an identical convention (own connection/thread, push into a channel, `main.rs` `select!`s it)
without one, and forcing a trait before any caller needs `Vec<Box<dyn Adapter>>` polymorphism would
be speculative generality. A new `supervisor/src/hardware/` tree, sibling to `dbus/`, holds all
five, since only backlight even touches D-Bus.

### 0034.1. Keyboard backlight rides UPower, not sysfs

Uses `org.freedesktop.UPower.KbdBacklight`, verified live against this dev machine with `busctl
--system introspect`: a single fixed object at `/org/freedesktop/UPower/KbdBacklight` with
`GetBrightness() -> i`, `GetMaxBrightness() -> i`, `SetBrightness(i)`, and signal
`BrightnessChanged(i)`. No `EnumerateKbdBacklights`, `SetPercentage`, or
`DeviceAdded`/`DeviceRemoved` exist on this UPower version, so there is nothing to enumerate or
hotplug.

1. **Percent is derived, not native.** `GetMaxBrightness()` is read once at construction and cached,
   since a keyboard's step count doesn't change at runtime. `backlight_pct = round(100 * brightness
   / max)` (round-half-away-from-zero, matching ADR-0035's `round_milli_c`); the D-Bus interface
   itself has only a raw `[0, max]` scale, and this machine reports `max = 3`.
   `keyboard:set_backlight(pct)` converts back as `round(pct * max / 100)`, clamped to `[0, max]`.
2. **No-backlight degrade.** A machine with no keyboard backlight fails the constructor's
   `GetMaxBrightness()` call; the controller degrades to `backlight_pct = -1` (the same sentinel
   convention as `temp_gpu`), logs once, and `set_backlight` becomes a no-op.
3. **Connection reuse.** Rides the already-open system D-Bus connection shared with NetworkManager,
   BlueZ, polkit, and idle-inhibit.

### 0034.2. Keyboard lock state uses evdev, keyboard layout gets its own narrow compositor trait

`§5`'s spec of `caps_lock` via compositor socket interception is wrong: Hyprland's `hyprctl devices
-j` has no `scrollLock` field at all. The next candidate, `wl_keyboard.modifiers`'s `mods_locked`
bitmask, is gated by Wayland surface focus and would never fire for a background daemon like the
Supervisor. A third candidate, sysfs LED nodes
(`/sys/class/leds/*::{caps,num,scroll}lock/brightness`) as primary with inotify for live updates and
evdev as fallback, was also backwards: live-tested on this dev machine (physically toggling Caps
Lock twice, watched with `inotifywait -m`), the sysfs `brightness` value genuinely changed
(`0`->`1`->`0`) but fired zero inotify `MODIFY` events, because this kernel's `input_leds` driver
doesn't call `sysfs_notify()` when it drives the change itself.

1. **Fixed design: evdev primary, sysfs a static fallback.** `evdev::Device::open` on the keyboard
   selected by `supported_leds()` (whichever reports `LED_CAPSL`) gives both initial state
   (`get_led_state()`) and every live change (`into_event_stream()`'s `EV_LED` events) in one
   mechanism; Waybar's `keyboard_state.cpp` already does the same for the same reason. Sysfs is read
   once at construction as a best-effort static value only if evdev can't be opened (permission
   denied, or no LED-capable device found): this machine's LED `brightness` files are root-owned but
   world-readable, while `/dev/input/eventN` needs `input` group or `uaccess`. If neither resolves,
   all three lock fields default `false`, logged once.
2. **Layout gets a real, deliberately narrow compositor trait now**, unlike the "no adapter trait"
   call above, because the user asked for it ahead of the still-unbuilt workspace adaptor. Scope is
   exactly `active_layout(&self) -> Signal<String>`, `switch_layout(&self, index: usize)`,
   `kind(&self) -> CompositorKind`, not widened to guess workspace's eventual surface. Two
   implementors: Hyprland (`.socket2.sock`'s `activelayout` event triggers a full resync via
   `hyprctl -j devices`, write via `hyprctl switchxkblayout <device> <index>`) and Niri (JSON-RPC
   `KeyboardLayouts` query, event-driven off `KeyboardLayoutSwitched`, write via
   `{"SwitchLayout":{"layout":<index>}}`). The Supervisor picks an implementor at startup by probing
   `$HYPRLAND_INSTANCE_SIGNATURE`/`$NIRI_SOCKET`; if neither is set, `active_layout` degrades to
   unavailable.
3. **Single primary device, index-based write only.** `keyboard:switch_layout(index)` is the only
   write; `active_layout_index` + `layout_count` let Lua compute cycling itself, since both
   compositors already support index-based write selection natively.
4. **Correction (implementation round): `active_layout_index` read-back is not symmetric.** Niri's
   `KeyboardLayouts` event gives a names list plus current index directly. Hyprland's `hyprctl -j
   devices` gives only `active_keymap` (a human-readable name) and `layout` (a comma-separated
   XKB-code list) with no code-to-name correlation, so `active_layout_index` cannot be derived on
   Hyprland and stays at its last-known value (`0` until manually confirmed); `layout_count` stays
   accurate, but Lua's index-based cycling silently cannot cycle correctly on Hyprland as shipped.
   Disclosed structural gap, not fixed here.

### 0034.3. Camera privacy: kernel-level detection primary, PipeWire supplementary

The first proposal read Noctalia's PipeWire `Video/Source` node classification as strictly better
than an `inotifywait`+`fuser` approach; the user corrected this from experience, and it checked out.
PipeWire only sees camera access routed through the `xdg-desktop-portal` Camera portal or the opt-in
`pw-v4l2` `LD_PRELOAD` shim; raw V4L2 opens (mpv, ffmpeg, OBS's native v4l2 source, most native
Linux apps) never touch it, so the two mechanisms cover disjoint app populations.

1. **Kernel-level detection is primary.** The `/sys/class/video4linux/video<n>/streaming` flag
   (kernel 6.3+) was proposed but dropped: this dev machine's real UVC webcam doesn't expose the
   attribute despite kernel 7.1, so it couldn't be verified live. Detection is an fd-scan over
   `/dev/videoN` opens/closes plus a `fuser`-style confirm, since inotify alone can't tell "one
   handle closed" from "device free"; this proves `camera_active` for every app regardless of
   transport.
2. **PipeWire is a name-enrichment layer only.** It extends the already-running `audio::mixer`
   registry thread (no second PipeWire connection) to supply a real `application.name` for whichever
   portal-routed app it can see; a raw v4l2 user with no matching PipeWire link falls back to a
   `/proc/<pid>/comm` lookup off the `fuser`-reported PID. `privacy.camera_users: table` is an array
   of `{app_name}`, empty when inactive.

### 0034.4. Arch update checking uses the `alpm` crate, not `checkupdates`/`expac` subprocesses

Uses `alpm` (`github.com/archlinux/alpm.rs`, the Arch org's own `libalpm` binding, used in
production by `paru`), replacing both subprocess calls. Whether `checkupdates`'s `fakeroot` wrapping
is structurally required by the sync step itself, not just `pacman` CLI policy, was resolved by
building and running the real thing: a throwaway program copied `/var/lib/pacman` to a user-owned
temp dir, called `Alpm::new`, registered the `core`/`extra`/`multilib` sync repos, and ran
`syncdbs_mut().update(force: true)` as `uid=1000`, no `fakeroot`, no root; it downloaded a real
8.9MB `extra.db` from a live mirror and found the same 3 outdated packages the real `checkupdates`
binary reports on the same machine. One honest gap: not independently confirmed against `libalpm`'s
C source, only against this repo's real behavior.

1. **Fully separate service from the sysinfo scheduler**, not a fourth metric on it: same
   interval-suspend shape (`updates:configure({interval})`, suspend at `interval=0`) for
   consistency, zero shared code, at the user's request.
2. **Full signal shape, not count-only.** `updates.count`, `updates.packages` (`[{name, old_version,
   new_version, download_size, installed_size}]`), `last_successful_check`, `check_error`.
3. **The capability owns install and progress itself**, not bare Lua `process.run`.
   `updates:install()` builds on the existing `process::spawn_group_leader_piped` primitive, with
   its own progress fields (current step, current package, determinate flag, error, reboot-required
   heuristic) riding the `updates` signal. Installing needs root and routes through Oblisk's
   existing polkit agent (`dbus::polkit`, Phase 5), the first Phase-16-style write action that needs
   privilege elevation.

Rejected: a flat IDL bucket for all five mechanisms, because every existing capability
(`oblisk.battery`, `oblisk.audio`) is domain-named, never lumped.

## 0035. Sysinfo capability: five IDL fields, two hwmon preference lists, watch-driven suspend

`sysinfo` follows the wire-format IDL (five fields) over the looser prose specs (three metrics), the
same precedent ADR-0033 set for notifications: `cpu_percent`, `ram_percent`, `swap_percent`,
`temp_cores`, `temp_gpu`, but only three configurable intervals. Three tasks, not five:
`swap_percent` rides `ram_interval` (computed alongside `ram_percent` from the same `/proc/meminfo`
read); `temp_gpu` rides `temp_interval` (read alongside `temp_cores` from the same hwmon scan).

CPU% comes from `/proc/stat`'s first line (10-field layout: `user nice system idle iowait irq
softirq steal guest guest_nice`): `percent = 100 * busy_delta / total_delta`, `busy = total - (idle
+ iowait)`, the standard `top`/`htop` convention. The task keeps its previous sample in loop-local
state and discards it on every dormant-to-ticking transition, not just cold start, so a
freshly-resumed gauge never reports an hours-old averaged reading as its first value; the first
implementation got this wrong and was caught in review. RAM/swap use `MemTotal`/`MemAvailable`
(`used = total - available`, `percent = 100 * used / total`) and `SwapTotal`/`SwapFree` directly, no
hand-rolled Buffers/Cached estimate since `MemAvailable` already is one.

Temperature resolves two independent hwmon chip-name preference lists once, at controller
construction, not re-scanned per tick, since onboard sensors don't hotplug: `["k10temp",
"coretemp"]` for `temp_cores`, `["amdgpu", "nouveau", "nvidia"]` for `temp_gpu`. `temp_cores` is
every `tempN_input` on the winning CPU chip whose label matches `Core \d+`, sorted by trailing core
index, excluding the package-level aggregate (`temp1_input`/"Package id 0" on `coretemp`); falls
back to `acpitz`'s single sensor as a one-element array if neither `k10temp` nor `coretemp` is
present. `temp_gpu` is the winning GPU chip's primary sensor, or the IDL's own `-1` sentinel if none
of the three names match, verified live on a machine with no discrete-GPU hwmon chip. Wifi, NVMe,
and battery hwmon chips are deliberately excluded from both lists.

Suspend at `interval=0` is a real dormant await, not a polling no-op: each of the three tasks is
driven by a `tokio::sync::watch<Duration>`, and at `Duration::ZERO` the loop awaits only
`watch.changed()`, no `tokio::time::interval` armed, zero wakeups. All three start at
`Duration::ZERO`; nothing polls until Lua calls `configure` at least once.

This is the first capability where more than one independent task writes into the same shared state:
one `Arc<Mutex<SysinfoState>>`, three producers, each updating only its own fields under the lock
before signaling one shared unbounded `mpsc<()>`. `revision` bumps once per push regardless of which
fields changed. Percent fields default to `0` pre-first-sample; no `StateSnapshot` pushes until at
least one field has a real value.

`configure(cfg)` takes a table, the first capability action in this codebase to do so:
`arguments[0]` is a JSON object, not positional args. A present key overrides that task's interval,
an absent key leaves it unchanged; a wrong-typed present key drops the whole call with one
`eprintln!`, no partial-apply. Units are whole seconds. Config is Supervisor-global, not
renderer-generation-scoped, same category as network/bluetooth/tray, since there is no
per-generation cleanup to run on reload or crash.

Sysfs/procfs paths are parameters, never hardcoded: `cpu.rs`/`ram.rs` take `proc_root: &Path`
(default `"/proc"`); `temp.rs`'s chip-resolution and read functions take `hwmon_root: &Path`
(default `"/sys/class/hwmon"`). Tests build real fake-root trees under `tempfile::tempdir()` rather
than mocking strings. Module layout is
`supervisor/src/hardware/sysinfo/{cpu,ram,temp,controller}.rs`, mirroring
`hardware/idle/{notify,inhibit,controller}.rs`'s split-by-concern precedent (ADR-0032); no new
cross-controller adapter trait, per ADR-0034's rejection of one.

`temp_gpu` reading `-1` is the expected, documented outcome on any machine without a matching hwmon
chip (most laptops with integrated-only graphics), not a bug to chase.

## 0036. Mpris capability: playerctld excluded, track-identity caching, strict seek state

Decided to build `oblisk.mpris` with `playerctld` filtered out of discovery, per-player
track-identity caching for art and length, and seek state that only updates from the real D-Bus
signal.

1. **Exclude `playerctld` and non-controllable sources, silently.** `playerctld` is filtered by
   exact bus-name suffix (`org.mpris.MediaPlayer2.playerctld`), the only reliable signal since it
   proxies every property, including `Identity`, from the player it mirrors (confirmed live via
   `busctl`: identity strings are byte-for-byte identical on both). A source reporting `CanControl
   == false` is excluded from tracking at registration, matching Quickshell's own filter. Both are
   just absent from `mpris.players`; `playerctld` itself keeps working for anything that talks to it
   directly (media keys, `playerctl` CLI).
2. **No Supervisor-side "active player" selection.** `mpris.players` is a flat array with no
   `active`/`primary` field; Lua owns any "which one to show" policy, matching every other
   array-shaped capability (`notifications.feed`, `tray`'s item list).
3. **Album art: trust-checked local path passed through, no SHM copy, no HTTP fetch.**
   `album_art_path` is `artUrl` with the `file://` prefix stripped, canonicalized, and confirmed to
   be a real existing file, with no directory allowlist (real players cache art in widely varying
   locations). Empty string if the key is absent or non-`file://`; remote `http(s)://` art is
   unsupported (no HTTP client exists in this workspace, and the renderer has no native
   network-image loader).
4. **Player `id` is the bus-name suffix**, reconstructed on every write and never cached separately.
   **`length` defaults to `-1`** when `mpris:length` is absent or the wrong D-Bus type (a live
   stream/radio case), matching this codebase's existing "genuine unavailable, not a fabricated
   zero" pattern (ADR-0034/0035).
5. **Seek: `SetPosition(trackid, target)` when trackid is known, `Seek(target - position)` fallback
   otherwise; clamp `target` to `[0, cached length]` Supervisor-side before either call.** Does not
   rely on the MPRIS spec's trackid-staleness guard on `SetPosition`, confirmed unreliable by live
   testing against `mpv-mpris`, which honored a deliberately wrong `TrackId` anyway. Position state
   updates only through the real `Seeked`/`PropertiesChanged` signal, never optimistically, keeping
   this codebase's "state flows through the signal, not the write call" convention with zero
   exceptions.
6. **Track-identity caching for `album_art_path`/`length`, keyed by a composite of `trackid` +
   `xesam:url` + `xesam:title`.** Any one field changing marks a track change. On an unchanged key,
   a missing or malformed `artUrl`/`length` in a resync keeps the previous value instead of clearing
   it, since some players omit these keys on some updates for the same still-playing track.
7. **Degrade shape: keep the player entry, degrade only the affected field** on a transient
   property-read failure, matching bluetooth/tray precedent. **Discovery:** `ListNames` scan at
   startup, `NameOwnerChanged` filtered by the `org.mpris.MediaPlayer2.` prefix thereafter.
8. **One capability, N producers, one shared `Arc<Mutex<Vec<PlayerState>>>`**, reusing ADR-0035's
   multi-producer-capability mechanism. **Module layout:**
   `supervisor/src/dbus/mpris/{watcher,player,controller}.rs`, session bus.

## 0037. Capability roster: generic push, per-module dispatch, no merged channel

Amended by ADR-0076: the roster moved from `CAPABILITIES: &[&str]` to the `shared::Capability` enum,
turning decision 2's `push_snapshot` `debug_assert` into a type (deleting the assert) and routing
decision 3's per-module `dispatch` through one exhaustive match in `supervisor/src/capabilities`
instead of a literal `&str` match in `main.rs`. Every decision below still stands; the rejected
merged channel is still rejected, since each capability's channel is still one typed single-variant
enum and `idle`/the audio arm are still the two carve-outs (only the await site moved). The
rejection rested on each capability's residue being "a one-line select arm"; ADR-0070's lazy-start
`Option` wrapper later made each one six lines.

A 2026-08-27 review found each capability's depth well-scoped in its own module but its edges
hand-stamped across `main.rs`/`snapshot.rs`, with a renderer pre-seed list frozen at four names
while nine snapshot capabilities existed.

1. **One generic `push_snapshot`** over `&impl Serialize` replaces nine per-capability
   `push_*_snapshot` clones and the inlined audio copy. ADR-0029 already keeps payloads untyped; the
   capability name is the only real datum.
2. **The capability roster lives in `shared`.** The renderer seeds one live signal per rostered
   name, so every rostered Lua global exists from a generation's first evaluation and reads `nil`
   until its first snapshot, uniformly (including `sysinfo`, `nil` until configured).
   `push_snapshot` asserted roster membership so a forgotten entry failed on the supervisor's first
   push in development, not in a user's `shell.lua` at boot. Unrostered names still fall back to the
   lazy signal path.
3. **Each capability module owns its action dispatch** via one `dispatch(controller, envelope)`
   adapter holding its action match, argument parse, and `tokio::spawn`; `main.rs` keeps a literal
   match with one arm per capability, no registry, no trait. `NetworkState`/`BluetoothState`
   ownership moved into their controllers behind `handle_signal(signal) -> State`; the immediate
   `scanning` flip and clear-on-discovery (ADR-0029/0030) route through each controller's own signal
   channel as new variants (`ScanStarted`, `DiscoveryCleared`) for FIFO ordering, and
   `pending_network_connect` moved into `NetworkController`.

Rejected: merging the seven single-variant `Changed` channels into one `(capability, payload)`
channel. Once decisions 1 and 3 land, each capability's residue is one channel and a one-line select
arm; merging would trade that for serialize-in-controller indirection plus two permanent carve-outs
(`idle` is event-shaped per ADR-0032, audio's mixer arm is bespoke). Do not re-propose unless a
capability needs to push from a context that cannot reach the main loop's select.

## 0038. Surfaces come from `shell.lua`, not a fixed role enum

Decided that the evaluated Lua topology, not a closed Rust `SurfaceRole` enum, is the only source of
Wayland surfaces; each `surface` node from `shell.lua` maps to one `zwlr_layer_surface_v1` per
output it targets.

Amended: ADR-0078 gives `exclusive` a third value (`"Ignore"`, layer-shell's `-1`, reserve nothing
and ignore other surfaces' reservations); decision 2's in-place-update list is unchanged. ADR-0049
amends decision 2: "created once, at startup" still holds for the `panel` and `lock` roles, but not
for the `popup`/`window` roles ADR-0040 added, since `xdg_popup` needs a real input-event serial for
its grab and consumes its positioner at `get_popup` time; for those two roles `visible` creates and
destroys the Wayland object rather than mapping and unmapping it. The declared set is still fixed
for a generation's life, so ADR-0001's topology split is unchanged. ADR-0088 finishes that move:
`visible` now creates and destroys a `panel`'s object too, because the layer-shell re-map the
original decision rested on is not honoured in practice, so all three roles behave the same way.

1. **The evaluated topology is the only source of Wayland surfaces.** `SurfaceRole` and the three
   `create_*` calls in `wayland::run` are deleted; `main_bar`/`overlay_canvas`/`wallpaper_layer`
   survive only as ordinary ids in the default config, not as Rust constants.
2. **A generation creates exactly the surfaces its own evaluation declared, once, at startup.**
   Adding or removing a `surface`, or changing its `layer`/`anchor`/`monitor`/`namespace`, is a
   topology change: the Supervisor spawns a candidate that builds its own surface set from its own
   evaluation. Within a live generation, two things move without a swap: `visible` shows and hides a
   surface, and the fields layer-shell lets a client change live (`margin`, exclusive zone,
   `keyboard_interactivity`, size) apply in place. See the amendments above: "created once" is now
   true only of a surface that has never been hidden, since ADR-0088 made hiding destroy the object
   for every role.
3. **A surface targeting multiple outputs produces one surface instance per output**, generalizing
   the `"{id}@{output}"` surface-id convention (already used for wallpaper) to every surface.
   Monitor hotplug adds and removes instances in place, no generation swap, since plugging in a
   monitor is not a config edit: the declared surface set doesn't change, only how many instances a
   `monitor = "All"` declaration expands to.
4. **`surface` gains `namespace`, `keyboard_interactivity`, and `margin`.** `namespace` is the
   layer-shell namespace string compositor rules key off (e.g. Hyprland's `layerrule`); hardcoded
   per role today, it blocks per-panel compositor rules. `keyboard_interactivity`
   (`None`/`OnDemand`/`Exclusive`) is required for any surface that must take typing, such as a
   launcher. `margin` is the anchor offset, needed by a panel inset from a screen edge and not
   obtainable from padding.
5. **Input regions stay per surface.** The existing bounding-box union is generalized, not deleted:
   it applies to any surface whose visible content is smaller than the surface itself.

Rejected: keep the fixed roles, host every popup inside `overlay_canvas` (the shipped model). One
shared surface cannot give per-panel namespace, per-panel `keyboard_interactivity`, correct
paint-order layering (the same problem that already forced ADR-0007), per-output content, or a
per-panel exclusive zone. The "zero-overhead footprint" counter-argument doesn't hold in practice:
both checked reference toolkits (Quickshell, ashell) create layer surfaces at runtime with no
reported cost, and a `wl_surface` plus `zwlr_layer_surface_v1` is one roundtrip.

Not built at the time, later reversed: xdg-shell toplevels and xdg-popup were deferred as out of
scope, then built as ADR-0040's `window` and `popup` roles (the popup cost estimate had been too
high; PBA's staging generalizes to xdg-shell with no special case). Click-outside-to-dismiss was
recorded as having no compositor-agnostic mechanism; wrong, `xdg_popup.grab` is exactly that
(ADR-0040 decision 2). Session-lock surfaces were recorded as staying Supervisor-owned so a Renderer
crash can't drop the lock; wrong, `ext-session-lock-v1` keeps the compositor locked independent of
the lock client's life, and a locked session hides every non-lock surface anyway, so the original
plan to keep painting the Lua lock UI from the Renderer could never have worked (ADR-0042).

This settles the model, not the delivery: it cannot be implemented until the Renderer's scene and
its Wayland objects share one thread (ADR-0039).

## 0039. The Lua VM, retained scene, and paint pass share the Wayland dispatch thread

Decided that the Lua VM, the `Loader`, the retained `Scene`, and the paint pass all move onto the
Wayland dispatch thread; the former socket thread is demoted to framed I/O only (reads
`SupervisorFrame`s and forwards them over a channel, writes outbound frames it receives over
another). `mlua::Lua` is `!Send` and must be built on the thread that runs it, and `Scene`'s nodes
hold `mlua::Value` properties so `Scene` is also `!Send`; painting the retained scene therefore
requires the scene and the VM on the same thread as the GL context, which is the Wayland thread.
This is a hard constraint, not a preference.

Amended: decision 4 was deferred to Phase 20, and its stated reason was wrong. Consolidating threads
makes real per-surface sizes reachable but not attributable, since `Scene` keys surfaces by the
config's own `id` while `wayland::mod` derives `TrackedSurface::surface_id` from
`SurfaceRole::label()`, two id spaces that don't intersect; deleting `SurfaceRole` (ADR-0038) is
what unifies them, so decision 4 lands with that work instead. Decisions 1, 2, 3, and 5 are
unaffected; 1 through 3 shipped with the refactor. Also amended: the Consequences section's claim
that ADR-0021's 5ms CPU cap "is enforced, not merely measured" was not true when written. The
`Lua::set_hook` abort raises an ordinary Lua error a `pcall` inside the closure can catch, and
`set_hook` installs per Lua thread so a closure running inside a coroutine is never hooked at all;
both gaps were found in review and are being closed. The other two bounds below, shaping staying
off-thread and full evaluation happening only on config edit, are unaffected.

1. `Loader`, the live-signal map, rescue state, and `Scene` are constructed inside `wayland::run`
   rather than the socket thread; a move, not a hand-off.
2. The four PBA channels collapse to two: `ready_tx`/`presented_tx`/`activate_tx` become direct
   calls, since evaluation and buffer-commit are now the same loop; `secure_submit_tx` becomes an
   outbound-frame send.
3. One `ShapingHandle` survives, shared by content-sizing and painting (was two, one per thread,
   each paying `FontSystem::new()`'s roughly one-second startup). Shaping itself stays off-thread as
   a real bounded cost.
4. `PLACEHOLDER_OUTPUT_SIZE` is deleted; layout resolves against each surface's real configured
   size. (Deferred to Phase 20, see amendment above.)
5. `overlay_input_regions` gets its production caller, per ADR-0038 decision 5.

The cost is real: a slow `shell.lua` evaluation now blocks Wayland dispatch instead of stalling only
its own thread. It is bounded by the CPU-cap hook (see amendment on its actual enforcement gap), by
text shaping staying off-thread, and by full re-evaluation happening only on config edit, which a
`StateSnapshot` push does not trigger (ADR-0029). Both reference toolkits accept the same trade:
Quickshell runs QML on Qt's own GUI/scenegraph thread, and Noctalia v5 dropped Qt specifically to
own its whole stack, event loop and rendering, on one thread.

Rejected: keep the two-thread split, ship resolved trees over a channel. Rejected because it pays a
crossing on every feature, in both directions, forever: a pointer click would cross Wayland thread
to socket thread to Lua closure to re-evaluation to snapshot to Wayland thread, and ADR-0038's
surface creation would become a request/response protocol instead of a method call. The snapshot
itself isn't free either, since it must drop or resolve `mlua::Value` properties, reopening the
cache-invalidation problem ADR-0023 item 2 deferred. The thread boundary wasn't protecting anything:
a Lua evaluation failure was already caught and routed to rescue in-process.

Rejected: move EGL and paint onto the socket thread instead. Worse in practice, since Wayland
dispatch and EGL surface lifetime are coupled through `WlEglSurface`, whose `configure` events
arrive on the dispatch queue; splitting them relocates the same defect instead of removing it.

Scope: this ADR settles where the state lives, not the paint pass, surface manager, or input
dispatch, which it unblocks but does not specify. Landing the refactor changes no observable
behavior, same hardcoded surfaces and PBA handshake, on fewer threads.

## 0040. Four surface roles: panel, window, popup, lock

ADR-0038 had recorded xdg-shell toplevels and xdg-popup as non-goals. That scope was set aside: the
target became Quickshell's freedom (floating windows, panels, popups, session lock, per-screen
variants), so this ADR replaces those two non-goals with a design of four Lua-facing constructors
matching Wayland's own four surface roles.

1. **Four constructors, not one `surface` with a `kind` field.** `panel` -> `zwlr_layer_surface_v1`,
   `window` -> `xdg_toplevel`, `popup` -> `xdg_popup`, `lock` -> `ext_session_lock_surface_v1`,
   mirroring the protocol directly. A single schema with a `kind` discriminant was rejected: the
   property sets are mostly disjoint, so it would accept `layer` on a toplevel or `title` on a layer
   surface, with validation reduced to a per-kind allowlist; four constructors give each role an
   honest schema `layout::node` validates directly (Quickshell reached the same shape with four
   separate window types). The old `surface` constructor is renamed to `panel`; `surface` stops
   being Lua-callable and becomes the umbrella concept (a declared `wl_surface` plus its role).
   Keeping `surface` as an alias was rejected: no users to migrate, and an alias would blur the
   umbrella term against one of its own four members.
2. **Popups parent to a panel or a window, and take a real grab.** A popup is created the same way
   regardless of parent (`create_positioner`, then `get_popup` with a null parent) and is rooted
   under an `xdg_surface` or a layer surface before its first commit; a bar's dropdown is a
   first-class `xdg_popup` with full `configure`/`popup_done`/`grab`, not a second layer surface
   with hand-computed coordinates. This corrects ADR-0038, which recorded click-outside-dismissal as
   having no compositor-agnostic answer (true for layer surfaces, false for popups):
   `xdg_popup.grab` gives the grabbing popup keyboard focus and delivers `popup_done` on
   outside-click, keyboard dismissal, or screen lock. Three spec constraints bind the
   implementation: a denied grab is a normal outcome (`popup_done` arriving immediately after `grab`
   is expected, not an error); grab must answer a real input event and be requested before the popup
   maps, or the protocol raises `invalid_grab`, making input routing a hard prerequisite for popups;
   nested popups are destroyed in reverse creation order, owned by the engine, not the config.
3. **A popup's anchor rect comes from the click that opened it.** `xdg_positioner` requires a
   non-zero size and non-zero anchor rect (`get_popup` otherwise raises `invalid_positioner`), in
   parent-surface-relative coordinates, which is exactly the space a resolved node already lives in.
   `button`'s `on_click` gains an argument carrying the clicked node's resolved rect, passed
   straight to `anchor_rect`; no new node-identity concept is needed, and it matches how ashell
   derives menu positions from the triggering button's on-screen rect. `set_constraint_adjustment`
   defaults to `flip_y | slide_x` rather than the spec default of `none` (no repositioning),
   matching what a config author expects from a dropdown; the raw bitfield is still available for
   configs that want to be specific. Spec precedence is fixed: flip, then slide, then resize.
4. **Floating windows reuse the staging discipline already built.** `xdg_toplevel`'s initial-commit
   rule (commit with no buffer, wait for `configure`, ack, then attach) is the same one layer-shell
   already implements almost verbatim, so PBA's null-buffer staging generalizes across all four
   roles with no protocol-specific special case. What differs: `xdg_toplevel`'s configure carries a
   state array (`maximized`, `fullscreen`, `resizing`, `activated`, `tiled_*`) layer-shell has no
   analogue for, and its ack goes through the wrapping `xdg_surface`, not the role object;
   `set_min_size`/`set_max_size` are advisory, a fullscreen configure is binding. `window` gets
   `title`, `app_id`, `min_size`, `max_size`, and `on_close`, a Lua callback that may decline, since
   `xdg_toplevel.close` is a request the client may ignore. Decorations are not built: Oblisk
   requests server-side decoration via `zxdg_decoration_manager_v1` and accepts whatever mode it
   gets, with no client-side titlebar frame; revisit only if a window genuinely wants a system
   titlebar.
5. **`smithay-client-toolkit` covers this, with one escape hatch.** SCTK 0.21.1 wraps `XdgShell`,
   `Window`/`WindowHandler`, `Popup`/`PopupHandler`, `XdgPositioner`, and `LayerSurface::get_popup`,
   including the null-parent path this design uses. The one gap is `xdg_popup.grab`, which SCTK does
   not wrap: reached directly via `popup.xdg_popup().grab(seat, serial)`, the engine owning the grab
   bookkeeping, the same shape as ADR-0009's `wp-text-input-v3` handling (use SCTK where it wraps,
   reach through where it doesn't, per ADR-0008).

Scope: `lock`'s role is named here; which process holds `ext_session_lock_v1` is a separate question
revisiting ADR-0010. All four roles need the paint pass and the Lua-declared surface manager
(ADR-0039) before any can receive content; `popup` additionally needs input routing for its grab and
anchor rect. The reload model is unchanged: adding or removing a declared surface of any role is a
topology change, and per-role instancing follows ADR-0038 decision 3.

## 0041. `oblisk.screens` is Renderer-sourced; variants are a Lua loop

Quickshell's per-monitor idiom (`Variants { model: Quickshell.screens; PanelWindow { screen:
modelData } }`) splits into two separate questions: how to repeat a surface per screen, and where
the screen list comes from. Only the second needs anything built.

1. **No `variants` primitive; Lua already has `for`.** QML needs `Variants`/`Repeater` because a
   declarative markup language has no other way to say "one of these per element." A plain Lua `for`
   loop over `oblisk.screens:get()` building a table of `panel {}` nodes does the same job; adding a
   `variants` constructor would wrap a language feature the config language already has. Do not
   re-propose a repeater primitive; a future need here would be about reload identity (decision 3),
   not iteration.
2. **`oblisk.screens` is a Renderer-local signal, not a Supervisor capability.** It carries what
   `wl_output` reports per connected output: `name` (connector, e.g. `"DP-1"`), `width`, `height`,
   `scale`, `refresh`, reactive to outputs appearing and disappearing. It is sourced from
   `smithay_client_toolkit`'s `OutputState`, which the Renderer already maintains and already needs
   for layout. This is a deliberate exception to ADR-0037's shape: every other Lua signal is a
   capability the Supervisor owns, pushes as a `StateSnapshot`, and lists in `shared::CAPABILITIES`;
   `screens` is a Renderer-local global seeded at VM construction and updated from output events on
   the same thread instead. Routing it through the Supervisor was rejected: the Supervisor's own
   Wayland connection exists only for idle-notify and lock authority (ADR-0010) and binds no
   outputs, and making it bind `wl_output` and push snapshots would add a process hop plus a second
   source of truth for geometry the Renderer must hold anyway to lay out against. ADR-0039 removes
   the only reason this was ever awkward, since once the Lua VM shares the Wayland thread,
   `OutputState` is a local read.
3. **Identity is the `id` set, and it decides swap versus in-place.** `monitor = "All"` declares one
   surface, which the engine expands to one instance per output (ADR-0038 decision 3); hotplug
   changes the instance set, not the declared `id` set, so it is handled in place with no generation
   swap. An explicit Lua loop declares N surfaces with N distinct ids; hotplug then changes the id
   set, which is a topology change and therefore a generation swap (ADR-0001), a correct
   classification since the config genuinely declares different surfaces before and after. Rule for
   config authors: use `monitor = "All"` when every screen gets the same panel, and a loop when
   screens get genuinely different content and a swap on hotplug is acceptable.
4. **A hotplug reload reuses the file-edit reload path exactly.** A config looping over
   `oblisk.screens` must re-evaluate when that list changes, or its per-screen panels go stale; this
   reuses ADR-0024's existing machinery rather than adding a second reload path. On an output change
   the Renderer re-evaluates, diffs its own topology, and reports
   `Unchanged`/`TopologyChanged`/`Failed` to the Supervisor exactly as it does for a `Reevaluate`
   frame, and the Supervisor stays the one authority deciding in-place versus swap. The only new
   thing is the trigger: `inotify` on the config directory is one, a `wl_output` change is now
   another. Rollback, rescue, and the topology diff are unchanged.

Consequences: the IDL's `workspaces.outputs` currently duplicates `name`/`width`/`height`/`scale`
from a worse source, the still-undesigned compositor workspace adaptor, rather than from
`wl_output`, which would leave the Renderer laying out against one copy while Lua reads another.
Split by what actually knows the answer: `oblisk.screens` owns geometry, scale, and connector names;
`oblisk.workspaces` keeps workspace state and refers to screens by `name` instead of restating their
geometry. Popups need `oblisk.screens` too, since an `xdg_positioner`'s constraint adjustment is
resolved by the compositor against the output the popup lands on, so a config positioning its own
popups needs to know which screen it is on.

## 0042. The Renderer holds `ext_session_lock_v1`; the Supervisor supervises the lock client

Supersedes ADR-0010's session-lock half (its idle-notify half stands unchanged). The Renderer holds
`ext_session_lock_v1` and creates one `ext_session_lock_surface_v1` per output; `lock` is the fourth
surface role in ADR-0040, so the lock screen is an ordinary Lua-authored node tree.

ADR-0010 put the lock in the Supervisor to survive a Renderer crash, but `ext-session-lock-v1`
already guarantees fail-secure: the compositor must not unlock when the client dies, so that risk
does not exist for a real lock client. The decisive problem is that ADR-0010's design cannot render
a Lua lock screen at all: the `locked` event hides all normal (layer-shell) content, and
`get_lock_surface` is scoped to the connection that holds the lock, so the Supervisor cannot hand a
lock surface to the Renderer. The process that holds the lock is the process that paints it.

The Supervisor keeps everything except the protocol object: it owns `ext_idle_notifier_v1` and
decides when to lock (ADR-0010's idle half), issues the lock command, and, since it already tracks
and reaps generations, detects and respawns a dead lock client.

Authentication composes from existing pieces: the lock screen's `textfield` uses `secure_submit`, so
keystrokes go into `shared::SecureBuffer` and never through Lua (ADR-0005, ADR-0027); the buffer
crosses the control socket to the Supervisor, which runs PAM in its re-exec'd worker (ADR-0028); on
success the Supervisor tells the Renderer to unlock and it calls `unlock_and_destroy`.

Constraints: only one client may hold a session lock, so no generation swap can happen while locked
(PBA's overlapping-generation handoff is blocked; a config edit queues until unlock; in-place
reloads still work). Lock surfaces must track outputs (reusing ADR-0041's `oblisk.screens`); a
second surface on one output is a `duplicate_output` error, and destroying a lock surface while its
output is still active makes the compositor fall back to a solid color. `finished` means two
different things and neither may be swallowed: on the initial `lock` request it means denial
(surfaced via `oblisk.rescue`); later it means the compositor tore the lock down itself.
`unlock_and_destroy` must only be called after successful authentication; a client that wants to
exit right after unlocking must `wl_display.sync` first.

If the Renderer dies while locked, the session stays locked under the compositor's own fallback;
whether a respawned Renderer can retake the lock is compositor policy (Hyprland gates it behind
`misc:allow_session_lock_restore`, off by default), so recovery is not portable. ADR-0010's
`smithay-client-toolkit` `session_lock` dependency moves from the Supervisor to the Renderer; the
Supervisor keeps its own Wayland connection for idle-notify alone.

## 0043. Memory budget: declared fonts, atlas eviction, and PSS as the measurement

Noctalia's own published numbers are qualitative and use no stated measurement method, so they are a
direction, not a reproducible benchmark. Oblisk adopts its own falsifiable target instead: **50 MB
PSS per monitor for the shell's own processes at steady state**, matching the order of magnitude
Noctalia claims.

1. **Measure PSS, not RSS, and report three numbers.** Summing RSS across the supervisor and
   one-or-more-renderer processes double-counts shared pages (libc, GPU driver objects, mapped
   fonts, and, during a PBA handoff, two renderers running the same binary), overstating pressure.
   1. Steady state: sum PSS across supervisor and renderer via `/proc/[pid]/smaps_rollup`; this is
      the number compared against the 50 MB/monitor budget.
   2. Per-renderer USS (`Private_Clean + Private_Dirty`): what one generation uniquely costs.
   3. Handoff peak, sampled only in the window where two renderers are alive, reported separately
      from steady state, never folded into it.
   GPU memory is excluded from all three; read it from DRM fdinfo (`/proc/[pid]/fdinfo/*`), not
   `smaps`, since EGL/dmabuf buffers are GEM objects invisible to `VmRSS` on a real GPU (though not
   under llvmpipe, where they land in RSS as ordinary heap).

2. **Fonts are declared in config, not discovered from the system.**
   `cosmic_text::FontSystem::new()` eagerly parses metadata for every system font (commonly 1000+
   faces; ADR-0023 item 8 measured this at roughly one second) with no API to scope it. Built: a
   global `fonts { ui = ..., mono = ..., fallback = {...} }` table, recorded into `Lua::app_data`
   and installed by `ShapingHandle::set_chain` before first paint; the renderer loads only the
   declared families (roughly 10 to 20 faces) plus fallback, never calling `load_system_fonts()`.
   Read once at startup; editing it live requires a restart (`renderer/src/lua/fonts.rs`). Per-node
   `font_family` is deliberately not built, since both shaping and paint fall back per glyph across
   one chain, and a node-level override would reintroduce the shaping/paint face-disagreement hazard
   `text::shaping` already documents. Cost: an undeclared codepoint renders as tofu, which is why
   the default fallback chain covers CJK and emoji. Upgrade path (not built): build the system
   database lazily on a shaping miss, once a real config needs it.

3. **The glyph atlas needs eviction, because femtovg has none.** Atlas pages are 512x512 RGBA8 (1
   MiB each, not the 2048x2048 an earlier note claimed), held in an unbounded `Vec`, freed only on
   an explicit `clear()`. Decision: clear the whole atlas (not an LRU, since femtovg exposes no
   per-glyph eviction) when it exceeds a page-count threshold and the shell is idle, rebuilding on
   demand.

4. **Per-surface buffers are why the dynamic surface model helps here.** One RGBA8 buffer at
   2560x1440 is about 14 MiB; double/triple-buffered, 28 to 42 MiB per surface, scaling with surface
   area not surface count. ADR-0038/ADR-0040's per-popup and tightly-sized-panel surfaces beat the
   old permanently-mapped fullscreen `overlay_canvas`, so the generality decision and the memory
   target point the same way.

Non-goal: no allocator swap, arena, or `jemalloc` until decision 1's measurement exists and says
where the memory is.

Amendments from Phase 24's first real measurement (niri, one 1920x1200 output, i915, Mesa 26.2.1),
none changing the decisions: DRM fdinfo field is `drm-resident-<region>` (i915), not `drm-*-memory`;
a DRM client's several fds repeat identical byte counts (one case: three fds each reporting 279968
KiB), so the harness keys on `(drm-pdev, drm-client-id)` to avoid counting one client three times;
the Supervisor cannot count monitors (ADR-0041 keeps `screens` Renderer-sourced), so the harness
reports absolute totals and leaves division to the reader. The handoff sample is unconditional,
taken once at the widest point of the window (after `run_pba` returns `Ok`, before the superseded
generation is reaped); the steady-state sample stays opt-in behind `OBLISK_MEMORY_SAMPLE_SECS`.

First reading: steady state totalled **149.7 MiB PSS** (supervisor 12.6 MiB; generation 0 pss 137.0
MiB, uss 130.4 MiB, gpu 14.3 MiB) against the 50 MB/monitor budget, roughly 3x over. Handoff
totalled 196.9 MiB (generation 0 pss 92.2 MiB uss 42.6 MiB gpu 14.3 MiB; generation 1 pss 92.1 MiB
uss 42.4 MiB gpu 14.5 MiB) -- 1.32x for two renderers, not 2x, vindicating PSS (a naive RSS sum
would show roughly 274 MiB). Attribution: 82.3 MiB `libLLVM.so` (59% of the renderer's PSS, 55% of
the whole shell, pulled in by Mesa's gallium megadriver even though this machine renders on i915
hardware, not llvmpipe), 25.0 MiB anonymous, 13.3 MiB `libgallium`, 12.4 MiB heap, 4.2 MiB the
renderer binary, 0.1 MiB every mapped font. Decision 2 was right by four orders of magnitude more
than this ADR estimated: the declared chain costs 137.0 MiB versus **2207.9 MiB** for
`load_system_fonts()`'s full system set (2648 faces on this machine), a 16x multiplier on the whole
shell; that also exceeds the 1.1 GB of font files on disk, showing `fontdb` 0.23 loads file contents
rather than memory-mapping them as this ADR originally assumed. The `system_fallback` path (taken
when fontconfig is unreachable) is shipped code that now measures at 2.2 GB, a live hazard this ADR
surfaces but leaves for Phase 19 to bound. Decision 3's atlas eviction is untested by this reading,
since unbounded growth is a days-long leak invisible seconds after boot. Open question, left
unsettled: Oblisk's own pages (binary, heap, anonymous, fonts) come to roughly 41.7 MiB in the
renderer plus 12.6 MiB in the supervisor; whether the 50 MB target should exclude the driver library
is not decided here.

## 0044. Signals resolve at layout time, and a push marks the scene dirty

Before this decision nothing connected a capability's pushed state to the screen:
`apply_state_snapshot` only updated the handle, ADR-0039 confirmed re-evaluation runs only on config
edit, and every property parser called `reject_signal`, contradicting the IDL's own
`string/Signal`-style type unions. Signals now resolve at layout time instead of evaluation time,
and a push marks the scene dirty so it re-resolves without re-running Lua.

1. **Property parsers resolve a `Signal` instead of rejecting it.** `reject_signal` is removed; a
   parser finding `Value::UserData` holding a `Signal` calls `get()` and parses the result under the
   same rules as a literal, making `marshal.rs`'s `check_number`/`check_integer`/`check_string`
   load-bearing for the first time. `:get()` inside `shell.lua` still reads once at evaluation time
   and never updates; passing the handle itself is what opts into reactivity. Carve-out: the
   `SurfaceTopology` fields keep rejecting a `Signal`, because they are computed at evaluation time
   so `handle_reevaluate` can diff them against `applied_topology` to choose a generation swap
   versus an in-place reload (ADR-0001); a signal there would resolve once for that comparison and
   then drift under the live generation. A signal
   resolving to `nil` means the property is absent, so the parser's own default applies instead of
   erroring on `nil` (a Lua table cannot itself hold `nil`, so this is the only way a signal-bound
   property can be explicitly absent); every rostered signal reads `nil` until its first
   `StateSnapshot`, and `run_startup_evaluation` runs before then, so without this rule a config
   binding a bare capability signal cannot boot.

2. **`LiveSignalHandle::set` marks the scene dirty; the dirty bit re-resolves, it does not
   re-evaluate.** A push sets one flag. On the next loop turn a dirty scene re-runs `Scene::apply`
   against the retained `LoadOutput` from the last evaluation; `shell.lua` does not run. The
   retained `VirtualNode` tree still holds the `Signal` handles Lua put into it, so re-applying
   reads current values through decision 1; reconciliation matches by position and preserves node
   identity and leases (ADR-0023 §4). This is also the missing input to frame gating: a surface
   whose re-resolve produces different geometry or paint properties needs a frame, one that produces
   an identical result does not. `ponytail:` one flag covers the whole scene, so any push
   re-resolves every surface; the ceiling is many surfaces plus a high-frequency capability where
   most surfaces don't reference it. The upgrade path is per-surface flags, which needs the
   dependency graph decision 3 declines to build.

3. **No memoization and no dependency graph.** `computed` and `map` keep recomputing on every read
   (ADR-0021); the dirty bit is a single boolean, not an invalidation set. This is the correct
   baseline, not a placeholder: the graph buys skipping work that a 5ms-capped evaluation and a
   per-surface layout pass already finish in well under a frame, and it should be built only when a
   profile names the re-resolve as the cost. Memoization keyed on signal identity would not help
   regardless: `SignalKind::Computed` holds `deps: Vec<Signal>` by value and `Signal` derives
   `Clone`, so `computed({s, s}, f)` embeds two independent copies of `s` instead of referencing it
   twice; twenty levels of that build a 2^20-node tree evaluated as 1,048,575 closure calls.
   ADR-0021's 5ms cap, governing a whole evaluation rather than each leaf call, is what keeps that
   survivable: the config gets an error instead of a wedged shell, measured firing after roughly
   3,200 calls. A cache would need `Signal` to become a shared reference (`Rc`) before any shared
   identity is left to key on.

4. **A generation's Lua VM outlives its retained scene, so an in-place reload does not reset it.**
   `ResolvedNode::properties`, `RetainedNode::properties`, and decision 2's retained `LoadOutput`
   are all `HashMap<String, mlua::Value>`, and every value pins the `Lua` that created it.
   `handle_reevaluate` calls `evaluate_file` on the same `Loader`; it does not reset the VM. One VM
   per generation, created once and dropped only when the generation ends; resetting the VM is a
   generation swap's job, and a swap gets a new process anyway. The reason is that reading a
   retained value from a dead Lua state panics, since `ValueRef::to_pointer` locks the state, which
   makes struct field order load-bearing: the `Loader` must be declared after anything holding
   values derived from it, so Rust drops those fields first. (Not, as first stated, that a reset
   would leak the old VM by refcount: mlua 0.12's `ValueRef` holds a `WeakLua`, not a strong
   reference, so no such leak exists.)

5. **Lua-authored state is named, and the name is what survives a reload.** Live signals are
   read-only to Lua, so a config needs somewhere to keep runtime UI state nothing else watches.
   `state(name, initial)` returns a writable `Signal` whose `:set()` marks dirty through decision 2.
   A generation holds a `name -> Signal` map that outlives any evaluation; `state` returns the
   existing signal when the name is already present and ignores `initial`, so an in-place reload
   finds the same signal holding the same value and an open dropdown stays open across a save. Named
   state dies on a generation swap, which is accepted: the map lives in the process being reaped,
   and the upgrade path, if it matters, is serializing it into the PBA handshake. Amendment (found
   by the wallpaper case, `state("wallpaper", path)` hand-edited in the config file and expected to
   take effect): "a name already in the map wins" was too broad. A changed literal re-seeds the
   signal; a value the config file doesn't touch keeps the live value. The registry stores the
   literal it last seeded from; on re-evaluation, a new `initial` that differs from the stored one
   means the author edited the file, so it is adopted (through the same `:set()` path, marking
   dirty); one that matches keeps whatever the signal holds now, including a runtime write. Scalars
   only: `mlua` compares tables by pointer and every evaluation builds a fresh table, so a table
   `initial` (e.g. `popup_anchor`'s default) always keeps the original behavior rather than being
   treated as changed. The rule assumes the literal is stable across evaluations; `state("t",
   os.time())` would re-seed on every reload, which is an unrelated config bug.

Rejected: re-evaluating `shell.lua` on every push. It puts a full Lua run in the path of every
capability update, retracting ADR-0039's premise that full re-evaluation is rare; it is also wrong
on identity, since a fresh evaluation gives every `on_click` closure a new identity, so the retained
scene would reconcile against a tree that differs everywhere instead of only where state changed.

Not built: `list` stays deferred. A `Signal` in `source` resolves under decision 1, but expanding it
through `itemfn` is Lua execution during a resolve rather than during an evaluation, and
`children_of` does not handle `list` yet (ADR-0023 item 1 still owns it).

## 0045. Nodes reconcile by scoped `id`, and `list` items by `key`

ADR-0023 §4 matched a freshly evaluated node to its retained counterpart by position among its
parent's children, which is correct only while child order never changes: inserting one node shifts
every sibling below it, so leases and (once ADR-0044's named state exists) state transfer to the
wrong subtree. Nodes now reconcile by an optional, per-parent-scoped `id`, and `list` items by a
`key` function.

1. **Any node may carry an `id`, scoped to its parent.** `id` becomes a base property in IDL §5.1,
   available on every node kind, not only top-level surfaces. It is a reconciliation hint only: not
   unique across the whole tree, not addressable from Lua, no effect on layout or paint. Scoping is
   per parent, not global, so a reusable component (ADR-0047) can use internal ids and still be
   instantiated twice under different parents. A duplicate `id` among siblings is a `LayoutError`
   routed to rescue, not a warning, since the tree is already validated on every resolve.

2. **Identified children pair first, then the rest pair by position.** Within one parent, match
   every fresh child that has an `id` to the retained child with the same `id`; match the remaining
   fresh children to the remaining retained children by order among themselves (ADR-0023's original
   rule, applied to the leftover set). This degrades to today's behavior when a config uses no ids,
   and lets ids be added only to the few nodes that hold a lease or named state. The matching rule
   is exact and direction-matters: an `id` means "the same node, and only the same node," in both
   directions. A fresh child with an `id` matches only a retained child with that same `id` (or is
   new, never drawn from the positional pool); a fresh child with no `id` matches only a retained
   child with no `id`; everything unclaimed is retired child-first. The first implementation read
   "match the remaining fresh children to the remaining retained children" the wrong way: it let an
   unmatched identified child adopt whatever positional retained node was next, and let an anonymous
   child inherit a node that had declared an explicit `id`. Measured case: retained `[a, b, c]`
   against fresh `[b, c, d]` produced `[3, 4, 2]` with nothing retired, so `d` silently inherited
   `a`'s node and subtree, making a declared `id` a weaker guarantee than declaring none.

3. **`list` takes a `key` function, and a duplicate key is an error.** `key` is a Lua function from
   a `source` element to a string, called on the element itself rather than on the node `itemfn`
   builds, so a key is computable without building anything; items reconcile by key. One shape, not
   two: unlike Quickshell's `objectProp` (a property name, which can't key a list of plain strings),
   a function covers both cases and is cheap enough at expected list sizes (tens of items per
   resolve). Without `key`, items match by index and every item below an insertion rebuilds, a
   documented cost, not a rejected configuration. A duplicate key is an error surfaced through
   rescue, deliberately unlike `ScriptModel`, whose docs leave duplicate behavior undefined.

`list` itself stays deferred (ADR-0023 item 1, restated by ADR-0044): it is registered as a Lua
constructor in `NODE_KINDS` but rejected by `layout::scene::ensure_supported_kind`, so no config
reaches reconciliation through it yet, and `key` has nothing to attach to until it does. Top-level
surface ids are unchanged, already required and unique and already keying `Scene`'s `HashMap`;
surfaces are the root scope, so decision 1's per-parent rule starts one level down.

## 0046. Rescue renders out of band when no scene survives

`oblisk.rescue` (IDL §2.10) is a Lua signal a config reads and renders into its own tree, which
works only while the config works. When `shell.lua` fails to evaluate at startup there is no tree to
render through (ADR-0024 item 4: the shell stays blank), the same shape of mistake ADR-0042 found in
ADR-0010, where a presentation path depended on the thing that had just failed.

1. **Split the two failures, because only one of them is recoverable.** A reload failure leaves a
   working scene on screen: ADR-0024's rollback guarantee holds, the pre-edit config is still
   running, and `oblisk.rescue` is the right mechanism, unchanged. A startup failure leaves nothing,
   no prior scene and no Lua tree, and gets a separate out-of-band path. `oblisk.rescue` was never
   wrong; it was load-bearing for a case it structurally cannot cover.

2. **The Supervisor spawns a rescue process, which is not a generation.** On a startup evaluation
   failure with no prior scene, the Supervisor re-execs itself (following ADR-0028's PAM-worker
   mechanism) with a flag and the error text, into a process that binds one `Overlay` layer surface
   per output and draws the error in hardcoded Rust: no Lua VM, no config, no capability
   connections. It has no generation id, receives no dependency snapshots, takes no part in the PBA
   handshake, and holds no authority over any output; the Supervisor reaps it as soon as a real
   generation reaches presentation evidence.

3. **It shows the error, not a shell.** Error text, the file and line `mlua::Error` already carries,
   and the path it tried to load. No fallback bar, no default config, no recovery UI. The rescue
   process has no reason to grow: it is killed the moment a working config exists, so any feature
   added to it is a feature nobody sees while the shell works.

Rejected: a built-in default config to fall back to. It still needs a working Lua VM, loader, layout
pass, and paint pass to report that those are broken, so it fails at exactly the moments a fallback
matters most, and it risks making a broken shell look like it is working.

Rejected: exit with the error on stderr (near enough to current behavior). A shell launched from a
session file or a compositor `exec-once` has nothing attached to stderr, so the user's whole
experience is a screen that stays empty; the error is already in the journal, which is not the
notification.

No `inhibitReloadPopup` equivalent is needed: decision 1 routes the handleable case (reload failure)
to `oblisk.rescue` and never spawns a rescue process for it, so there is nothing to inhibit.

## 0047. The config is a directory, not a file

The loader and the watcher were reading and watching one `shell.lua`. The config becomes a
directory: `require` resolves inside it, requires are cleared and re-evaluated on reload, and the
whole tree is watched.

1. **`package.path` points at the config directory and nothing else.** Set it to the config
   directory's `?.lua` and `?/init.lua`, replacing mlua's default rather than prepending to it, so
   `require "widgets.clock"` never picks up a same-named module from the system Lua tree. Installed
   Lua libraries become unreachable by default; a user who wants luarocks gets a user-declared path
   list appended later, not the system default restored. mlua's safe mode already replaces the C
   searchers and makes `package.loadlib` raise, so no `Lua::unsafe_new` is needed for C modules, and
   none should be reached for.
2. **Clear `package.loaded` before every re-evaluation.** ADR-0044 decision 4 keeps one Lua VM per
   generation without resetting it on reload, and `require` caches by module name in
   `package.loaded`. Without clearing, editing a required module re-runs `shell.lua` against the old
   cached module and changes nothing on screen, indistinguishable from a reload that silently failed
   to happen.
3. **Watch the directory tree, and gate on a content hash.** Watch the config directory recursively
   instead of one filename, keep a path-to-hash map refreshed on every evaluation, and drop any
   inotify event whose file hashes the same as last time (editors write-truncate-rewrite and produce
   swap-file churn that debouncing alone does not filter). Tracking which files `require` actually
   loaded was rejected: that set is only known after a successful evaluation, so a broken first
   config would leave nothing watched and no way to recover by editing. Only `.lua` files trigger a
   reload.

Lua's own module cache gives per-module singletons for free, unlike Quickshell's
`Singleton`/`SingletonRegistry`; decision 2 keeps that behavior correct across reloads. ADR-0045's
per-parent `id` scoping is what makes a required module reusable across multiple instantiations
without id collisions.

## 0048. The config VM drops the blocking parts of the Lua stdlib

mlua's default `StdLib::ALL_SAFE` gives a config `io` and `os` in full, including `io.open`,
`os.execute`, and `os.exit`. Since ADR-0039 put the Lua VM on the Wayland thread, any of those calls
blocks every surface on every monitor until it returns, and ADR-0021's 5ms CPU cap does not catch it
because a thread parked in a blocking syscall executes no instructions. This is not about malicious
configs; it is about a one-line mistake stalling the compositor's frame loop.

Construct the VM with an explicit `StdLib` set instead of `ALL_SAFE`, dropping `IO` and `OS`
entirely, then re-register only the four `os` functions that read process-local state and return
immediately: `os.time`, `os.date`, `os.clock`, `os.getenv`. Everything else in `os` (`execute`,
`exit`, `remove`, `rename`, `tmpname`, `setlocale`) and all of `io` is refused. `debug` and `ffi`
were already absent under `ALL_SAFE` and stay absent; `coroutine`, `string`, `table`, `math`, and
`utf8` are untouched because none of them block.

`process.run` (ADR-0018, ADR-0026) is the supported way to run a command: non-blocking,
callback-delivered, envelope-guarded, generation-stamped, reaped by the Supervisor. `os.execute`
would give a config a second way that bypasses all of that and blocks the frame loop besides.
`os.exit` is refused because it terminates the Renderer mid-generation with no PBA teardown, which
the Supervisor would see and treat as a crash.

This makes reading a file from Lua impossible, a real loss for theme files and cached tokens. The
loss is accepted for now: the ten capabilities cover the hardware state a shell reads, `require`
(ADR-0047) covers loading Lua data files, and `process.run "cat"` covers the rest adequately. A
future non-blocking `oblisk.read_file` returning through the same callback path as `process.run` is
the intended fix; building it now would add a second file-reading mechanism before the first has a
caller.

Rejected: keep the stdlib and document the hazard, because a config that blocks for 40ms produces no
error and no log, and reads as compositor or driver stutter rather than as the two-line function
that caused it.

Rejected: put Lua back on its own thread to contain the blocking calls, because ADR-0039 weighed
that exact cost deliberately and took it; reintroducing a thread boundary just to contain
`os.execute` is a bad trade against deleting `os.execute` in one line.

`Loader::new` calls `Lua::new_with(...)` with the explicit set and registers the four kept `os`
functions before `register_node_constructors`. `oblisk-idl-api-specs.md` section 1 states what the
config VM contains, since "Lua 5.4" alone no longer describes it.

## 0049. Popups and windows are created when shown, not at generation startup

ADR-0038 decision 2 ("a generation creates exactly the surfaces its own evaluation declared, once,
at startup") and ADR-0040's grab rule (a grab must answer a real input event and be requested before
mapping, or `xdg_popup` raises `invalid_grab`) contradict each other for popups: a popup created
once at startup and later unmapped cannot request a grab from an event that has not happened yet,
and `get_popup` consumes its positioner so it cannot re-anchor without `xdg_popup.reposition`, which
ADR-0040 deferred. The protocol decides this, not preference: popups are per-open objects.

1. **The role decides the lifetime.** `panel` surfaces exist for the generation's whole life,
   created at startup (ADR-0038 decision 2, correct for the only role that existed when it was
   written). `lock` exists while the session is locked (ADR-0042). `popup` and `window` exist only
   while shown. What a config declares is still fixed for a generation's life and adding or removing
   a declaration is still a topology change (ADR-0001); only the Wayland object's lifetime now
   differs from the declaration's for these two roles.
2. **`visible` creates and destroys, rather than mapping and unmapping.** When a `popup` or `window`
   node's `visible` resolves true, the engine creates the Wayland object; when false, it destroys
   it. This is driven by the existing re-resolve path (`on_click` writes named state per ADR-0044
   decision 5, the write marks the scene dirty, the dirty re-resolve reads `visible` as true and
   creates inside that pass), with no new machinery.
3. **Opening a popup is a value change, never a topology change.** The declared set that ADR-0001
   keys on does not change when a popup opens or closes, only when its declaration is added or
   removed. This keeps ADR-0001's split intact: a config with twenty popups that are never opened
   holds twenty retained-scene nodes and zero Wayland surfaces, buffers, or EGL surfaces, which
   matters against ADR-0043's 50 MB per-monitor budget.

Not built: `xdg_popup.reposition` (a popup following a moving anchor while open). Decision 2's
fresh-popup-per-open already covers a dropdown opening under different buttons; only an anchor
moving during an already-open popup's life would need it, and nothing needs that.

Destruction order: nested popups are destroyed in reverse creation order (ADR-0040); a parent popup
whose `visible` goes false destroys its children first.

Amended: decision 2's original mechanism was wrong about where the re-resolve runs.
`re_resolve_if_dirty` runs in the poll loop after `dispatch_pending` returns, not inside the
dispatch callback, so no serial is left on the stack by the time a popup is created. The actual
mechanism: a pointer press arms a serial field; the poll loop disarms it after re-resolve and apply
have run for that turn. A popup created by a click finds an armed serial; a popup created by
anything else (e.g. a D-Bus notification marking the scene dirty) finds none and its `grab = true`
is refused, and refused means not created at all, not created without a grab. The anchor rect still
reflects the button actually clicked because `on_click` receives that rect (ADR-0050 decision 3) and
writes it to a `state` signal the popup reads, via the config rather than off the dispatch stack.

Amended: a popup's `PopupSpec` is built from resolved properties, not from raw evaluation-time
`VirtualNode::properties`. `socket.rs`'s `panel_specs` parses raw properties, which is correct for
`panel` (whose topology fields like `layer` and `namespace` reject a `Signal` on purpose). A popup's
`anchor_rect` is meant to change with each click, so parsing it at evaluation time would freeze it
at the last reload. The authoritative spec is built from the resolved tree at the point the surface
is reconciled, after `resolve_properties` has run once for that pass (ADR-0044 decision 1);
evaluation-time parsing still validates literal properties early so typos land in ADR-0046's rescue
log rather than surfacing as an `xdg_positioner` protocol error at first open.

## 0050. Pointer hit-testing walks a path, a click is press-and-release on one node, and focus attributes the secret

`on_click` had been an inert property since ADR-0021 item 2; making it callable required deciding
hit-testing shape, click semantics, what a handler receives, and how keyboard focus is tracked.

1. **Hit-testing returns the path, not the topmost node.** Returning "the deepest visible node" is
   wrong for a `button` wrapping a `text`: the deepest node has no `on_click` and the button never
   fires. Hit-testing instead returns the whole chain, root-first and deepest-last; each caller
   (`on_click`, focus attribution) scans from the deep end for what it wants. Containment gates
   descent (a node whose rect excludes the point, and its children, are not entered), which is what
   makes the result a path. Bounds are half-open (`x <= point.x < x + width`) so two buttons sharing
   an edge cannot both claim it. Hitting and painting agree exactly: containment-gated descent makes
   a node's hittable region its intersection with every ancestor's rect, and `paint_node`'s scissor
   chain (added Phase 19 item 17) makes its painted region the same intersection, without either
   walk carrying an explicit clip rect, so a scroll offset or transform given to one walk must be
   given to the other in the same commit. `ResolvedNode::rect` is parent-relative, not
   surface-local; `layout::hit` exports `absolute_rect`, the sum along the path, for callers that
   need the absolute position, which is a second reason the return type is the whole chain rather
   than a node and a depth.
2. **A click is a press and a release on the same node.** Firing on press removes the ability to
   press, notice a mistake, and drag off before releasing, so a press arms and a release fires only
   if it lands on the same node, identified by the pair of surface instance id and the armed node's
   rect (a `ResolvedNode` carries no persistent identity). A re-resolve between press and release
   that moves the button cancels the click. Arms for `left`, `right`, and `middle` (evdev codes),
   not `left` only: refusing to hand the config the button is itself a policy the config cannot
   override, and real configs assign different actions to the right button routinely. `ArmedClick`
   also records which evdev code armed it, since a release must match surface, rect, and button, or
   a right-press-then-left-release could wrongly complete as a right click. An unhandled button code
   arms and fires nothing rather than mapping to a catch-all `"other"`, because a config handed
   `"other"` cannot distinguish two different unnamed buttons. Back/forward buttons are left out
   because their evdev-to-name mapping is ambiguous and unneeded so far.
3. **`on_click` receives the button's rect and, as a second argument, the button name as a string.**
   The callback signature is `function(rect, button)`, where `rect` is `{ x, y, width, height }` in
   the surface's logical coordinates and `button` is `"left"`, `"right"`, or `"middle"`. The rect
   travels from engine to config, not the reverse: "the popup's anchor rect comes from the rect
   on_click returns" (ADR-0040, ADR-0049) describes the round trip through the config (`on_click =
   function(rect) menu_anchor:set(rect) end`, with the popup's `anchor_rect` bound to that signal),
   not a Rust-side return value, since the engine cannot know which popup a given click was meant to
   open. `button` is a string, not a raw evdev code or a normalized small integer, matching every
   other categorical value crossing this boundary (`fit`, `layer`, `anchor`, `align_h`); it is a
   second argument rather than a fifth rect field, so a one-argument handler written before this
   addition keeps working unchanged. A handler that raises is logged and swallowed, since a broken
   `on_click` is a config bug and must not take down an otherwise-painting shell (ADR-0046 covers
   evaluation failures, not one misbehaving handler).
4. **Focus attributes the secret, and no focus means no frame.** A click whose path contains a
   `textfield` focuses that node, and the engine remembers that field's `secure_submit` `{
   capability, action }` (replacing the prior `PLACEHOLDER_SECURE_SUBMIT_CAPABILITY`/`"unknown"`
   stand-in, which addressed no real Supervisor capability and put a password on the wire for
   nobody). A completed `wp-text-input-v3` submit with no focused field sends nothing, rather than
   the old `"unknown"/"unknown"`; the buffer is zeroized either way. Focus clears on a click landing
   on no `textfield`, on `zwp_text_input_v3`'s `leave`, and on the keyboard leaving the surface: the
   field stops being focused the moment anything indicates the user is elsewhere. A right or middle
   press decides focus the same way a left press does (every toolkit agrees), and also arms the
   `xdg_popup.grab` serial and counts toward `pointer_input_count` (ADR-0049's amendment, ADR-0051
   decision 1), since both exist to answer "did real user input cause this" and a right-click
   qualifies.

Not built: click-outside-to-dismiss for a `panel`, because layer surfaces have no
compositor-agnostic grab; a `popup` does not have the problem, which is why one is used instead of a
second `panel`.

A handler written with no `button` parameter now also runs on right and middle clicks, where before
those events did nothing; a config that wants left-only behavior checks `if button ~= "left" then
return end` explicitly.

## 0051. A popup anchors to one parent instance, and a compositor dismissal latches

ADR-0049 settled when a popup's `xdg_popup` exists. Two things it left open block implementation:
which parent instance a popup roots under when its `parent` names a multi-monitor panel, and what
happens when the compositor, not the config, destroys the popup.

1. **A popup anchors to the parent instance the arming click landed on.** A `panel` with `monitor =
   "All"` expands per output (ADR-0038 decision 3), so a `parent` name can refer to several
   `zwlr_layer_surface_v1`s, but `xdg_surface.get_popup` takes exactly one parent. A popup does not
   expand per output: it is opened by one click on one monitor and belongs there, so its instance id
   is the bare declared `id` (like a `window`'s), and its parent is the instance whose surface the
   arming click was delivered to, read off the same record that already tracks the grab serial's
   originating surface. When no click armed it (a `grab = false` popup opened by, say, a D-Bus
   notification), the first instance of the named parent is used. Rejected: expanding a popup per
   parent instance (`click_menu@eDP-1`, `click_menu@DP-1`), because one shared `visible` signal
   would then open the dropdown on every monitor from a single click.
2. **A compositor dismissal latches until the user asks again.** `xdg_popup.popup_done` means the
   compositor already destroyed the object, most commonly from click-outside. The engine destroys
   its handle and fires `on_dismiss`, but cannot trust the config to write `visible = false`:
   without a latch, the resolved tree still reading `visible = true` would recreate the popup on the
   next re-resolve, which the same click-outside would dismiss again, forever, even for a config
   with no `on_dismiss` at all. So a dismissed popup's declaration is latched against recreation.
   The latch is keyed on pointer input, not on the value of `visible`: it records which
   pointer-input count was current at dismissal, and stays latched only while no further pointer
   input has arrived. Keying it on `visible` cycling false-then-true does not work in practice,
   because niri (and any compositor) delivers the click that closes a grabbed popup to the parent
   bar in the same event batch as `popup_done`, so `on_dismiss` writing false and the bar's
   `on_click` writing true both land before the engine's single end-of-turn sample of `visible`,
   which then only ever reads true and the false edge is never observed. This differs deliberately
   from `WindowHandler::request_close`: a `close` request the client may ignore, so the engine
   destroys nothing there, while `popup_done` means the object is already gone and the only question
   is whether an unasked-for replacement appears.
3. **A refused grab means no popup, not a popup without one.** `grab = true` with no armed serial
   (ADR-0049's amendment) means the popup is never created, logged once. A compositor-side grab
   denial reads the same way to the config (immediate `popup_done`, `on_dismiss` fires, treated as a
   normal outcome per section 6.3) and goes through decision 2 unchanged; nothing distinguishes it
   from click-outside on this side of the protocol.

Nested popups close in reverse creation order (ADR-0040); a parent's `popup_done` means the
compositor already destroyed its children, so the engine drops its handles in that order without
sending anything. A popup's surface instance exists from generation startup even though its
`xdg_popup` does not, same as a `window`, which is what lets `visible` be read off the resolved
tree; twenty declared popups still cost twenty retained nodes and zero Wayland objects. The latch
lives on the tracked surface and dies with the generation, so a PBA swap starts every popup
unlatched.

Not built: a popup shown on more than one monitor at once. A config that wants a dropdown on every
bar declares one popup per monitor and drives them separately.

Known limitation: a grabbing popup opened from `on_click` fails wlroots's
`wlr_seat_validate_pointer_grab_serial` (button count and serial are both stale by release time, per
ADR-0050 decision 2's press-then-release click), so it works on smithay-based niri but flashes open
and shut on sway or Hyprland. The fix needs an `on_press` hook in the IDL, which does not exist yet;
shipped as-is meanwhile.

## 0052. The session lock is commanded through a capability, and its surfaces live for the lock

ADR-0042 settled that the Renderer holds `ext_session_lock_v1` and the Supervisor supervises the
client, but left four things open: what triggers the lock command, where a lock screen is declared,
what happens with no declared lock screen, and which failures go where.

1. **`oblisk.lock` is an ordinary capability, with `lock` but no `unlock` action.** A config
   triggers it through ADR-0037's generic capability dispatch (`oblisk.lock:invoke("lock")`), the
   same mechanism every other write action uses; no new mechanism or Rust-side policy is invented.
   There is no `unlock` action: the lock screen's own node tree is Lua, its `button` callbacks run
   while locked, and an `unlock` action would put a one-click bypass of PAM on the very surface PAM
   guards. The unlock direction has exactly one caller by construction, the Supervisor's own
   `PamOutcome::Success` arm, making ADR-0042's "never call `unlock_and_destroy` except on a
   successful authentication" checkable by reading one match arm. Rejected: a Supervisor-owned lock
   timeout applied regardless of what the config asks for, because it would lock a session whose
   config never declared a lock screen, stranding the user at a black screen; decision 3 covers that
   case directly instead. Since Lua could not write at all yet, this decision pulls Phase 25 item 1
   (the generic `CommandEnvelope`-building method) forward into this phase; `expected_revision` is
   `0` because a lock command is not a read-modify-write. The capability is seeded as `oblisk.lock`
   rather than a bare `lock` global because section 6.4's `lock` node constructor already owns that
   bare name.
2. **A `lock` node is declared at the root of `shell.lua`, and its Wayland object's lifetime is the
   lock, not the declaration.** This reverses a Phase 22 rejection of a root-level `lock`. ADR-0049
   already separated where a declaration lives from when its Wayland object exists (for `window` and
   `popup`); `lock` gets the same treatment: one retained node from generation start, with no
   `ext_session_lock_surface_v1` until the compositor sends `locked`. `lock` joins `NODE_KINDS` as a
   fourth constructor and expands to one instance per output, the way `monitor = "All"` does for
   `panel`, because the protocol requires a lock surface on every currently-present output, not
   because a config chooses it. A `lock` node has no `visible`, `monitor`, `anchor`, or size
   property: the compositor alone decides when lock surfaces exist (created after `locked`,
   destroyed by `unlock_and_destroy`), so a `visible = false` on one would either be ignored or
   destroy a surface the compositor still expects, which is what makes the compositor "fall back to
   rendering a solid color" per ADR-0042; there is no useful reading of the property, so none is
   offered.
3. **A config with no working lock screen refuses the lock rather than acquiring it blind.**
   `oblisk.lock:invoke("lock")` against a config with no `lock` node, or a `lock` node that cannot
   reach PAM, is refused before `SessionLockState::lock` is ever called, and the session stays
   unlocked. Acquiring the lock and painting nothing would be a black screen with no password field
   and, since the protocol guarantees the compositor will not unlock on client death, no way out
   short of a VT switch; that is a denial of service, not fail-secure, since nothing was protected
   by a lock never taken. "Can reach PAM" means exactly one `secure_submit` field targeting
   `("lock", "authenticate")`, not "at least one": a lock surface must be typable the instant the
   compositor gives it keyboard focus with no pointer click, so the field that arms the keyboard
   must be identifiable without guessing, and two candidate fields would arm neither. An in-place
   reload that would delete the only such field from a currently-held lock screen is vetoed and
   rolled back, since a `child` edit is not topology and is normally left ungated (ADR-0042) but
   this is the one in-place edit that strands the session; restyling a live lock screen still works.
4. **Acquisition failures surface through `oblisk.rescue`; authentication failures surface through
   the capability's own state.** The split follows whether a lock screen is on the glass to read the
   message. A refused lock (decision 3), a denied lock request, an absent
   `ext_session_lock_manager_v1`, or a compositor teardown (`finished` after `locked`) all leave the
   ordinary scene showing, so `rescue` (rendered by the config's own surfaces) reaches the user. A
   wrong password happens with lock surfaces mapped and everything else hidden, where `rescue` is
   unreachable, so it reaches the config as `oblisk.lock` state instead: `{ active, authenticating,
   attempts, error }`. `attempts` exists because capability state is sampled at layout time
   (ADR-0044) rather than evented, so two consecutive identical failures would otherwise look like
   one unchanged `error` string to a config trying to count them itself. The Renderer sets `rescue`
   itself rather than round-tripping through the Supervisor, since it is the process that learns of
   the refusal first.

A masked field had to stop using `zwp_text_input_v3` and read `wl_keyboard` directly (amending
ADR-0027), because text input delivers nothing without a bound input method and a password could
otherwise never be typed. The Renderer never calls `unlock_and_destroy` on its own reading of a PAM
outcome; the password crosses as a `SecureSubmit` for `("lock", "authenticate")`, the Supervisor's
re-exec'd worker runs the PAM conversation (ADR-0028), and success comes back as the same
`SetSessionLock { locked: false }` command. A compositor-initiated teardown (`finished` after
`locked`) is answered with `unlock_and_destroy` because the protocol makes `destroy` a protocol
error once `locked` was sent and there is no other legal teardown verb; ADR-0042's rule against
calling it forbids the Renderer initiating an unlock, not ending an object the compositor already
ended. Removing a declared `lock` node while locked is already blocked as an ordinary topology
change queued behind ADR-0042 decision 4's swap gate. Nothing locks on idle yet:
`SupervisorFrame::IdleEvent` reaches the Renderer with no Lua-side dispatch registry to deliver it
to, so a config's only path to `lock()` today is an input callback.

Not built: a fallback lock screen when no `lock` node is declared. ADR-0046's rescue renderer is not
repurposed into one, since rescue exists for a config that failed to evaluate, and a config that
evaluated fine but declared no lock screen has not failed at anything. A Supervisor-owned lock
policy with a built-in lock tree is the upgrade path if a session ever needs to lock against the
config's wishes, and neither half is built.

## 0053. Five specified capabilities were never given a phase, and a bar is what found them

An audit triggered by trying to write a real bar config found that `oblisk-idl-api-specs.md` section
2 specifies fifteen capabilities while `shared::CAPABILITIES` built eleven; `battery`, `brightness`,
`workspaces`, `system`, `power`, and most of `audio` were missing or mismatched, and no build phase
owned any of the gap (the phase that built the capability roster was scoped by the D-Bus services
doc's section numbers, not the IDL's). Three unspecified capabilities (`privacy`, `updates`, `lock`)
exist instead, each added deliberately by its own ADR.

1. **Build `battery`, `system`, and audio's missing fields now; give `brightness`, `workspaces`, and
   `power` a phase of their own.** The first three are small and are what the bar needs.
   `workspaces` in particular is compositor-specific (`niri-ipc` is already a dependency) and
   deciding whether it speaks one compositor's IPC or an abstraction over several is a design
   question that a bar-fixing pass should not settle badly just to finish.
2. **`system.time` pushes only when the epoch second it would report actually changes, not on every
   scan tick.** A literal per-second `StateSnapshot` would mark the scene dirty and drive a full
   re-resolve and repaint of every surface every second, forever. Comparing against the last emitted
   second costs one comparison and gives the same steady-state cadence. This is a floor, not a fix:
   a bar drawing `HH:MM` still repaints 60 times a minute to change once. A configurable interval is
   the upgrade path (`sysinfo`'s per-task `watch` channel already has the shape) but needs the Phase
   25 Lua write path first, so it is not built now.
3. **`audio`'s payload moves to section 2.4's shape, and its existing fields are renamed.** The
   built payload was a bare array of `{node_id, pid, app_name, process_name}`; adding master
   `volume`/`muted` forces the outer shape to change regardless, so field names move to the spec's
   spelling in the same commit rather than matching neither the old shape nor the new. `pid` is kept
   alongside the spec's fields because finding the owning process was genuinely hard work (ADR-0016)
   and dropping it to narrow the table would discard that. Master `volume`/`muted` are real; per-app
   `volume`/`muted` ship as placeholders (`1.0`, `false`) because each stream node needs its own
   `SPA_PARAM_Props` subscription, a separate slice of work, and a wrong-but-present number was
   judged worse than an absent one, so the placeholder is named in the code rather than left to be
   discovered. This breaks the one existing consumer config, rewritten in the same commit. The parse
   has to distinguish two `ParamType::Props` objects a sink node advertises, the real mixer and an
   unrelated ALSA device-settings object with no mixer keys, by the presence of `channelVolumes`;
   misreading the second as the mixer produces a volume that is correct once per boot and zero
   afterward, a bug no captured-pod unit test would catch since the captured pod is the one that
   parses correctly.

`brightness`, built later: reads over the udev `backlight` subsystem, not inotify, since inotify
does not fire on a sysfs attribute write (confirmed with `udevadm monitor`), with the same 30s poll
fallback `battery` uses. Device selection ranks by the kernel's `type` attribute (`firmware`, then
`platform`, then `raw`, tie-broken by sorted name, skipping any device with non-positive
`max_brightness`), rather than first-in-readdir-order, because readdir order is wrong on a machine
with both `acpi_video0` and a native device. Writes go through `login1.Session.SetBrightness` on
`session/auto`, not a direct sysfs write, because the backlight node is root-owned `0644` and the
Supervisor runs unprivileged; the cost is that logind silently refuses a `set` from a session that
is not the seat's active one, which is accepted as correct behavior to inherit. With no backlight
device, the capability never pushes at all rather than fabricating `0`, since section 2.3 gives no
absence sentinel and a `0` would read as "screen off" rather than "no backlight hardware"; the
signal stays nil under ADR-0037's nil-until-hydrated contract. This is the same gap `audio` left
open, resolved the other way.

`power`, built later: `active_profile`/`profiles` come from power-profiles-daemon and
`on_battery`/`energy_rate` from UPower, two unrelated daemons that can each be missing
independently, so every field is optional individually rather than the capability pushing nothing
when any one source is absent; nothing is fabricated for a missing field, since section 2.13 gives
no sentinel for any of the four. `on_battery`/`energy_rate` come from UPower rather than the sysfs
paths `battery` already reads, because UPower's `OnBattery` is the system-wide answer across every
power supply, correct on a docked laptop with two adapters where a hand-picked sysfs device would
not be. The power-profiles-daemon half is built to the documented D-Bus API (trying both
`net.hadess.PowerProfiles` and the post-0.20 `org.freedesktop.UPower.PowerProfiles` names) but not
live-verified, since the daemon is not installed on the machine this was written on; the UPower half
was verified live on this machine.

Not decided: whether `oblisk.system.state` should ever be writable. Section 2.11 calls it read-only
and the implementation loads `state.json` once at construction, so nothing currently writes that
file and the read-only contract has no producer. Named here as a real gap, not filled by this ADR.

## 0054. The icon theme resolver lives in the renderer, and `image` is the node that draws a file

The Renderer resolves icon theme names and draws image files. The Supervisor does not gain an
icon-resolving capability.

1. **Renderer-side resolution.** The Renderer resolves theme names synchronously through the
   `freedesktop-icons` crate (0.4) with an in-process cache. The control socket carries one-way
   commands and one-way `StateSnapshot`s only, with no request/response shape, so a synchronous
   round trip to the Supervisor would block the render thread, which is also the Wayland dispatch
   thread and the config VM thread (ADR-0039, ADR-0048).
2. **An absolute-path name draws that file.** `icon { name = "/dev/shm/.../telegram.png" }` draws
   the file directly; `icon { name = "audio-volume-high" }` goes through theme lookup, matching how
   `Icon=` works in every `.desktop` file. Lets the tray collapse to `icon { name = item.icon_name
   or item.icon_path }`.
3. **`image` is a new node kind; `icon` is `image` plus the resolver.** `icon` is square and takes a
   theme name. `image` takes `source` (a path), width/height, and `fit`, for non-square content such
   as album art and wallpaper (ADR-0055). A departure from the IDL's node-kind list, recorded as
   such rather than folded into `icon`.
4. **SVG rasterizes through `resvg`; raster decodes through femtovg's bundled `image` crate.**
   Needed because Adwaita ships SVG icons. The cache key is the resolved path plus the integer pixel
   size, since the same SVG rasterized at two sizes is two different textures.
5. **`system:find_icon`'s `app_id` to `.desktop`-file lookup is not built.** Only the theme-name
   lookup has a caller. No Lua-facing `find_icon` is exposed, since `icon.name` already resolves
   theme names.
6. **The cache key also carries the file's mtime and length, not just path and size.** A path-only
   key served stale pixels once a tray icon changed, because `dbus/shm_icons.rs` overwrites
   `/dev/shm/oblisk-$UID/tray/{name}.png` in place on every `NewIcon` (ADR-0031 named this gap).
   Costs one `stat` per image node per frame, hit or miss.
7. **Eviction queues the texture id and frees it before the next frame is recorded**, not at
   eviction time. femtovg resolves an `ImageId` to a texture at `flush`, not at `fill_path`, so
   freeing immediately unbinds a texture an already-recorded draw call still names, and femtovg
   silently substitutes default paint on a missing id rather than erroring.

Rejected: resolving icons in the Supervisor with an LRU cache (`oblisk-supervisor-services-dbus.md`
§ 9.2), because the control socket has no request/response shape to carry it and a late-resolving
`icon.name` would make every icon in a `list` a two-pass lookup.

Not built: byte-bounded LRU eviction (current cache is a count-bounded FIFO); off-thread resolution
for cache misses (worker thread plus the scene dirty flag from ADR-0044 as wake-up).

## 0055. Wallpaper is an `image` on a Background panel, not a capability

Wallpaper is not a Supervisor capability. A config declares a `Background`-layer panel itself and
puts an `image` node in it; the `wallpaper:set(mon, path, fit, anim, dur)` row is superseded.

1. **No `wallpaper` capability.** Every argument of the old `wallpaper:set` already has a home:
   `mon` is `panel.monitor` (ADR-0038), `path` is `image.source`, `fit` is `image.fit` (decision 3).
   `anim`/`dur` have none, because nothing in the engine animates anything (decision 4). A
   capability here would relay a string the config already has to a surface the config already
   declares, with a D-Bus connection attached for no reason. ADR-0007's placement on the
   `Background` layer stands; only who declares the surface changed, and ADR-0038 already changed
   that.
2. **A runtime wallpaper change is a `state()` signal, not a command.** Binding `state(name,
   initial)` to `image.source` lets a config change its own wallpaper with no IPC, and lets it read
   back the current value, unlike a fire-and-forget command. It does not persist:
   `oblisk.system.state` is read-only and nothing writes `state.json` (open hole per ADR-0053), so a
   runtime-chosen wallpaper is lost on reload.
3. **`fit` is a property of `image`, with three modes.** `cover` (default, scales to fill and
   crops), `contain` (scales to fit, leaves remainder unpainted), `stretch` (ignores aspect ratio).
   No `tile`, since nothing has asked for it. `cover` is the default because it is the only mode
   that cannot leave bars down the side of a screen.
4. **ADR-0002's transition semantics stay unbuilt.** There is no animation model (no easing, no
   transition, no clock faster than `system.time`'s 1 Hz). What ships, an immediate texture swap
   with no crossfade, is ADR-0002's first-frame/reload branch, not a contradiction of it; the
   transition branch remains correct and unimplemented.
5. **`oblisk.config_dir` is added as a static string on the `oblisk` table**, beside
   `oblisk.version`, giving Lua a way to reference a file shipped beside `shell.lua`. It is the
   parent directory of the `shell.lua` actually loaded, not a second call to `shared::config_dir()`,
   so it cannot disagree with the file being read. Needed for a config to point `image.source` at
   its own wallpaper file; applies equally to any shipped icon or sound.

Not built: a file picker, directory scan, or `process.run` recipe for choosing a wallpaper path.
Memory cost of a full-screen texture (a 3840x2160 wallpaper is 32 MB RGBA, the largest single entry
ADR-0054's cache will hold) is left for Phase 24's harness to measure.

## 0056. `workspaces` speaks niri, and § 2.9 is wrong in three places

`workspaces` is built against niri only, with no compositor trait, and corrects three errors in §
2.9's shape. Amended by ADR-0075: every decision below still stands (one implementor, no trait), but
the compositor probe decision 1 reuses moved into a top-level `compositor` module, and the niri
types stay confined to `workspaces/niri.rs`.

1. **One compositor, no trait.** `workspaces` has one implementor, niri, so no trait is built; a
   trait with one implementor is Speculative Generality (ADR-0034 made the same call for
   `keyboard`). Only `detect_compositor()`/`CompositorKind` (the
   `$HYPRLAND_INSTANCE_SIGNATURE`/`$NIRI_SOCKET` probe) is shared with `keyboard`'s
   `CompositorLink`; nothing else, because Hyprland's workspace model (one active workspace per
   monitor, one globally focused monitor) does not map onto niri's (`is_active` per output,
   `is_focused` global) by field renaming, and writing that mapping without a machine to test it on
   risks a plausible but wrong implementation. A session with no `$NIRI_SOCKET` gets no `workspaces`
   push and the signal stays `nil` (ADR-0053's missing-backlight posture, ADR-0037's
   nil-until-hydrated contract). Extract a trait when a second compositor is implemented and
   live-tested.
2. **A second niri event-stream socket, not a shared one.** `keyboard` already holds one
   `Request::EventStream` connection; `workspaces` opens its own rather than sharing, because
   sharing would couple `keyboard` and `workspaces`' lifetimes and ordering, which no two
   controllers in this codebase do today (each owns its connection, pushes into a channel, `main.rs`
   `select!`s the receiver, ADR-0034). The duplicated startup replay costs a few kilobytes once per
   boot. A third consumer would need a shared owner with per-capability subscribers; two does not.
3. **Each output entry gains a `workspaces` array** of `{ id, idx, name }` ordered by `idx`, because
   § 2.9's `active_workspace`/`focused_workspace` are opaque ids with no way to render a strip of
   buttons. `id` is niri's stable, monitor-independent identity (what `workspaces:focus(id)` takes);
   `idx` is the 1-based on-output position shown to the user and not stable across a reorder; `name`
   is niri's optional named workspace, `nil` when unset. `is_urgent` is not carried, since nothing
   draws it yet.
4. **`focused_workspace` is optional, present only on the output that holds focus.** niri models
   focus as one global fact (`Workspace.is_focused`) but § 2.9 puts the field inside the per-output
   structure, which would be false everywhere but the true output if not made optional. Repeating
   the global id on every output would claim every monitor has focus; hoisting it out of the array
   would contradict the spec's structure. Optional costs nothing on a single-output machine (always
   present, always equal to `active_workspace`) and gives a config an exact `out.focused_workspace
   ~= nil` focus test.
5. **`active_client.is_fullscreen` is omitted; `class` is `Window.app_id` renamed.** niri-ipc
   26.4.0's `Window` has no fullscreen field anywhere (not in the struct, not in the event stream);
   fullscreen exists only as actions, not as readable state. The field is left absent (reads `nil`
   in Lua) rather than fabricated as `false`, which would be wrong for exactly the fullscreen
   windows a check exists to find. `class` maps from `app_id` since Wayland toplevels have no X11
   `WM_CLASS`; every shell makes the same substitution.

Not built: a per-workspace window list (only the single focused `active_client` is reported);
special workspaces are not modeled.

## 0057. `json.decode` is one function on the engine's existing null mapping

`json.decode` reuses `renderer/src/lua/json.rs`'s existing `to_lua` conversion rather than adding a
second JSON-to-Lua path, so `process.run` output (`lsblk --json`, `busctl --json=short`, `niri msg
-j`, curl responses) becomes readable from Lua.

1. **The decoder is `to_lua`, the same function every pushed capability payload already goes
   through.** That function turns off mlua's `serialize_none_to_null`/`serialize_unit_to_null`, so
   JSON `null` becomes an absent key rather than a truthy lightuserdata sentinel. A second decoder
   (pure Lua, or a fresh `lua.to_value` call) would map `null` differently, so a config would meet
   one rule on `oblisk.tray.items` and another on `lsblk --json` output. A `null` array element
   leaves a hole, so `ipairs` stops at it, consistent with the existing mapping.
2. **Failure returns `nil` plus a message, following `io.open`, not `cjson`'s raise.** A decode
   failure is routine: `out_cb` fires once per line, so a config decodes a growing buffer or decodes
   whatever a failing subprocess printed instead of JSON, and raising would force a `pcall` at every
   call site. The input argument is `mlua::LuaString`, not `String`, so non-UTF-8 subprocess output
   becomes a readable decode error instead of an mlua argument-conversion error. A failure
   converting the parsed value into Lua is folded into the same `nil`-plus-message pair, with
   wording distinguishing bad input (config author's problem) from a conversion failure (engine's
   problem).
3. **Success returns one value; failure returns two**, matching `io.open`'s variable arity rather
   than `dkjson`'s fixed three. A trailing `nil` on success would break `table.insert(t, decoded,
   nil)`, which Lua reads as an explicit position argument and raises on.
4. **No `json.encode`.** Nothing calls it yet; a config wanting to send JSON as subprocess input
   concatenates strings by hand. Add `encode` beside `decode`, through the same options in reverse,
   once something needs it.

Rejected: jq, because it needs a shell pipeline (quoting, an extra process, a hard jq dependency)
and `jq -r` still returns text that Lua must then split on a delimiter, which breaks on the exact
strings (window titles, track names) a bar displays. Rejected: a query crate (jql etc.), wrong
shape, since Lua is already the query language and only a table-to-index conversion is needed.
Rejected: a pure-Lua decoder in the config (the § 3.3 banner's original proposal), because it
guarantees the null-mapping disagreement above and is a hand-written parser running on the Wayland
dispatch thread against untrusted input.

`json.decode("null")` succeeds and returns bare `nil`, indistinguishable from failure by the first
return value alone; only the absent second return distinguishes them, and both mean "no data" so
this is treated as harmless rather than fixed with a sentinel.

## 0058. A crashed Renderer is detected and respawned, because a lock cannot be recovered otherwise

The Supervisor watches the authoritative Renderer's exit, classifies it, and respawns it with a
bounded restart brake; a respawn re-acquires the session lock if one was active. Measured against
niri 26.04 (`v26.04-85-gdd75865f`) in a nested compositor: a second `ext-session-lock-v1` client can
take over an orphaned lock and release it with `unlock_and_destroy`, so takeover-and-unlock works.
Quickshell cannot do this (niri issue #2986, closed as working as intended: recovery is the shell's
job) because the knowledge that the session was locked dies with the process that held it; Oblisk
splits that knowledge into `supervisor/src/lock.rs`'s `LockState`, which outlives a Renderer crash.

1. **The authoritative Renderer's exit is an event.** `main()`'s `select!` gains an arm on
   `authoritative.child`, so a dead Renderer is learned immediately rather than inferred later from
   failed pushes; a healthy idle Renderer and a dead one both send silence otherwise.
2. **A departure is classified, and the lock state is part of the message.** Exit is reported as one
   of clean exit, non-zero exit, or signal, as a pure function over the exit code and signal (tested
   without a process, covering a SIGKILL from the OOM killer, a panic's non-zero exit, and a clean
   `0` from a shutdown reap, which must never read as a crash). The log line also names whether a
   lock was active, because an unlocked death costs a bar and a locked death costs the session.
3. **The Supervisor respawns, with a brake.** The brake, not the respawn, is the decision: an
   unbraked loop turns one dead bar into a strobing lock screen. The brake allows a bounded number
   of restarts inside a time window and stops and says so once exceeded. The ADR named no figure;
   `supervisor/src/generation.rs`'s `RESTART_LIMIT` and `RESTART_WINDOW` set it at 3 restarts
   inside 60 seconds.
4. **A respawn re-acquires the lock when `LockState.active` says one was active.** The Supervisor is
   the only participant that can make this call: the compositor will not and should not unlock, the
   dead Renderer's knowledge is gone, and the replacement starts with no history. `LockState`'s
   `acquisition` counter gives the re-acquired lock its own identity. If the compositor refuses the
   takeover, this degrades to today's outcome (locked session, VT switch) and must say so rather
   than retry into the brake. Refined after initial writing: the replacement re-acquires only if the
   on-disk config still passes ADR-0052 decision 3's acquisition predicate (exactly one `textfield`
   with `secure_submit` for `("lock", "authenticate")`), since a crash destroys the four reload-time
   protections (`defers_swap` on the `lock` node, `lock_stays_authenticatable`'s veto, rollback to
   the prior scene, live restyling) that all depend on a live generation existing; a replacement
   that fails the predicate must refuse the lock rather than present one with no way out.
   `lock_stays_authenticatable`'s refusal wording ("the lock screen that is on screen still stands")
   does not apply post-crash and must not be reused verbatim on this path.

Rejected: pinning the last config that successfully took the lock and handing it to the replacement
instead of what is on disk, because decision 4's predicate already catches the case this defends
against, at the cost of silently running code the user has since edited on the one screen where a
surprise is least recoverable.
Rejected: letting a session manager restart the whole stack (Supervisor and Renderer both), because
a restarted Supervisor's `LockState` resets to `Default` (`active = false`), reproducing the
quickshell failure this ADR exists to fix.
Rejected: having the Renderer re-acquire its own lock, because the process that would need to notice
the crash is the process that died.

Consequences: ADR-0042's swap gate (deferring a generation swap while a lock is requested or active)
remains the primary defence against planned reaps; this ADR covers only an unscheduled crash. A
respawned Renderer is a new generation: every `state` signal is lost, unlike an in-place reload
(ADR-0044 decision 5). The Supervisor's own death is still unhandled and is now the larger remaining
hole: killing the Supervisor leaves the Renderer reparented to `systemd --user`, spinning its 15ms
poll at 17.8% of a core, because `try_recv`'s `Err` collapses `Disconnected` into `Empty` (see
`renderer/src/wayland/mod.rs:634`).

## 0059. The Renderer exits when the Supervisor is gone, and a service manager reruns the pair

A Renderer that loses its Supervisor connection exits rather than surviving as an inert shell;
restarting the pair is a service manager's job, not either process's own.

1. **Exit on disconnect.** `TryRecvError::Disconnected` from the inbound channel is now handled
   instead of being read as `Empty` (which is what `try_recv`'s `Err` had collapsed it into), and
   the Renderer exits `70`. A surviving Renderer painted and hit-tested normally but could reach
   nothing behind it: every capability lives in the Supervisor, PAM is a Supervisor worker
   (ADR-0028), and `process.run` wrote into a socket nobody read.
2. **Exit while locked does not unlock.** A Renderer holding `ext_session_lock_v1` when its
   Supervisor dies exits still holding it; unlocking first would make one `kill` a way past a lock
   screen. The exit uses `std::process::exit`, not a loop break, because breaking drops `App` and
   SCTK's `SessionLockInner::Drop` sends a bare `ext_session_lock_v1.destroy`, which is
   `invalid_destroy` once `locked` was sent (ADR-0052). Any lock request already enqueued in the
   same drain is flushed before exit, since SCTK's lock path only enqueues and does not round-trip;
   skipping the flush let the exit message claim a locked session the compositor was never asked to
   lock.
3. **A tripped restart brake exits with a code that means do not restart.** ADR-0058's brake (three
   Renderer deaths inside 60 seconds) returns `Shutdown` and exits `3`; ordinary Supervisor exit is
   `0` or `1`. `RestartPreventExitStatus=3` in the unit file stops systemd from rerunning the whole
   stack roughly once a minute forever, which a code-blind restart policy would do since the brake's
   60-second window is looser than systemd's default start limit (5 starts in 10 seconds).
4. **Rerun is a service manager's job.** `packaging/oblisk-shell.service` exists because niri's
   `spawn-at-startup` does not restart what it spawns (niri issue #2986).
   `PartOf=graphical-session.target` stops the unit at session end instead of letting the Supervisor
   exit into a restart, and collects any Renderer or `process.run` child that outlived a Supervisor
   which ran no cleanup of its own, since the whole control group goes with the unit.

Rejected: reconnecting to a new Supervisor instead of exiting, because a new Supervisor means a new
`LockState`, capability roster and generation id, so a reattached Renderer would hold signals
hydrated from a process that no longer exists.

Rejected: the Renderer respawning its own Supervisor, because it inverts ownership: the Supervisor
holds capabilities and runs PAM (ADR-0042) precisely so the process on the glass cannot hand itself
a fresh roster.

Not built: a lock-survives-restart flag. A restart while the session is locked currently comes back
believing it is unlocked, since the new Supervisor's `LockState` is `Default`, while the compositor
is still locked from before.

## 0060. A restarted Supervisor learns the session was locked from a file in the runtime directory

ADR-0059 left one hole: a restarted Supervisor's `LockState` starts `Default`, so it believes the
session is unlocked while the compositor is still locked. This closes it with a marker file that
survives the process.

1. **The fact lives in a file, because it has to survive SIGKILL.**
   `$XDG_RUNTIME_DIR/oblisk-session-locked` exists exactly while the compositor is locked. It must
   survive the Supervisor being killed without running any of its own code, and must be readable
   before anything else happens at startup, so nothing in-process or Renderer-dependent qualifies.
   Using `$XDG_RUNTIME_DIR` rather than a config or state directory bounds staleness: the directory
   dies with the user's last session, so a marker can only be read back inside the login that wrote
   it.
2. **It is written off the Renderer's report, never off `LockState.active`.** `active` means "this
   shell holds the lock", and `LockEvent::RendererLost` clears it (ADR-0058 decision 4) even though
   the compositor stays locked and is required by protocol not to unlock on client death; a marker
   driven off `active` would erase itself in the exact case it exists for. The marker instead reads
   `shared::LockOutcome`: `Locked` sets it, `Unlocked` and `Finished` clear it, `Refused` leaves it
   alone, and `RendererLost` is not a `LockOutcome` at all so it cannot touch the marker.
3. **A set marker at startup feeds the re-acquisition path that already exists.**
   `relock_when_connected` (ADR-0058 decision 4's intent flag) now starts
   `Some(SupervisorRestarted)` when the marker is set, instead of always `None`. The acquisition
   predicate still gates it unchanged: a replacement re-acquires only if the on-disk config still
   declares exactly one `textfield` with `secure_submit = { capability = "lock", action =
   "authenticate" }`, checked in the Renderer rather than duplicated here. `RelockReason`
   distinguishes a crash replacement from a restart so the log line reads correctly for each.

Measured in a nested niri: the marker is absent before any lock, set the instant the lock is taken,
still set after `SIGKILL`ing the Supervisor and watching the Renderer exit behind it (ADR-0059), and
read by a second Supervisor which then asks generation 0 to take the lock over; the recovered
Renderer gets `locked` and keyboard focus lands on `secure_submit`.

An unreadable or unwritable marker file is logged and swallowed rather than treated as fatal, and
`is_set` reads a missing file as "not locked": every ambiguity resolves toward locking, since a
marker that should have been set costs the ADR-0059 hole (an unreachable lock screen), while a
marker that should have been cleared costs only one password prompt. One sequence still produces a
stale marker: kill the Supervisor while locked, unlock the session from outside oblisk entirely (VT
login plus `loginctl unlock-session`), then restart the shell; the shell relocks an already-open
session at the cost of one password prompt. `ext-session-lock-v1` has no request to ask the
compositor whether it is locked, and a takeover of an existing lock returns `locked` identically to
a fresh one, so nothing can settle these cases by asking.

Rejected: serializing the whole `LockState` into the marker, because `attempts` and `acquisition`
describe a lock screen and PAM worker that no longer exist after a restart; only the boolean means
anything across the boundary.

Rejected: clearing the marker on a clean Supervisor shutdown, because a clean shutdown does not
unlock the compositor either (`SIGTERM` makes the Renderer exit without unlocking, ADR-0059 decision
2), so clearing the marker there would show an unlocked-looking shell in front of a still-locked
session.

## 0061. Desktop entries are an enumerated capability, not a lookup call

Desktop-entry lookup for the launcher, focused-window icon, and tray items is served by a new
`applications` capability that snapshots the whole set, rather than by a synchronous per-`app_id`
`find_icon` call. This amends ADR-0054 decision 5, which had left the `app_id` half of `find_icon`
unbuilt.

1. **A snapshot capability, not the `find_icon` signature.** ADR-0054's objection to a synchronous
   resolver still stands: the control socket carries one-way commands and one-way `StateSnapshot`s,
   with no correlation id and no reply. It does not reach this data though, because desktop entries
   are a set that changes only when packages install, snapshot-shaped like `tray`'s items.
   `applications` joins `shared::CAPABILITIES` and pushes `{ entries, by_app_id }`; the Renderer
   needs no change since `json::to_lua` already converts any payload.
2. **`by_app_id` repeats the entries rather than indexing into them.** An index would be an array
   index, and JSON arrays count from zero while the Lua table they become counts from one, so every
   config reading it would carry an invisible off-by-one. `app_id` matching runs two passes, exact
   `StartupWMClass`/desktop file id first, then case-folded spellings and the last dot-segment of a
   reverse-DNS id, so one entry's fuzzy guess cannot displace another entry's exact match regardless
   of directory scan order.
3. **The argv never crosses into Lua.** `entries` carries `id`, `name`, `icon`, never `Exec`;
   launching is `applications:launch(id)` and the parsed command line stays on the Supervisor's
   side. A config that could read an argv could assemble a different one and run it with the
   Supervisor's privileges. `process.run` is also the wrong lifecycle for a launched GUI app: it
   pipes stdout/stderr and holds the `Child` for its exit code (ADR-0026), so a generation swap
   would reap a still-running application.
4. **Rescan on demand, not on a watch.** The scan runs at startup and on `applications:refresh()`;
   nothing watches `/usr/share/applications`. Reusing `watcher.rs` would mean generalizing an
   ADR-0047-governed, config-tree-specific file (recursive descent, content hashing, `.lua` filter)
   for an event that fires a handful of times a month. The scan pushes only when the result differs
   from the last one, since every `StateSnapshot` marks the scene dirty and drives a full re-resolve
   and repaint (ADR-0044).

Not built: locale support (`Name[de]` is skipped; half-doing it risks preferring the wrong regional
variant), `OnlyShowIn`/`NotShowIn` filtering, a `$TERMINAL` probe for `Terminal=true` entries
(refuses without the variable set rather than guessing), field-code stripping inside a longer token
(only a bare `%f`-style token is handled), and incremental scanning (every `refresh` re-reads every
entry, currently 290 lines across two directories, on a `spawn_blocking` thread).

## 0062. Hover is a signal the engine writes, not a callback it calls

The engine computes hover from pointer input and writes it into a signal the config only reads,
rather than calling an `on_hover` callback. This lets the Renderer itself own and publish reactive
state, not just relay it.

1. **A signal, not a callback.** A hover callback (`on_hover = function(entered) ... end`, mirroring
   `on_click`) would make every config wanting a tooltip rebuild the same open/close state machine,
   with a real bug class where the closing edge never arrives because the node was replaced by a
   re-resolve in between. Instead `hover(name)` returns a boolean signal, bound directly to
   `visible` or similar, so there is no edge to miss and no order to get wrong. A config wanting to
   *act* on the entry edge still cannot; `on_hover` can be added beside this later without changing
   it, but nothing in the reference shell's four measured hover affordances needs it, since all four
   are conditions, not actions.
2. **The engine writes it, and that makes the Renderer a source of signals.** Every signal before
   this was fed from outside the Renderer, either a capability's `Live` signal off the control
   socket (ADR-0029) or a `state(name, initial)` signal from Lua (ADR-0044 decision 5). Hover is
   computed by the Renderer itself and pushed into a signal the config only reads, establishing that
   the engine may own and publish reactive state, not just relay it, so the next signal of this
   shape (`focused`, `pressed`, `maximized`) is a spec row rather than a re-argument. Routing hover
   through the Supervisor and back as a capability was rejected: it would be a socket round trip to
   answer a question already resolved from a rect the Renderer already had, at pointer-motion rates,
   and hover has none of the long-lived-connection lifetime the process boundary (ADR-0020) exists
   to keep out of the Renderer. A hover slot is two signals: `hover(name)` (boolean) and
   `hover_rect(name)` (the node's absolute rect, ADR-0050 decision 3's coordinate space), kept as
   two names rather than one record because they bind to different properties (`visible`,
   `anchor_rect`). `hover_rect` keeps its last value when the pointer leaves rather than clearing,
   because § 6.3 refuses a zero-sized `anchor_rect` and the popup is still resolving on the closing
   turn. Identity is by name, one `name -> Signal` map per generation exactly as `state()` (ADR-0044
   decision 5), so an in-place reload finds the same signal and a tooltip open across a config edit
   stays open. `hover(name)` is its own signal kind rather than reusing the capability kind, so
   `signal:set()` (which already refuses everything but a `state` signal, ADR-0044 decision 5)
   rejects writes to it, and the engine's writer accepts only a hover signal, so a config cannot
   point `hover` at a capability's snapshot.
3. **The `hover` property carries the handle, so it does not resolve.** `resolve_properties`
   normally replaces every `Signal` in a property map with its current value (ADR-0044 decision 1),
   which would turn `hover` into a bare boolean with no way to recover which signal to write.
   `hover` is a structural property instead, copied through raw by `is_structural_property` the same
   way `id` and a panel's `layer`/`anchor`/`monitor`/`namespace` already are, since a property
   naming a thing rather than carrying a value has nothing to resolve.
4. **One write per boundary crossed, not one per motion event.** `wl_pointer` reports motion at
   device rate, and every `LiveSignalHandle::set` marks the single scene-dirty flag (ADR-0044
   decision 2), re-resolving the whole generation. The write compares against the current value
   first, so a pointer sitting still inside one button re-resolves nothing, and crossing a button's
   edge re-resolves twice (once per node). The hit test itself still runs every motion event, but it
   is a bounded tree walk, the same one the click path already does.
5. **Hovered means on the hit path, so ancestors are hovered too.** `hit_path` (ADR-0050 decision 1)
   returns the whole root-first chain of nodes containing the point; every node on that chain is
   hovered. Restricting hover to the innermost node would make it useless on composite widgets like
   a `pill` (`row` wrapping `button` wrapping `text`) without restructuring them. The topmost-child
   rule for overlapping siblings carries over unchanged from the click path.

Not built: a dedicated tooltip node (§ 6.3's `popup` with `grab = false` already serves as one),
`on_scroll` (a separate problem, ranked next), hover-driven animation (an expand-on-hover snaps), a
cursor-shape change on hover (`wl_pointer.set_cursor` untouched), and a keyboard equivalent (a
future `focused` signal, not this one).

## 0063. A display list is what makes a repaint skippable

An idle bar repainted every mapped surface once a second because ADR-0044 decision 2's dirty flag is
one flag for the whole scene, with no record of which surfaces actually changed. Painting now goes
through a flat display list that can be compared frame to frame, so an unchanged surface skips its
GPU work entirely.

1. **Paint through a display list, not straight to the canvas.** `paint_tree` used to walk the
   resolved tree and issue femtovg calls directly, with nothing in between to compare. The walk is
   now two halves: `build(root, scale) -> DisplayList` flattens the tree into a `Vec<DrawCmd>` of
   plain Rust data, and `execute(painter, images, list, scale)` turns that into femtovg calls.
   `wayland::App::paint_surface` builds the list, compares it against the one that surface last
   painted, and returns before touching the GL context on a match. The list is the sole source of
   truth for what gets drawn, so equal lists cannot mean different pixels, unlike a hash computed
   alongside the drawing that could silently drift out of sync.
2. **Compare plain data, never `ResolvedNode`.** Deriving `PartialEq` on `ResolvedNode` cannot work:
   a node's properties are a `HashMap<String, mlua::Value>`, and mlua compares tables by identity,
   so a property resolving to a table would compare unequal every pass and repaint forever (proven
   by the test `set_changed_cannot_dedupe_a_table_because_table_equality_is_identity`, written when
   hover rects hit this). `DrawCmd` holds no Lua value; `Rgba`, `BorderColor`, `EdgeInsets`, `Fit`,
   `LogicalRect` and `PhysicalRect` already derive `PartialEq`. Float equality is used deliberately:
   both sides come from the same parsers over the same inputs, so an unchanged input is
   bit-identical, and `NaN` comparing unequal to itself just means an extra repaint, never a stale
   frame.
3. **The clip is precomputed, not a save/restore nest.** A flat list has no nesting to hang scissor
   save/restore on, so each `DrawCmd` carries the precomputed intersection of its own snapped box
   with every ancestor's, and `execute` calls `scissor` directly. This is equivalent because every
   clip is an axis-aligned rect, intersection is associative, and the crate applies no canvas
   transform. A subtree whose clip is empty is left out of the list entirely, which also means
   moving something fully off-screen produces no list change and no repaint.
4. **Invalidate on anything that makes the buffer undefined.** `last_painted` holds `((width,
   height), DisplayList)`; `None` means "must paint". It is cleared when the surface is bound (a
   fresh `EGLSurface` holds no prior pixels) and written only after `eglSwapBuffers` returns
   success, so a frame that never reached the compositor cannot let a later identical list skip a
   paint the screen never got. Every branch that cannot prove the buffer still matches
   `last_painted` clears it, since painting once too often costs a frame but skipping once too often
   leaves a stale surface with nothing scheduled to correct it.

Measured A/B on the same release binary, 25 second windows on an idle session: renderer CPU went
from 0.80% of a core to 0.60%, niri's from 0.52% to 0.36%. The wallpaper's 2.23 repaints per second
went to zero; the bar still repaints on the clock's seconds digit (1.20 paints against 1.03 skips
per second).

Not built: skipping resolution of invisible surfaces. The scene still re-resolves and deep-clones
all eleven surfaces on every push, nine of them invisible, left separate because `lock.lua` and
`popup.lua` both rely on a `:map` running whether or not the node it feeds is visible. The single
scene-wide dirty flag also stays, so `build` still runs for every surface on every push.

## 0064. A masked field draws from a count the tree never holds

`textfield` painted nothing at all, so typing an invisible password into a lock screen made a typo
indistinguishable from a slow unlock; measured on this machine, `pam_unix`'s `pam_fail_delay` runs
about 2 seconds nominal per wrong attempt and `pam_faillock`'s defaults lock the account for 10
minutes after 3. The fix draws a masked character count that never enters the retained tree.

1. **Paint takes the character count as an input, not from the tree.** Putting the masked value on
   the node as a resolved property, the obvious route, is forbidden by ADR-0005: typed bytes live in
   `shared::SecureBuffer`, deliberately outside the Lua VM and the scene, and anything derived from
   them entering the retained tree would make the Renderer's clone/reconcile/retire machinery carry
   it. Instead the count travels beside the tree: `build(root, scale, focus: Option<&SecureField>)`
   takes an optional `SecureField { target, filled }`, and `SecureBuffer::char_count` (the only
   non-`expose_secret` read on it) discloses only a length, the same thing a row of dots discloses
   anyway. It counts characters, not `len()`'s bytes, since a non-ASCII character must draw one dot,
   not two or three.
2. **Focus is a destination, not a node.** `SecureField` carries the `{ capability, action }` pair
   rather than a node id, matching what `wayland::input::FocusedField` already tracks (a surface id
   plus a `SecureSubmitTarget`). This makes the routing rule fall out automatically: a field
   addressed to a different capability/action than the focused one does not fill, so a Wi-Fi PSK's
   length cannot leak onto the lock screen, the same rule `retarget_secure_submit` enforces for the
   bytes themselves. An unfocused field draws its placeholder rather than a fallback, because
   changing focus zeroizes the buffer and there is no typed state left anywhere to draw.
3. **A keystroke repaints, it does not re-resolve.** Typing changes no property in the retained
   tree, so `re_resolve_if_dirty` has nothing to notice. `App::secure_input_changed`, set by
   `apply_secure_key` and `focus_secure_submit`, triggers a repaint without re-resolving; ADR-0063's
   display-list comparison then narrows that repaint to the one surface holding the field, rather
   than marking the scene dirty and re-resolving all eleven surfaces to move one glyph.
4. **The PAM service is probed, and falls back.** `PAM_SERVICE` was hardcoded to `"login"`, the
   console-login stack, which runs `pam_nologin` and `pam_shells` among others, neither of which
   cares whether the person at the keyboard is the one who locked the screen.
   `packaging/pam.d/oblisk` is the stack Oblisk wants: `auth` and `account`, both `include
   system-auth`, no `session` or `password` chain since `run_conversation` only calls `authenticate`
   and `account_management`. `pam_service_in` probes for the installed file (a `stat` per
   authentication, deliberately uncached so installing the file does not require restarting the
   shell) and falls back to `"login"` when absent, because a missing service falls through to
   `/etc/pam.d/other`, which is `pam_deny` on a stock Arch install: naming `oblisk` unconditionally
   would turn a missing packaging file into every correct password being refused.

Not built: removing `pam_fail_delay`'s two-second wrong-password delay or `pam_faillock`'s lockout
(both are the machine's own brute-force protection, left to `faillock.conf` to tune), a caret,
placeholder styling, or a real text field beyond one `draw_line` of repeated glyphs.

## 0065. A font file is mapped once and shared, not copied per reader

Font bytes are mapped once via `fontdb::Database::make_shared_face_data` and shared by `Arc`,
instead of each consumer holding its own copy. RSS is the wrong metric for judging this; private
dirty is the number that reflects real cost.

1. **Map the font chain, don't copy it.** An 11MB `NotoColorEmoji.ttf` cost 27MB of private dirty
   because it was held three times: the shaping worker's `chain_bytes: Vec<Vec<u8>>` (via
   `data.to_vec()`), femtovg's internal copy in `Canvas::add_font_mem` (`data.to_owned()`), and
   cosmic-text's own mapping. Calling `make_shared_face_data` before handing the `Database` to
   `FontSystem` collapses all three onto one `Arc<dyn AsRef<[u8]> + Send + Sync>`: the worker holds
   the `Arc`, femtovg receives it via `TextContext::add_shared_font_with_index`, and cosmic-text
   finds the mapping already in the database. `add_shared_font_with_index` lives on `TextContext`,
   not `Canvas`, so `TextPainter` builds the context first and passes it to
   `Canvas::new_with_text_context`; `Canvas::add_font_mem` remains the only font route the canvas
   exposes on its own, and it still copies. Measured on an idle 11-surface session: RSS 192.5MB to
   166.3MB, private dirty 49.7MB to 26.4MB (deleting the emoji font entirely lands within 4MB of
   this fix, so keeping it costs almost nothing). The `unsafe` in `make_shared_face_data` is
   documented: its hazard is a font file rewritten on disk while mapped, a risk cosmic-text already
   accepts for every font it renders. Rejected: a private copy per font per process, to defend
   against a system font being edited in place, because the cost is paid on every process for a
   threat that doesn't happen in practice.
2. **Keep femtovg on OpenGL ES, not wgpu/Vulkan or a CPU rasterizer.** Mesa's GL stack (130MB,
   mostly `libLLVM`/`libgallium`) is `Shared_Clean` and already mapped into niri and other
   processes, so removing it frees no physical memory while the compositor runs. Switching to
   wgpu/Vulkan (measured via `vkcube`: 24.8MB RSS, 2.7MB private dirty) would only move a number a
   monitor prints, and femtovg has no wgpu backend, so it would mean replacing the whole 2D
   renderer, the paint execute path, the image upload path, and the EGL surface binding. Dropping GL
   for `wl_shm` plus `tiny-skia` would unmap Mesa but cost 9.2MB of private dirty per 1920x1200
   buffer (18.4MB double-buffered) for the wallpaper alone, versus the current GPU buffer objects
   costing this process nothing (`Rss=0` in smaps). Rejected: wgpu/Vulkan and `wl_shm`+tiny-skia,
   because both trade shared-clean memory (free) for private dirty (not free), making RSS look
   better while making the machine worse.

Not built: shrinking `VmSize` (768MB, mostly unmapped glibc arena reservations, address space is
free on 64-bit) and CJK font coverage (`"Noto Sans CJK JP"` resolves to nothing on this machine; a
missing package, not a bug here).

## 0066. The icon path lookup is the paint loop, not the GPU

Following ADR-0063 (which stopped unneeded repaints), this measures what a repaint that does happen
costs. The cost is CPU work recording draws, not GPU submission, and within that, icon name
resolution dominates.

Instrumenting `paint_surface` on an idle 11-surface session showed `Canvas::flush` (actual GL
submission) at 8.3ms against 165.1ms spent recording draws into femtovg, over a 30-second window;
`eglSwapBuffers` at 0.04% of a core (it does not block, no `eglSwapInterval` call is made, default
is 1); and the per-surface deep clone in `Scene::surface` at 1.6ms, negligible. Splitting recording
by draw kind found icons at 62 calls totaling 102.0ms, 1645µs each, versus 1.7µs for a box and
12.2µs for text. Of that 1645µs, 1638µs was `icons::resolve`, a `freedesktop-icons` search across
the active theme and its inheritance chain, not drawing; the texture itself was already cached
(`ImageCache`: 32 hits, 2 misses).

`resolve` now memoizes `(size, name) -> Option<PathBuf>` for the life of the process. A `None` is
memoized as carefully as a hit, because "not found" is the expensive answer: it means the whole
inheritance chain was walked and every candidate stat'd. Process lifetime is the correct scope
because `theme()` is already a `OnceLock` read once per process, documented as reflecting a theme
change only at the next reload; the memo is exactly as stale as the theme it's keyed against, and
the generation swap (ADR-0054) clears both. `with_cache` (on the `freedesktop-icons` call) still
caches parsed theme indexes; it does not cache the per-name search, which is what was measured and
fixed here.

Measured after: per-icon resolve 1645µs to 79µs, recording draws 165.1ms to 39.1ms, bar per repaint
3.9ms to 0.57ms, whole GL phase 0.64% to 0.19% of a core.

Not built: `eglSwapInterval(0)` (nothing to win, swap is 0.04% of a core), removing the per-image
`stat` (its cache key carries the file's revision so an edited icon appears without reload; the
alternative is re-reading bytes to detect a change, and it doesn't show up against a 79µs icon), or
removing the per-surface deep clone (0.00% of the window).

## 0067. The Wayland client addresses surfaces by position, the retained scene by identity

`wayland::App::surfaces` stays keyed by `Vec` index (position); `layout::scene`'s top-level surfaces
stay keyed by id (identity), per ADR-0038 (ADR-0023). These are not an inconsistency to reconcile.

ADR-0038's "identified, not ordered" governs how `shell.lua` declares surfaces and how the retained
scene matches a fresh evaluation against the one already applied, because node identity must survive
a reload. It does not govern how the Wayland client stores protocol handles for one generation's
live surface instances, a different problem.

Every index in `wayland/` comes from one of three origins, none of which is a caller already holding
an id: a protocol event carrying a `wl_surface` (rekeying would just make the callee scan again for
the index it needs anyway); a bulk loop that iterates and mutates (rekeying costs a `Vec<String>`
allocation plus a scan per element, on the Wayland dispatch thread in the paint and activate-draw
paths); or a cross-module call where the caller already resolved the index two lines above. Where a
caller genuinely holds an id instead of a position, the code already uses one:
`App::destroy_surface_by_id` takes `&str`, called from output removal and instance reconciliation,
not from a protocol object. The code uses identity where the caller holds identity and position
where the caller holds position, the same answer reached twice, not one answer applied
inconsistently.

The index also serves a borrow-checker constraint (`surface.rs:1053`): these methods need `&mut
self` for EGL state and `self.text_painter`, which a held `&mut TrackedSurface` would conflict with;
`paint_surface` alone touches `client`, `egl`, `gl`, `image_cache`, `shaping`, `text_painter`,
`exit` and `surfaces`.

Rejected: folding the nine `map_state` assignments across four files into named transitions. Worth
doing only if a transition carries an invariant; the candidate invariant, that `null_buffered` must
move with `map_state`, does not hold. `null_buffered` is PBA-candidate staging set only inside
`bind_and_clear`'s `is_pba_candidate` branch and cleared only where a `window` or `popup` object is
destroyed; `App::unmap` leaves it alone because a panel's object survives unmap. With no invariant
to enforce, the fold would say less than the assignment it replaces.

Not built: rekeying `wayland::App::surfaces` to surface id. `wayland/` keeps 26 methods taking
`index: usize` and roughly 123 `self.surfaces[index]` accesses; this is the intended shape.

## 0068. Paint properties are parsed once at apply time, and a bad one fails the pass

`node::paint_style` now parses all fourteen paint-property parsers once, while `Scene::apply`
resolves the node, instead of `layout::paint::build` re-running them on every node of every mapped
surface on every dirty turn. A malformed value now fails the apply pass instead of being treated as
absent.

The old cadence was wrong because ADR-0063 decision 1 made the display list decide whether a surface
repaints, so `build` had to run before a surface could decline a frame, and ADR-0044 decision 2's
single scene-wide dirty flag meant any capability push re-resolved every surface. Properties already
resolved once that pass were being reparsed at a cadence nobody chose.

The failure rule was the bigger change. One resolved property map previously had three different
opinions on a malformed value: `scene.rs` geometry used `?` (apply fails, scene rolls back,
`oblisk.rescue`); `layout::paint` logged and substituted the absent-key default;
`wayland::surface::apply_resolved_state` logged and kept the last applied value. Paint's lenient
rule was meant to stop a bad paint property from blanking the surface around it, a defence that
predates rescue and rollback: a bad `align_v` one line away already takes the tree down, so a
non-numeric `background` is the same class of bug. The lenient rule also logged the rejected value's
full `Debug` form every frame, forever, measured at 20MB in a hostile case. `apply_resolved_state`'s
rule stays as is: it runs at configure cadence, not the frame path, and has a last-applied value
worth keeping, which a paint pass does not.

What still runs at paint time is only arithmetic over already-parsed data that needs an input the
resolve pass lacks: physical scale for an `icon`/`image`'s pixel size, and keyboard focus for a
`textfield`'s placeholder-vs-mask choice. `PaintStyle` holds no Lua value, per ADR-0063 decision 2
(mlua compares tables by identity, so a table-valued signal would compare unequal every pass).

Consequences: `wayland::input::focused_target` is now infallible and its malformed-`secure_submit`
fallback branch is gone, because a tree with an unparseable `secure_submit` now fails apply before
any pointer event reaches it; `layout::secure_submit::secure_submit_targets` lost its matching
skip-malformed rule the same way. Coverage widened: `build_node` used to return early on invisible
or zero-clipped nodes, so a malformed property under `visible = false` was never parsed until the
node became visible; every resolved node is parsed now regardless of visibility, so such a config
fails at boot instead of at reveal time. `layout::node` lost twelve `pub` parser functions
(`parse_background`, `parse_radius`, `parse_border_color`, `parse_border_width`, `parse_font_size`,
`parse_foreground`, `parse_icon_name`, `parse_image_source`, `parse_fit`, `parse_placeholder`,
`parse_mask_character`, `parse_secure_submit`), now internal to `node`; asking what a node paints
means asking for its `PaintStyle`. `layout::paint` names no property and imports no `mlua`;
`ResolvedNode.properties` stays because `hover`, `on_close` and `on_dismiss` still want the raw
`Value` at their own cadence.

Not built: moving the geometry parsers too. They already run at apply time under the same `?`
failure rule this ADR gives the paint properties, so there was no second half to move.

## 0069. A scroll offset is engine state the layout pass clamps

A wheel event's scroll offset lives in the retained scene and is clamped by the layout pass, not
kept beside the Wayland surface or handed to config code as a raw delta. (Note: ADR-0077 moved
sizing/positioning to `taffy`, so the offset is now applied in the `finish` walk that reads solved
geometry back, not in `position_children` as decision 1 below describes; the clamp itself, computed
from visible children's border boxes plus margins plus `spacing`, is unchanged. Reading the extent
from taffy's `scrollable_overflow_rect` instead was rejected: CSS scrollable overflow excludes
children's margins, and this engine's footprint includes them.)

1. **The offset is in the scene, not beside the surface.** Keeping the offset on `wayland::App` next
   to its surface, applied at paint and hit-test time, looks cheap but puts geometry in two places:
   hit testing, hover, the clip stack and the display list all read `RetainedNode::rect`, and each
   would have to re-apply the offset correctly, in the same direction, or paint and click disagree.
   ADR-0067 had just separated what the Wayland client addresses by position from what the scene
   addresses by identity; this would reintroduce that split one layer down. So the offset is applied
   where `rect` is produced, and every reader gets it for free. The cost was measured, not assumed:
   a wheel event marks the scene dirty and re-lays out, p50 over 100 applies of a 400x600 `list`
   panel, release build: 200 rows 4.86ms (2.19ms after the shape cache landed), 500 rows 12.78ms
   (6.14ms after). 2.19ms fits a 120Hz frame and 6.14ms fits 60Hz; the shipped launcher (50 entries)
   costs about 1.2ms. Upgrade path if a list outgrows this: a narrower dirty flag or a virtualized
   list building only near-viewport rows, both additive, neither requiring this decision reversed.
2. **The engine owns the value, the config reads it.** An `on_scroll(delta)` callback cannot be made
   correct in config code: clamping needs the content extent (known only after the pass resolves and
   measures children) and the viewport extent (known only from surface config), neither available to
   Lua. Instead the engine emits a signal, following ADR-0062's shape: `scroll(name)` is name-keyed
   like `hover(name)` and `state(name, initial)`, so an in-place reload finds the offset the user
   left. `on_scroll` is deliberately not built; nothing in the reference config wants the raw wheel
   event, only a value to react to.
3. **A property, not a node kind.** `scroll = <signal>` is a property on containers that already
   flow (`column`, `list`), exactly like `hover`; a dedicated node kind would duplicate the whole
   layout arm to add one field.
4. **The layout pass clamps, and writes back what it used.** The clamp bound is `(total_main -
   content_main).max(0.0)`, computed where `spare` already is in the row/column layout arms, using
   values (`total_main`, `content_width`/`content_height`) already computed there, so no new
   parameter is threaded. The clamped value is written back to the signal, not just used, so a
   config reading `scroll("x")` after the pass sees where the list actually landed, not the last
   wheel delta, and a scrollbar built on it cannot disagree with the rows.
5. **A container with no stated extent on the scroll axis does not scroll.** A `Content`-sized
   container's extent equals its viewport, so the clamp bound is zero: a no-op, not an error, the
   same answer `Fill` gives in a `Content` parent (ADR-0023 item 10) and for the same reason, there
   is no remainder.
6. **Pixels when the compositor sends them, a step when it doesn't.** `AxisScroll` carries
   `absolute` (logical pixels), `value120` (120 = one logical step), and a deprecated `discrete`.
   Rule: use `absolute` when non-zero, otherwise `value120 / 120.0` steps of three lines each.
   `discrete` is ignored entirely, it is deprecated and compositors that send it also send
   `value120`.

Consequences: § 5.2 gains its first container that owns a viewport (the larger half of this work).
`input.rs`'s `_ => {}` arm for `PointerEventKind` is deleted; the match is now exhaustive. A surface
whose offset changed produces a different display list and repaints; every other surface compares
equal and does not (ADR-0063), so scrolling one panel does not repaint the bar.

Not built: a scrollbar. Drawing one needs the content extent, which this ADR does not expose; the
first config that wants one decides whether to expose it.

## 0070. A capability starts when the config first reads it

`run_supervisor` used to build every capability controller before reading the config at all, so an
`oblisk` process whose config mentions nothing still claimed three session-wide bus roles, opened
PipeWire, two niri IPC sockets and a dedicated Wayland connection, subscribed to every BlueZ and
NetworkManager device, scanned `/dev/video*` and every `.desktop` file, and polled once a second
forever. Capabilities now start lazily, on first config read.

1. **Reading `oblisk.<name>` is what starts `<name>`.** The `oblisk` table no longer carries its
   capability members directly; they live in a side table, and `oblisk`'s `__index` moves one across
   on first read, sending `RendererFrame::StartCapability` as it goes. The second read finds the
   member already on the table and the metamethod never fires again. Reading is the right trigger
   because it's the only thing a config can do to a capability it uses that it cannot do to one it
   doesn't: `oblisk.audio:invoke(...)`, `oblisk.audio:map(f)`, `computed({ oblisk.audio }, f)` and
   `content = oblisk.audio` all index `oblisk` first, so one hook catches every spelling, including
   ones a future IDL adds. Rejected: an explicit roster (`capabilities { "audio", "network" }`
   beside `fonts { }`), because it's a second list to keep in sync with the first, and drifting
   silently makes a config that reads `oblisk.network` without listing it get a signal that stays
   `nil` forever, indistinguishable from a machine with no Wi-Fi.
2. **Starting is one-way.** A started capability stays started for the life of the Supervisor
   process; dropping the last reader of `oblisk.bluetooth` on reload does not stop the BlueZ
   subscription. Stopping would require releasing a bus name another process may have taken
   meanwhile, draining in-flight requests, deciding the fate of a `last_snapshots` entry and its
   revision counter, and handling a `StateSnapshot` arriving after the stop, to buy back only what
   the process already paid unconditionally before this ADR. YAGNI. Consequence: a config that reads
   `oblisk.privacy` under a one-time-true `if` leaves the camera watch running until the session
   ends.
3. **A generation swap re-sends every start.** `StartCapability` is idempotent on the Supervisor
   side (a name whose controller already exists is logged and dropped), because each generation is a
   separate process with its own Lua VM and `__index`, so a candidate must not inherit the previous
   generation's reads. A candidate reads a `nil` signal for the milliseconds between its first read
   and the newly-started controller's first `StateSnapshot`; that is not new, ADR-0037 already seeds
   every capability to `nil` until first push, and `last_snapshots` replays the current value to a
   capability whose controller was already running.
4. **Construction happens inline on the Supervisor's select loop.** `NetworkController::new` and
   `BluetoothController::new` block on a full device enumeration; running that in the
   `StartCapability` arm stalls the loop for its duration, but that is exactly what the Supervisor
   already did today, unconditionally, before the loop started. Moving it into the loop changes when
   it costs, not what it costs, and frames behind it are delayed, not dropped. The ceiling case is a
   config reading eight capabilities in its first evaluation, paying for all eight serially before
   any pushes. Upgrade path if that matters: `tokio::spawn` the construction and deliver the
   controller back over a channel, at the cost of an `Option` transition per capability that the
   inline form avoids.
5. **`secure_submit` is a read too.** polkit is not a capability: it has no roster entry, no
   `StateSnapshot`, no `oblisk.polkit` member, so decision 1's hook can't see it. A config declares
   it via `secure_submit = { capability = "polkit", action = "authenticate" }` on a `textfield`
   (ADR-0005: a secure submit targets a capability, not Lua). Every applied scene's
   `secure_submit_targets` are started by name through the same deduplicating sender, so a config
   with a polkit prompt registers the agent and one without does not.
6. **Failing to register the polkit agent is not fatal.** `register_agent`, previously the fourth
   statement of `run_supervisor` and propagated with `?`, would abort startup on "an authentication
   agent already exists for the given subject", the normal case on any machine already running
   another desktop; `current_session_subject` one line earlier was equally fatal on a
   `$XDG_SESSION_ID` that pam_systemd hadn't set. Both now log and continue, matching what `tray`,
   `notifications` and `mpris` already do when their name is taken; the agent that loses the race
   simply issues no challenges.
7. **A config may declare no surfaces.** `return {}` and an empty file were both previously refused,
   making "run nothing" untestable and blocking end-to-end testing of decision 1. Zero surfaces is
   now legal: nothing downstream needed changing to support it (`candidate_has_staged` is `all` over
   an empty iterator, `run_pba`'s collection loop is `while collected.len() < expected.len()`,
   `expand_instances` over no specs yields no instances), which is itself evidence the old refusal
   was arbitrary. A zero-surface generation completes its PBA handshake immediately and waits on the
   socket for a reload.

## 0071. The GL context is built by the first surface that needs it

`egl::init` used to run unconditionally near the start of `run`, before the config was known to
declare any surface (a config declaring none is legal per ADR-0070 decision 7). `eglInitialize`
loads Mesa's driver: 12.9-34.7 ms (median 26 ms), pulling in `libgallium` plus LLVM at 125 MB
resident, so a `return {}` config still paid 151 MB RSS / 37 MB PSS for a context it never used.

1. **`App::egl` is an `Option`, built on the first bind.** `ensure_bound` calls `ensure_egl`, the
   only caller of `egl::init`, after its two cheap bails and before `WlEglSurface::new`. `App` holds
   the `Connection` rather than the raw `wl_display` pointer it passes on: the same pointer, but the
   refcount now guarantees `egl::init`'s SAFETY precondition instead of a comment promising the
   connection outlives the state built from it. `release_bound` and `paint_surface` reach `egl`
   through a local and are unreachable for a surface that never bound, so the `Option` does not
   spread further.
2. **A Candidate reaches ready without a GL context.** `bind_and_clear` short-circuits on
   `self.is_pba_candidate || !self.ensure_bound(index)`, since a Candidate stays invisible until
   `ActivateDraw`, its only bind. It therefore never called `ensure_bound` before signalling ready,
   so deferring `egl::init` moves Mesa's load out of the ready window: a Candidate now holds ready
   (measured 104 ms, against a 2000 ms `ready_timeout`) with `libgallium` unmapped. The cost lands
   after `ActivateDraw` instead, where the first bind grows from about 2 ms to about 30 ms, under
   two frames at 60 Hz.
3. **An EGL failure is now fatal later.** Before, `egl::init` failing was a `?` out of `run`, so the
   process died before signalling ready and the Supervisor rolled back to the last generation with a
   working context. Now the failure surfaces from `ensure_egl`, which sets `self.exit` like any
   other bind failure in `ensure_bound`, after promotion, with no rollback left. Accepted: reaching
   it needs a driver replaced or a GPU reset under a live process; the alternative, initializing
   eagerly just to prove it works, is what this ADR removes.

Results: `return {}` RSS fell from 151 MB to 16 MB (PSS 37 MB to 13 MB, the honest number since
Mesa's pages are shared across processes). A 13-surface config runs 183 MB RSS / 59 MB PSS. No test
in `cargo test` catches a regression here: constructing `App` needs a live Wayland connection and
`egl::init` needs a live compositor, so the property is checked by running a real session and
reading `/proc/<pid>/maps` for `libgallium`.

## 0072. A tray item is addressed by the name it registered

Two tray items drew wrong for two unrelated reasons, both fixed together because the same screenshot
found both.

1. **The registered name is the destination, the owner is the identity.** `resolve_registration`
   used to resolve a well-known `service` argument to its owner via `GetNameOwner` and address every
   later message there. Slack's Chromium D-Bus code dispatches property reads on the message's
   destination field, not the owner, so `GetAll` and `Get Id` addressed to the owner (`:1.659`) both
   errored while the same calls addressed to the registered name
   (`org.freedesktop.StatusNotifierItem-1240273-1`) returned 14 properties including a valid 22x22
   pixmap. `resolve_registration` now returns a `ResolvedRegistration` carrying both: `unique_name`
   is the identity (registry key, what `NameOwnerChanged` reports on, what the spool filename is
   built from), `destination` is the address every `Get`/`GetLayout` carries and stays the
   well-known name for a well-known registration. The `GetNameOwner` call stays, since only the
   owner answers the identity half and dropping it would leave an item nothing can ever clean up.
   Accepted risk: a well-known name can move to another owner between the lookup and a later read;
   against an item that is simply never readable, this is the better failure.
2. **`foreground` on an `icon` is the value `currentColor` resolves to.** Telegram's
   `org.telegram.desktop-mute-symbolic` resolves through the icon theme to a Breeze KDE
   colour-scheme SVG using `currentColor`, which Plasma and Qt rewrite at load time but
   `usvg::Options::default()` does not, so Oblisk rasterized Breeze Light's baked-in text colour
   (`#232629`, mean opaque RGB 17/19/20 at 18px) on a dark bar. `icon` now takes a `foreground`,
   meaning what CSS `color` means: `rasterize_svg` rewrites the SVG textually before `usvg` sees it
   (every bare `color:` declaration repointed; the root `<svg>` gets a `color` attribute when the
   file uses `currentColor` but never defines it), since `usvg` resolves `currentColor` while
   building the tree and exposes no hook before that. A file with no `currentColor` is returned byte
   for byte, so full-colour icons are untouched. `CacheKey` gains the colour, otherwise the first
   tint drawn wins for the life of the process. ADR-0031's preference for `IconName` over
   `IconPixmap` stands: Telegram also ships a 16x16 pixmap, and upscaling it to avoid recolouring a
   vector is the wrong trade.

Not built: a real CSS parser. `color:` is matched textually across the whole file, so one inside a
comment or attribute value would be rewritten too; no theme file tested has one.

## 0073. The tray host asks the bus what is already there

A tray item registers with the watcher once, at application startup. Restarting Oblisk left the
watcher new and empty, and applications that don't re-register on `StatusNotifierHostRegistered`
(Slack does not) stayed invisible until restarted themselves, which a shell doesn't get to demand.

The decision: `TrayController::new` calls `ListNames` at startup, keeps names matching
`org.{kde,freedesktop}.StatusNotifierItem-` (both spellings, since KDE's is the de-facto name and
Chromium claims the freedesktop one), and runs each through the same `resolve_registration` plus
`register_item` path a live `RegisterStatusNotifierItem` call takes, serially rather than joined (a
session has a handful of tray items). The `StatusNotifierHostRegistered` signal stays; an
application that does re-register overwrites its own entry at the same registry key.

What this cannot find: an item that registered an object path without owning a well-known
`StatusNotifierItem-*` name, since nothing on the bus says which connections export the interface
without asking each one in turn. Vesktop is that shape but doesn't need the scan; it listens for the
watcher and re-registers on its own. So the scan happens to cover the applications that need
covering, which the ADR records as luck rather than design. Introspecting every connection at
startup was rejected as dozens of round trips at startup looking for something usually not there.

Adoption running before `spawn_name_owner_changed_forwarder` creates no new race: the forwarder
subscribes inside its own spawned task regardless of ordering, and an item disconnecting
mid-adoption is caught by `register_item`'s existing pre-insert liveness check, the same guard that
already covers a live registration's identical window.

`is_item_bus_name` carries the tests: both name spellings match, the watcher's own name and
`StatusNotifierHost-1234` do not, and the trailing `-` is the whole guard against a name that merely
starts the same way.

## 0074. The tray backend exposes what the spec defines

An audit of the tray against `org.kde.StatusNotifierItem` found six gaps. Scoring them by whether
`dev-config` used them marked four as YAGNI; that test is wrong because Oblisk is a framework and
the Supervisor is its API, so the right test is whether the spec defines a feature and applications
implement it. Rescored, five of six are in (Telegram, Vesktop and Slack all export the relevant
methods and properties on a live session; only the values were empty).

1. **Delete a spooled icon when its item goes.** `write_icon_png` wrote
   `/dev/shm/oblisk-$UID/tray/{unique_name}.png` and nothing deleted one; since `/dev/shm` outlives
   the process and a reconnecting app gets a new unique name, every restart left a file resident
   until reboot. `NameOwnerChanged` removal now deletes an item's PNGs, and `TrayController::new`
   sweeps the directory on startup. Accepted risk: two Supervisors running at once means the second
   sweeps the first's live files (blank icons until re-spool), a debugging accident rather than a
   mode, against a leak measured in kilobytes.
2. **`Passive` is the config's call, not the backend's.** The spec's "likely that visualizations
   will chose to hide it" is a presentation recommendation, not a data rule, so `TrayItem.status`
   reaches Lua and `sys_tray.lua` filters while the backend carries every item. Deliberately the
   opposite of ADR-0031's `should_call_activate`, which is centrally enforced because a wrong click
   has a side effect on another process; hiding an icon has none.
3. **All three icon variants are carried, none are composited.** `TrayItem` gains
   `attention_icon_{name,path}` and `overlay_icon_{name,path}`, resolved through the same pipeline
   as the base pair, each spooling to its own filename (`{name}.png`, `{name}-attention.png`,
   `{name}-overlay.png`) so the three don't collide on one path. The Supervisor doesn't apply them:
   whether `NeedsAttention` swaps the icon is a bar's presentation choice, and overlay compositing
   needs a `stack` node, which is a canvas the Supervisor doesn't have.
4. **`SecondaryActivate` and `Scroll` join `TrayAction`.** Middle-click and scroll-over-icon,
   exported by Telegram, Chromium and Qt's own tray; without them no config can express either.
   `SecondaryActivate` gets no `should_call_activate` gate, since `ItemIsMenu` governs only the
   primary click. `Scroll` gets its own argument parser: its `[id, delta, orientation]` would
   misread as `Activate`'s `[id, x, y]` if shared, silently dropping every scroll; the orientation
   string passes through unvalidated since interpreting it is the application's job.
5. **`IconThemePath` resolves in the Supervisor.** An application bundling artwork the session theme
   doesn't know previously resolved to a name the renderer couldn't find. With a theme path set, the
   Supervisor now tries `{path}/{icon_name}.png` and `.svg` and returns a hit as `icon_path`
   (already an absolute path), needing no renderer or IDL change. It outranks `IconName` when it
   hits. A name containing a path separator is refused rather than sanitized, since a themed icon
   name never contains one.

Not built: `AttentionMovieName` (KDE3-era, nothing sets it), `WindowId` (X11), `Category` (no bar
here sorts by it).

Correction recorded: the first audit pass rejected decisions 3-5 as speculative because
`sys_tray.lua` calls no tray action, which measures one config rather than the surface every config
gets. Worth remembering for any capability whose only in-repo consumer is `dev-config`.

## 0075. Compositor detection is session-level, and `workspaces`' seam is a file

ADR-0034 put `CompositorKind` and the compositor probe inside
`supervisor/src/hardware/keyboard/layout.rs`, alongside `CompositorLink`. ADR-0056 gave `workspaces`
no trait, reusing that same probe, which left `workspaces/controller.rs` importing from a sibling
capability and `niri_ipc`'s types as the input of the capability's one pure function. This ADR
reverses neither prior decision; it is the preparation ADR-0056 named, done while there is still one
implementor because it only gets more expensive with a second.

1. **The probe moves to `supervisor/src/compositor.rs`.** Which compositor is running is a
   session-level fact, not something `hardware::keyboard` should own. `CompositorKind` and
   `detect_compositor` move to a new top-level `compositor` module; `hardware/keyboard/layout.rs`
   keeps `CompositorLink` and its two implementors. A pure move, behaviour unchanged.
   `CompositorLink` deliberately does not move with it, since ADR-0056 settled that the trait is
   keyboard-layout-shaped and putting it next to the probe would suggest otherwise.
2. **The probe is a table, and an unsupported session gets named.** `detect_compositor` becomes a
   `PROBES` table of `(kind, env var)` in probe order instead of an `if`/`else` chain, so precedence
   is read rather than inferred; a test asserts every `CompositorKind` has an entry and vice versa,
   with an exhaustive `match` that fails the build on an unmatched variant. `$XDG_CURRENT_DESKTOP`
   is deliberately excluded: it's a name set by whatever launched the session even when the
   compositor never came up, unlike every `PROBES` var, which its compositor sets because it is
   running. A shared `unsupported_session_report()` now names the session ("this session is sway,
   which has no implementor") instead of each capability printing its own unhelpful message.
3. **`derive_state` takes rows, not `niri_ipc` types.** ADR-0056's "don't write a trait for one
   implementor" is sound; that it also made `niri_ipc::Workspace`/`niri_ipc::Window` the reduction's
   input type was a separate, costlier call, since none of `derive_state`'s judgement (grouping,
   ordering, the optional `focused_workspace`, `app_id`-into-`class`) is niri-specific logic, only
   niri-specific types. `derive_state` now takes `&[WorkspaceRow]` and `Option<&FocusedWindow>`;
   `workspaces/niri.rs` is the only file naming `niri_ipc`, mapping niri's state onto those rows.
   The split follows what varies: which window holds focus is the adaptor's question (niri flags it
   per window; another compositor may query separately), what that window becomes is neutral and
   stays in the reduction. `StatePublisher` (reduce, drop a no-op update, store, wake `main.rs`) is
   the write half of the same seam, eight lines every adaptor would otherwise copy.
4. **The seam is a module boundary, and stays one until a second implementor is live.**
   `WorkspacesController` still matches `CompositorKind` rather than holding a `Box<dyn>`; a trait
   with one implementor is still Speculative Generality, and Hyprland's per-monitor-active versus
   niri's global-focus models still don't map by renaming fields. What changed is that the line a
   trait would sit on is now a file boundary: a second compositor is a sibling module plus two
   exhaustive-match arms, inheriting the reduction, the publish contract and their tests.
   `derive_state`'s ten tests build rows directly and name no compositor; `niri.rs`'s tests keep
   ADR-0056's wire-JSON fixtures, deserialized from a live `niri msg -j` rather than struct
   literals, so a niri field rename breaks them.

Not decided: whether the eventual trait is one trait or two, left for the commit that adds a second
live-tested compositor.

## 0076. The capability roster is a type, and the module tree mirrors it

Replaced the hand-kept `CAPABILITIES: &[&str]` list and sixteen separate controller locals in
`main.rs` with a `shared::Capability` enum and a `Capabilities` struct, and reorganized the
Renderer's capability modules to mirror the roster one-to-one.

1. **`shared::Capability` replaces the string roster.** The roster is an enum generated by one
   `roster!` macro (produces `ALL`, `as_str`, and the enum). Both `main.rs` matches became
   exhaustive, so a new variant fails the build at exactly the two arms needing code.
   `snapshot::push_snapshot` now takes `Capability`, replacing ADR-0037's `debug_assert` with a type
   that makes an off-roster name unrepresentable. `idle` and `polkit` stay off the roster: `idle` is
   event-shaped, not snapshot state (ADR-0032); `polkit` arrives via `secure_submit` naming it, not
   a capability read (ADR-0070 decision 5).
2. **`Capabilities` owns the sixteen controllers, `main.rs` owns the engine loop.**
   `Capabilities::new` builds every channel and returns the receiving half as `Signals`; `start`,
   `push`, and `dispatch` are the three things a capability does. `main.rs` drops from 1081 to about
   760 lines. The split between `Signals::next` (only awaits `recv()`) and `Capabilities::push`
   (runs in the winning `select!` arm's body) is load-bearing: `network` and `bluetooth` await while
   building state, and folding that await into the raced future would let a busier branch drop a
   signal mid-flight. This is not ADR-0037's rejected merged channel: channels stay sixteen typed
   single-variant ones, and ADR-0037 decision 3's "static calls, no registry, no trait" dispatch is
   unchanged.
3. **One module per roster entry, flat, under `capabilities/`.** The old `dbus/` and `hardware/`
   transport-based groupings are dissolved (`battery`/`power` were the same subject split by
   transport, not something a config author can see). Shared helpers moved to `capabilities` itself
   (`read_attr`, `parse_bool_arg`); `shm_icons` sits beside `tray` and `notifications`, its only two
   users. `polkit` was never a capability and moved to top-level `crate::polkit`. `lock` keeps one
   asymmetry: `LockController` is built at boot, not started on demand, because the Supervisor's
   relock path (ADR-0060) commands it before any config is read, so `Capabilities::dispatch` is
   handed it rather than owning it.

A 2026-09-01 follow-up found the roster type still had a gap: adding a capability guarded the Lua
namespace, stubs, and schema check, but not the channel wiring (`Signals`, `Senders`,
`Signals::next`, and `Capabilities::new`'s pairs were four more hand-written lists).
`capabilities::capability_channels!` now derives all four from one list and emits an
exhaustive-match check so a roster variant with no channel and no stated exception fails to build.
`lock` is the one stated exception (ADR-0060, ADR-0052). `Signal` itself stays hand-written: its
variants document which payload each capability carries, and `push`'s exhaustive match already
guards it.

Not built: reorganizing the Renderer's largest files (`layout/scene.rs`, `wayland/surface.rs`,
`wayland/input.rs`); each was judged cohesive on its own and left alone.

## 0077. The layout math is taffy's, not this crate's

Supersedes ADR-0023's hand-written one-pass layout solver: taffy now owns sizing and positioning.
ADR-0023 items 1-9 and 12 (what Phase 12 did not build) are untouched; this reverses item 4's
arrangement formula, item 10's one-pass budget, item 11's un-repositioned descendants, and the "one
recursive function doing all three passes" decision, because item 11 was a defect ADR-0023 itself
named and no later ADR fixed.

1. **`taffy` 0.14 owns sizing and positioning; `layout::scene` owns everything else.** `scene.rs`
   keeps node identity and reconcile (ADR-0045), the lease and child-first teardown, the depth cap,
   once-per-node property resolution, the scroll clamp and writeback (ADR-0069), and text elision. A
   pass is now `prepare` (resolve/parse each node once, build one taffy node per node), `solve` (run
   taffy), `finish` (read geometry back, apply scroll offset, elide text). `taffy_style` is the only
   place that knows what `row` or `Fill` means: `row`/`column` are `Display::Flex`; the stacking
   model (ADR-0023 item 4) is `Display::Grid` with every child pinned to row 1/column 1, one
   auto-sized cell, each child aligned independently, container sized to their bounding union;
   `Fill` along the flow axis is `flex_grow: 1.0` over a zero basis, elsewhere it is a stretch;
   `spacing`→`gap`, `visible = false`→`Display::None`. `flex_shrink` and `min_size` are forced to
   zero (this engine has no shrink concept); rounding is disabled since `text::snap` handles pixel
   snapping at paint time. Dependency count goes 151 to 152 (arrayvec/slotmap/smallvec were already
   present); only `flexbox`, `grid`, `taffy_tree`, `std` features are on. `content_size` was tried
   and dropped: taffy's `scrollable_overflow_rect` excludes children's margins but this engine's
   footprint includes them (amendment on ADR-0069), so the scroll extent is summed here instead.
   `scene.rs` loses 78 lines; the change is a net deletion. The measure callback answers only `text`
   and `icon` sizing, memoized on `(text, size, wrap width)` via `ShapingHandle` so repeats never
   cross the channel.
2. **Item 11 is fixed, item 10 is not.** A `Stretch` child of a `Content`-sized parent now correctly
   relayouts its descendants after the parent's size resolves (item 11's bug). A `Fill`/`Percent`
   child of a `Content`-sized row still resolves to zero (item 10), because that is the same answer
   CSS gives an indefinite container: no regression, and now backed by a specification rather than a
   comment.
3. **Two behaviours change deliberately; one that could have does not.** An invisible node
   (`Display::None`) now leaves the layout entirely, with no size or no `spacing` gap reserved,
   where the old pass resolved its geometry and then declined to place it; nothing outside
   `layout::scene` can observe the difference. Property getters now fire in plain depth-first
   declaration order (no more multi-round recursion), matching the existing guarantee that every
   getter fires exactly once in config-write order. A `Stretch` alignment still outranks an explicit
   size on the same axis, even though CSS would apply stretch only to an `auto` cross size, because
   changing that is a config-facing question separate from the solver swap and was left alone.
4. **The depth cap stays at 64, on a better measurement.** The old comment modeled worst-case stack
   depth from this module's own recursion only, excluding mlua frames, and was optimistic. Measured
   by shrinking a thread's stack to the crash point: the old hand-written pass needed about 1,400
   KiB for the worst case (cap depth with a 31-deep computed `margin` chain), a 1.44x margin on the
   2 MiB debug test thread, versus the 2.6x the old comment claimed. The solver's same worst case
   peaks at about 1,040 KiB (1.97x margin); a 64-level tree with no signals costs about 590 KiB
   (~8,960 B/level). A `taffy::Style` is 552 bytes, built and consumed in a frame that returns
   before recursion descends, rather than multiplied across the cap.

Not built: reusing one `TaffyTree` across passes with dirty-node marking, which is what taffy's
per-node cache is for. Currently a tree is built and dropped inside each `apply_one_instance` call
(why a failed walk needs no extra rollback). Not built because nothing has measured a need, and
because a tree that outlives a pass needs reconciliation against the retained tree, a second
identity problem beyond the one ADR-0045 solved.

## 0078. `exclusive` is three answers, not a boolean

`exclusive` was typed `boolean`, but `zwlr_layer_surface_v1::set_exclusive_zone` has three distinct
meanings (a positive zone reserves that much; `0` reserves nothing but still positions inside what
others reserved; `-1` reserves nothing and ignores what others reserved, covering the output), and
the boolean could reach only the first two.

This was caught live: `dev-config`'s wallpaper is a `panel` on `Background` anchored to all four
edges with `exclusive = false`. At startup it filled the 1920x1200 output; once the bar mapped and
claimed 39px, niri reconfigured it to 1920x1161 and it sat below the bar instead of behind it. No
boolean value fixed this: `exclusive_zone_for` answers `0` for a surface anchored to all four edges
(no single edge to reserve against), so `true` and `false` were the same request on exactly the
surface that needed the third answer.

1. **`exclusive` accepts `boolean` or `"Ignore"`.** `true` reserves along the anchored edge; `false`
   (default) reserves nothing but stays inside what others reserved; `"Ignore"` reserves nothing and
   ignores what others reserved. Parsed into `node::Exclusive { Reserve, Respect, Ignore }`, one
   variant per protocol case, mapped by `apply_exclusive_zone` to the derived zone, `0`, and `-1`.
   Additive, not a migration: `true`/`false` keep their existing meanings, so only `dev-config`'s
   wallpaper needed editing. `boolean / string` rather than a pure enum matches this IDL's existing
   shape for a scalar with one special case (`width`/`height` are `integer / string`, parsed by
   `parse_size_mode`); spelling it as a three-way string enum would read more uniformly but breaks
   every existing config for no gain. Named for what each does (`Reserve`/`Respect`/`Ignore`), not
   for the number sent: `Reserve` and `Respect` are the two non-ignoring answers, and the pairing is
   what makes `0`'s meaning legible.
2. **The deferred placeholder stays `Respect`.** `exclusive` is not a structural property, so a
   `Signal` in it resolves normally at layout time, but it is read twice: `socket::surface_specs`
   reads the raw map before any getter runs (placeholder value), and `App::apply_resolved_state`
   re-reads the resolved tree. `Respect` is the only one of the three invisible for the frame it
   lasts: `Ignore` would paint a wallpaper over the bar until corrected; `Reserve` would shove every
   window aside.
3. **An unknown string fails the pass.** `exclusive = "ignore"` or `"None"` are refused by name
   rather than silently read as `Respect`, the same protection `NODE_PROPERTIES` gives against key
   typos (before it existed, `aling_v = "Center"` was silently accepted and read by nobody). A value
   typo deserves the same treatment as a key typo.

Rejected: a numeric zone like Quickshell's `exclusionMode` + integer `exclusiveZone` (where setting
the integer implicitly flips the mode to `Normal`), because nothing needs a custom reserved amount
and the implicit mode flip is the part worth not copying; renaming to match Quickshell's
`Auto`/`Normal`/`Ignore` (`Auto`=`Reserve`, `Normal` with zone 0 = `Respect`, `Ignore`=`Ignore`
exactly), because these names describe what the surface asked for rather than how the number was
derived, only `Ignore` is shared since its meaning is the protocol's own.

Not built: bumping the IDL minor version. The versioning rule only starts at the first push to
origin (ADR-0069 set this precedent adding `scroll` to the same spec section); 0.1.0 is a
placeholder until then.

## 0079. A rounded clip is an offscreen pass, not a rounded scissor

An earlier phase shipped only a rectangular clip, leaving femtovg's `intersect_rounded_scissor` as
the intended upgrade path; that path does not work, and a real offscreen-composite pass replaced it.
The bug it was left for: `dev-config`'s battery indicator is a pill whose fill child should be cut
by the pill's rounded clip, but a rectangular clip cut it square, and the config's workaround
(giving the fill the pill's own radius) rounded the fill's right edge too, drawing a lozenge instead
of a filled arc.

1. **`clip = "Rounded"` renders its subtree offscreen and composites it through the node's path.**
   `layout::paint::build` emits a `Draw::Clipped` group holding the node's children; `execute`
   allocates an offscreen image sized to the node's clip rectangle, draws the group into it, then
   fills the node's rounded path with that image as the paint, so the path itself is the mask.
   `Draw::Clipped` is the one recursive variant in the display list; every other clip is an
   axis-aligned rectangle, which is what lets each `DrawCmd` carry one flattened `clip` instead of a
   save/intersect/restore nest (ADR-0063 relies on the display list staying comparable, and
   `Draw::Clipped` still derives `PartialEq`). The rounded case emits three commands where the
   rectangular case emits one (fill, group, border), because the border must paint over the clipped
   content, matching QML's `ClippingRectangle`.
2. **femtovg's `intersect_rounded_scissor` is not a real alternative.** femtovg 0.26 keeps exactly
   one scissor in its canvas state, a single rounded rectangle, so it cannot represent "this
   rectangle intersected with that arc." Intersecting a rectangle into an existing rounded clip
   takes one of three branches: keep the rounded clip, drop the radius, or re-round the intersection
   with the old radius; a part-width child of a pill takes the third, which re-rounds the child's
   own box into the same lozenge artefact. Measured on an 80x32 pill at radius 16 with a child
   filling its left 30px: the scissor route bled the ground through at 8% along the pill's straight
   top edge where the offscreen-composite route did not.
3. **`radius` does not imply a rounded clip; a config asks for one.** `clip` takes `"Box"` (default,
   a plain scissor rectangle the GPU applies for free) or `"Rounded"` (an offscreen render target
   plus a composite per clipping node per repaint). Most rounded boxes on this bar have no
   overflowing child, so charging all of them for a pass none of them need is the wrong default;
   QML's own `Item.clip` is rectangular and ignores `radius` for the same reason. femtovg fills a
   path directly with an image paint, so one render target suffices, versus Quickshell's
   `ClippingRectangle`, which needs two because its mask must be a texture for a fragment shader to
   sample. A childless node with `clip = "Rounded"` costs nothing: there is no group to render.

Not built: hit testing does not know about the arc (`layout::hit` intersects the same rectangles, so
a pill's corner, about 4px on a 34px control, is outside the fill but still clickable); the honest
fix is hit testing sharing paint's walk rather than a second rounding rule, and nothing has asked
for it. The offscreen image is allocated and freed per clipping node per repaint, after the flush
that consumes it, since femtovg records draw calls and executes them at flush; a size-keyed pool
next to `ImageCache` is the upgrade path once a config puts a rounded clip on something repainting
at pointer rate. Today the bar only repaints on signal change (ADR-0063).

## 0080. The battery comes from UPower, not sysfs

Replaces the earlier sysfs/udev-based battery watch and the `charging: boolean` field in § 2.2:
`oblisk.battery` now reads UPower's `DisplayDevice` over D-Bus and follows its `PropertiesChanged`,
with no sysfs, no udev, and no timer. Two separate bugs from the same bar drove this, and one source
fixes both.

The reading was stale: the pill's percentage sat still while the machine discharged, because
`run_battery_task` read `/sys/class/power_supply` only when udev fired and fell back to a 30s poll
only if the watch failed to build, wrongly assuming a watch that builds also fires. Measured
alongside `udevadm monitor --udev --subsystem-match=power_supply`: `capacity` moved from 69 to 65
over the window, but 0 `power_supply` uevents were delivered, while UPower's own view tracked every
point. The ACPI driver on this hardware emits a uevent only on plug/unplug.

A boolean could not represent what was happening either: `charging` was `status == "Charging" ||
status == "Full"`, but sysfs's five status words cover states a charge-threshold laptop actually
visits, and three of them collapsed into `false` while one said `true` for a battery that was not
charging (`Not charging` at the limit on mains, and `Discharging` while draining down to the limit
on mains, both need the mains adapter's `online` bit read and combined with status, which is
UPower's own `up-device-supply` logic reimplemented from the same files). On this machine
`charge_control_end_threshold` is 70, so that misreported state is most of every day.

1. **`DisplayDevice`, and the states by name.** `BatteryStatus` maps UPower's `Device.State`
   numbering to names (`Charging`, `Discharging`, `PendingCharge`, `PendingDischarge`,
   `FullyCharged`), serialized as a string so a config compares `b.state == "PendingCharge"`, the
   same shape `mpris`'s `play_state` already uses. An unrecognized future state reads as `"Unknown"`
   rather than failing the capability. `present` is `Type == Battery && IsPresent`, both halves
   (matching Quickshell's `isLaptopBattery` check; `IsPresent` alone is true on non-battery
   hardware). `time_to_empty`/`time_to_full` come from the same `GetAll` call and are `nil` when
   UPower reports `0` (while charging or before it has estimated). The proxy addresses the
   well-known fixed path `/org/freedesktop/UPower/devices/DisplayDevice` directly rather than
   calling `GetDisplayDevice()`, and takes one `PropertiesChanged` subscription for the whole object
   rather than one per property, so a percentage and a state flip that happen together arrive as one
   message. Checked against Quickshell's own source (`core.cpp`/`device.cpp`): no timer anywhere,
   because UPower does the polling and every client inherits it.
2. **No sysfs fallback.** A host without UPower prints one line and reports nothing, the same
   pattern § 2.13 uses for a missing power-profiles-daemon and ADR-0053 established for any
   capability with no implementor. A sysfs fallback was considered and dropped: it cannot fill the
   payload it would be falling back for (no `PendingDischarge` without also reading the mains
   adapter, no `time_to_*` at all), and it is the same path measured above as blind to changes the
   kernel does not announce. A fallback that reports a worse answer under the same field names is
   harder to diagnose than no answer.

Not built: 0% glitch suppression (holding the last percentage when UPower reports a spurious 0% on
AC) is not written, because it has not been reproduced on this machine; the fix is known (about six
lines, keyed on state not being `Discharging`) if it is seen. The charge threshold itself is not in
the payload: `charge_control_end_threshold` is a sysfs file UPower does not expose, so a config can
say "charge limit reached" but not the percentage; it is one `read_attr` away if needed.
`brightness` keeps its udev watch unchanged since it was confirmed firing when written, and logind,
not the kernel driver, is its write path.

## 0081. The stubs are checked against the config, not just parsed

`just check` now runs `lua-language-server --check` over `dev-config/oblisk` and `share/starter`, so
`lua-meta`'s declared types are checked against real config code instead of only being parsed. This
amends `lua-meta/nodes.lua`'s header, which had argued that spelling `Signal` into every property
union would drown the useful types.

Three existing guards (`luac -p` proving a file parses; `meta_stub_tests` proving every node kind is
declared, its `---@field` names match `accepted_properties`, and every parser-read property is
accepted by some kind; `supervisor/src/stubs.rs` proving the generated half matches its payload
types) all checked names, never a declared type. `.luarc.json` already pointed the language server
at `lua-meta`, so editors were already running this check and discarding the answer.

That gap let a wrong type stand: the header claimed every property takes `Signal` whether or not its
union said so, as a stated readability trade. A generated probe binding `Signal` to all 192
kind/property pairs found 28 properties that reject a `Signal` the engine accepts (7 of those are
callbacks where the union would be true but useless); 21 were fixed by spelling `|Signal` on
`radius`, `spacing`, `font_size`, `align_h`, `align_v`, `clip`, `elide`, `fit`, `size`,
`text_align`, `direction`, `exclusive`, `keyboard_interactivity`, `app_id`, `parent`, `grab`,
`gravity`, `placeholder`, `mask_character`, and a popup's `width`/`height`. Since `lua-meta` is what
the language server reads, an omitted union member was a red squiggle under working config code, and
the cost fell on whoever wrote that code.

Two sets still correctly name no `Signal`: structural properties (`id`, `hover`, `scroll`, a panel's
`layer`/`anchor`/`monitor`/`namespace`) are copied raw rather than resolved because they are
identities, so the engine truly refuses a `Signal` there; callback properties (`on_click`,
`on_change`, `on_submit`, `on_close`, `on_dismiss`, `itemfn`, `key`) resolve normally and are then
refused for not being a function, so `fun(...)|Signal` would be true but would only worsen
completion.

Generating `nodes.lua`/`surfaces.lua` from Rust was considered and rejected. The names are already
data (`NODE_KINDS`, `COMMON_PROPERTIES`, `BOX_PROPERTIES`, `NODE_PROPERTIES`), but the types and
prose live in 49 `parse_*` functions and 45 `properties.get` call sites across ten files, so
generating would only move a hand-written claim from a `.lua` file to a `.rs` file. A genuinely
derived version would need rewriting the parse layer to per-kind structs the parsers read fields
off, trading this crate's per-property error messages for serde's; that rewrite was not undertaken.
Instead, `every_type_the_stubs_declare_is_accepted_by_the_engine` builds `kind { property = <sample
of the declared type> }` for all 408 declared type members and runs a real `Scene::apply`, so a type
with no sample fails the test rather than passing quietly.

The engine test and `just types` catch opposite gaps and neither subsumes the other: the engine test
catches the stub promising what the engine refuses, by feeding each declared type to `Scene::apply`;
`just types` catches the stub omitting what the engine accepts, by checking real config code. The
engine test cannot do the second direction, since trying every undeclared sample and asserting
refusal collapses on aliases (`width` is `Length|Signal` but accepts a bare `integer`; `content` is
`string` but accepts a `Color` sample because a hex colour is a string).

Measured value: `dev-config` returned six diagnostics, all in `components/icon_button.lua` and none
a stub bug (three locals holding either a colour string or a `Signal`, disambiguated by a runtime
`type(x) == "userdata"` test since LuaCATS has no user-defined type guards; fixed with four
`---@type` and five `---@cast` annotations). The check earns more on the 174 generated payload
fields than on node properties: a typo like `b.percentt` in a `:map` callback is now a build failure
naming the field, where before it silently rendered a blank pill. The check is optional in `check`,
since `lua-language-server` is not a build dependency and there is no CI installing it; if absent,
it is skipped with a line saying so, never a silent pass.

`lua-meta` is now checked as its own workspace rather than loaded as a `workspace.library` for
`dev-config`/`share/starter`, because a library's own diagnostics are suppressed. That had hidden a
real fault: LuaCATS' `---@return T a, b` declares two returns, so a comma inside single-return prose
makes the next word a type, and `---@return Signal Read-only, like \`map\`.` had declared a return
of type `like`, invisible because checking the config correctly found nothing wrong with the config.
`lua-meta`'s files declare everything they reference, which is what lets them stand alone as a
workspace; single-return prose now uses `---@return T # ...`, LuaCATS' explicit comment marker.


## 0082. `oblisk.network` is subscribed to the association, not just to the scan

`NetworkState` carries the whole of docs/oblisk-idl-api-specs.md §2.5, and the forwarders watch the
NetworkManager properties that move when a link comes up. ADR-0029 left "the exact `NetworkState`
struct shape" open and the first implementation answered it with `scanning` plus the AP list; this
is the rest of the answer, forced by a bug that shape could not avoid.

1. **The AP list is not a connectivity source.** `available_networks[].active` was the only thing
   saying whether the machine was online, and it is wrong for that job three ways: a wired link
   never appears in it at all, a powered-down radio is indistinguishable from a powered one joined
   to nothing, and an association that has not yet won the default route reads the same as a working
   one. `connected` now comes from NetworkManager's `PrimaryConnection`, which names the active
   connection holding the default route (`/` when nothing does) — §2.5's "default gateway interface
   is active", read literally off the one property that means it.
2. **The wake-ups were the bug, not the dedup.** The forwarder subscribed to `AccessPointAdded`,
   `AccessPointRemoved`, and `LastScan`. None of the three moves when the radio joins or leaves a
   network, and on a connected idle machine none of them fires at all: measured on this hardware,
   90 seconds of an established association produced zero. Associating after the bar started left
   `active` false until the next scan happened along, minutes later. `Wireless.ActiveAccessPoint`,
   each device's `Device.State`, and the manager's `WirelessEnabled`/`NetworkingEnabled`/
   `PrimaryConnection` are now subscribed too. This is what Quickshell's own NM backend binds
   (`src/network/nm/`: `Network.connected` tracks the active connection's state, never AP identity),
   and the same conclusion arrived at from the other end.
3. **One `Changed` variant, not one per source.** Every added subscription ends in the same full
   re-derive, since ADR-0029 item 6 already refuses to keep incremental state. `NetworkSignal`'s
   `AccessPointsChanged` became `Changed` rather than growing five siblings that all mean the same
   thing to `handle_signal`.
4. **`ssid` names the association; `connected` answers the route.** `"Ethernet"` when the default
   route is wired, the joined SSID otherwise, `nil` when nothing is joined — so a network still
   negotiating DHCP has an `ssid` and a `connected` of `false`. Wired wins over a simultaneous Wi-Fi
   association, because `ssid` has to name the link `connected` is about, and a docked laptop stays
   joined to Wi-Fi the whole time it is on a cable.
5. **`ethernet_enabled` is device state, not link carrier.** §2.5 words it as the carrier, but the
   carrier is up whenever a cable is seated, which would leave `set_ethernet_enabled(false)` (ADR-
   0029 item 5: `Device.Disconnect()`) looking like it did nothing. Reporting `ACTIVATED` is the
   read-back the setter's own toggle needs, the same kind of deviation §4.1 already documents for
   `NetworkingEnabled` being read-only and `Enable()` being the real switch.
6. **No startup read.** zbus emits a property stream's current value once when the cache first
   fills, so the forwarders prime the first snapshot on their own; a separate build-and-push at
   construction would only duplicate it.
7. **`AccessPoint.Strength` is watched on the associated AP only, and needs no debounce.** Measured
   over 180 seconds on this hardware: the associated AP emitted 26 times, a 6-second poll that stays
   quiet while the number holds, against 76 emissions across all 17 APs in range — one every 2.4
   seconds, indefinitely, for percentages behind a panel that is closed almost always. A rebuild
   re-reads every AP's strength regardless, so the association's own clock refreshes the whole list
   at a third of the traffic. One rebuild per ~7s is also the answer to ADR-0029 item 6, which said
   to reconsider debounce only against real numbers: these are the numbers, and they do not justify
   it. The watch is re-targeted on every `ActiveAccessPoint` change and the previous task aborted —
   an orphan would go on asking for rebuilds for an AP nothing is connected to.

8. **Access-point proxies are kept warm; the connected AP is sorted ahead of the cut.** Two things
   the rebuild got wrong once it started running every ~7 seconds rather than once a scan.

   Binding a fresh `AccessPointProxy` per access point per rebuild made zbus set up a property
   cache each time — a match rule, a `GetAll`, an unsubscribe — and throw it away at the end of the
   loop body. Measured at 10 access points: 11.25ms a rebuild for fresh-and-cached, 9.51ms for
   fresh-and-uncached, 0.84ms for proxies held across rebuilds. 13x, and it is spent inline in
   `main.rs`'s `select!` arm where ADR-0028 already warns about unbounded awaits. The proxies now
   live in the controller keyed by object path, pruned against the live path list each rebuild
   rather than by watching `AccessPointRemoved` — the same signal that asks for the rebuild anyway.

   Separately, `build_state` reads `ssid` and `strength` out of the deduplicated list *after* it is
   cut to 20, so an association weaker than 20 neighbours was truncated away and a plainly-online
   machine reported as joined to nothing. Dense apartment RF reaches 20 SSIDs easily. `active` now
   sorts ahead of strength, which keeps the connected network inside the cut and also makes the
   payload order the one a panel wants, so `network_panel.lua` no longer copies and re-sorts the
   list to arrive back where it started.

Not built: multi-adapter selection and hot-plugged device discovery, both still open from ADR-0029's
module header.


## 0083. `network:connect` reuses a saved profile, and the AP order is deterministic

A second pass over the same two references ADR-0082 came from — the Quickshell `NetworkService.qml`
this shell mirrors and quickshell-mirror's `src/network` backend — against the finished
implementation. Two of the differences were real defects on our side.

1. **Connecting created a profile every time, saved or not.** `connect_inner` called
   `AddAndActivateConnection2` unconditionally. NetworkManager does not deduplicate: it accepts a
   second profile with the same `id` *and* the same SSID without complaint, confirmed by adding
   `oblisk-dup-test` twice and getting two UUIDs back. So every re-join from the panel left another
   copy behind, and autoconnect could later pick a stale one over the good one. The QML does not
   have this bug because it asks `wifiNetworkForSsid(target)` first and only creates for an SSID
   the machine has never seen.

   `activate_intent` now takes the same shape: a saved profile for the SSID gets
   `ActivateConnection`, and only an unknown SSID reaches `AddAndActivateConnection2`.

2. **A password typed for a saved network is written back, not dropped.** Reusing the profile
   raises a question creating one never had: what a re-typed password means. Ignoring it would make
   a profile saved with the wrong key unfixable from the panel — forget-then-rejoin would be the
   only route — so it goes to `SettingsConnection.Update` first.

   Via `Update` rather than delete-and-recreate because `Update` replaces the whole profile and
   every other section survives it. The three Wi-Fi profiles on the development machine each carry
   `ipv4.address-data`, `route-data` and `802-11-wireless-security.auth-alg`; recreating would drop
   all of it to fix a typo.

   ponytail: skipped for enterprise profiles. `GetSettings` omits secrets, so rebuilding an 802.1X
   profile from its own read-back would drop the stored password with it. NM's copy is the better
   bet until there is a secret agent to answer for one, which ADR-0029 leaves out of scope.

3. **The AP sort had no tiebreak, over an input with no order.** `dedup_and_top20` sorted on
   `(active, strength)` with a stable sort, and its input is a `HashMap` drain whose order moves as
   access points come and go. Two APs at one strength swapped rows between rebuilds for no reason,
   and a tie across the 20th place decided arbitrarily which one was cut. The QML's comparator ends
   in `localeCompare(ssid)` for exactly this; ours now ends in the SSID too.

4. **Tier-based ordering was measured and rejected.** The QML sorts on `signalTier`, not raw
   signal, and says why: "scan-to-scan jitter cannot reshuffle the list under the cursor." Worth
   copying on its face, since ADR-0082 took rebuilds from once-a-scan to every ~7 seconds. Reading
   `AccessPoint.Strength` off the bus eight times over 56 seconds says otherwise: every neighbour
   held a single value (swing 0) and only the associated AP moved (swing 3, 60..63), because
   NetworkManager refreshes a non-associated AP's `Strength` only at scan boundaries. The one that
   does move is pinned to row 0 by `active` already. Tiering would trade a real ordering signal for
   a stability problem this backend does not have, and would widen exactly the ties item 3 is
   about.

Closed by ADR-0084: nothing reported a failed association, because `AddAndActivateConnection2`
returns before the radio has tried.


## 0084. A connect attempt reports its own outcome

`network:connect` returned as soon as NetworkManager accepted the request, which is before the radio
has tried anything. A wrong password was an `eprintln!` and a panel that showed nothing, which is
ADR-0083's one remaining item. `NetworkService.qml` answers it with `connectError`/`connectingSsid`
properties; this is the same answer over D-Bus.

1. **Two fields, not a new channel.** `NetworkState` gains `connecting_ssid` and `connect_error`,
   pushed on the snapshot the capability already sends. The pattern is `LockState`'s
   `authenticating` + `error` and `UpdatesState`'s `installing` + `install_error`, down to the
   naming and to `connect_error` holding prose rather than a machine code — "words fit to draw" is
   already this codebase's convention for a failure a config has to render.

   `connecting_ssid` names the network rather than being a bare flag because a list has to know
   which row is in flight. Neither field is derivable from NetworkManager — they are a memory of an
   attempt, not a reading of the stack — so `handle_signal` carries both across the full re-derive
   exactly as it already carries `scanning`.

2. **The verdict comes from `Connection.Active`'s `StateChanged(state, reason)`.** Both activation
   calls hand back an activation object; `ACTIVATED` clears the attempt, `DEACTIVATED` maps its
   reason through `connect_error_text`, and `NO_SECRETS` is the wrong password. Subscribing happens
   after the call returned, so the current `State` is read once to close the gap. ponytail: a
   failure landing inside that gap loses its reason and reports the generic line, since only the
   signal carries one; success does not, which is the far likelier race.

3. **`rusty_network_manager` 0.7.1's binding for that interface cannot work, so this one proxy is
   hand-written.** The crate declares the signal `#[zbus(signal, name = "state_changed")]`, and zbus
   takes an explicit `name` verbatim instead of PascalCasing it, so
   `ActiveProxy::receive_active_state_changed` subscribes to a member NetworkManager never emits.
   Found by measurement, not by reading: an activation that reached `ACTIVATED` in about a second
   produced no signal in twenty. Its sibling `Device` proxy spells the same attribute
   `name = "StateChanged"` and works, which makes it a typo upstream rather than a convention. The
   local proxy declares only `StateChanged` and `State`, and ADR-0013's "go through the crate" rule
   stands everywhere else.

4. **No "one attempt at a time" refusal.** `lock:authenticate` and `updates:install` both refuse a
   second while one runs, and the QML does the same. Here a verdict is simply dropped when its SSID
   is no longer the one in flight, which fixes the same overlap — an older failure landing on a
   newer attempt's spinner — without a rule that has to be explained to a config.

5. **A saved network connects without a password.** Reaching any of the above needed the connect
   path to be reachable at all, and it was not: `network:connect` only stashes an intent, the secret
   that releases it can come only from a focused `secure_submit` field, and the bar cannot host one
   — it is `keyboard_interactivity = "None"`, and a layer surface that takes focus on demand takes
   it the moment it maps. So every click on a saved network stashed an intent nothing would ever
   consume.

   A profile that exists already has its key, so the intent completes itself. This is what
   `NetworkService.qml` does for a `known` network too. An unknown SSID still waits for a password
   and still has nowhere to type one; a prompt means giving a surface keyboard focus, which is a
   surface-policy decision and not this ADR's.

6. **The panel reports in its header, not per row.** A spinner on the row would mean mapping
   `available_networks` into enriched items on every push, putting back the copy ADR-0082 removed
   from this panel. `connecting_ssid` names the network in the header line instead, and
   `connect_error` replaces it in `RED`, which is how `lock.lua` draws a failed attempt.

Verified on real hardware, both branches: reusing the saved profile left the profile count at three
and never dropped the link, and the activation reported `error=None` on success and
`Some("device disconnected")` for an SSID that does not exist. The 45-second ceiling is a backstop
for an activation object that stops answering, not the mechanism — NetworkManager reported both
outcomes in seconds.

## 0085. The Wi-Fi password prompt, and what it cost to give a popup the keyboard

ADR-0084 decision 5 left this open on purpose: an unsaved secured network stashed an intent nothing
would consume, because a prompt "means giving a surface keyboard focus, which is a surface-policy
decision and not this ADR's". It turned out the mechanism already existed and the policy was the
whole problem.

1. **The Supervisor decides when to ask, not the config.** `NetworkState` gains `password_ssid`,
   set by `resolve_connect_intent` for the one case that cannot proceed on the click alone: no saved
   profile and the network is secured. A config could not derive it — whether a profile exists lives
   in NetworkManager's settings (ADR-0037) — and the three branches are `NetworkPanel.qml`'s own.
   An SSID that is hidden or out of range is treated as secured, the way `showPasswordInput`'s
   `?? true` does, because nothing here can say otherwise. This also fixed a latent case the old
   `connect_if_saved` never handled: an *open* unsaved network stashed an intent forever.

2. **`network:cancel_connect` is the way out, and it is idempotent.** Escape inside a
   `secure_submit` field clears the entry and stays in the field, so without a cancel a prompt
   raised by a mis-click would hold the keyboard until something else took it. Being a no-op when
   nothing is pending is what lets `panel_host` spend it unconditionally on every close — closing
   the panel answers the prompt — without every panel close clearing `connect_error`.

3. **Keyboard focus is a scope, not a surface.** *Still true, but no longer load-bearing for this
   prompt: under ADR-0087 the field and the keyboard are on the same surface. The rule stays for
   every other popup-hosted field.* The field lives on `panel_host`, an `xdg_popup`;
   the compositor hands the keyboard to `bar`. niri gives a grabbing popup the keyboard only if its
   parent held it when the popup mapped, which is never true here — the prompt is raised by a click
   *inside* the already-open panel. So `wayland::input`'s `keyboard_focus_scope` is the focused
   surface plus every popup shown under it, and `sole_secure_submit_in_scope` asks "exactly one"
   across the whole scope. Before this the prompt was untypable until the panel was closed and
   reopened, which worked by accident: the second map found the parent focused.

4. **The bar claims the keyboard when a panel opens, not when the prompt appears.**
   *Superseded by ADR-0087, which retires the popup this worked around.* Measured twice: raising
   `keyboard_interactivity` on a *mapped* layer surface makes niri re-evaluate focus, which breaks
   `panel_host`'s grab, and the panel is dismissed before the focus event even arrives — the prompt
   appeared and vanished in the same frame. Bound to `panel_open` the change lands on the pass that
   creates the popup instead, since `bar` precedes `panel_host` in the surface list. The cost is
   that any open panel takes the keyboard; the alternatives looked like `grab = false` (which
   retires ADR-0051's click-outside-to-close) or drawing the prompt outside the popup. The third
   alternative, not seen at the time, was to stop being a popup.

5. **A field that becomes visible under a focus that already arrived needs its own arming.**
   Consequence of 4: there is no second `enter` when the prompt appears, so
   `arm_secure_focus_if_the_scope_now_declares_one` runs once a turn beside the existing teardown
   check. Only when nothing is armed, so it can never take a field from the press that chose one on
   a surface declaring several — the guess ADR-0050 decision 4 refuses to make.

Typed characters still never reach the Lua VM: `secure_submit` carries them from the Wayland thread
to the capability and nowhere else (ADR-0005/ADR-0027), so the prompt has no `on_change` and no
`on_submit`. It is the only such field on `panel_host` across all five panels, which the "exactly
one in scope" rule makes load-bearing rather than incidental.

## 0086. `lua-meta` types nothing unless a signal is `userdata`

A notification's `body` is a `NotificationSpan[]` (ADR-0033) and two config sites read it as a
string. The result was not a wrong label: `text.content` must be a string, so every re-resolve
failed and the shell froze on its last good scene for as long as that notification was in the feed.
The stub declared the field correctly and the language server said nothing, which is the part worth
recording.

1. **A `---@class` in a union accepts any table.** Measured against `lua-language-server` 3.19.1:
   `string|Signal` accepts a `NotificationSpan[]`, and still does with a required `---@field` on the
   class, and still does with `---@class Signal: userdata`. Only the built-in `userdata` refuses a
   table. So every node property spelled `X|Signal` — all 46 of them — accepted every payload type
   in the IDL. `Bound` is `---@alias Bound userdata`, and it is the honest spelling anyway:
   `components/icon_button.lua`'s `is_signal` already tests `type(value) == "userdata"`.

2. **`Signal<T>`, with its methods as `---@field`.** Written as `function Signal:map(fn)` with
   `---@param fn fun(value: T)`, the class's own `T` does not bind and the annotation silently does
   nothing — it reads correctly and checks nothing, which is worse than omitting it. As a
   `---@field` it binds, so `oblisk.network:map(function(n) ... end)` types `n` and a misspelled
   field is an `undefined-field`. `map` returns `Signal<any>` rather than the mapped type: a
   `---@field` cannot introduce a second type parameter, so one hop is typed and a chain past it is
   not. `computed`'s callback stays untyped for the same reason plus an overload per arity.

3. **The diagnostics ship below the level anything reads them at.** `param-type-mismatch`,
   `assign-type-mismatch` and friends carry **Hint** severity, and both `just types` and an editor's
   default check run at `Warning`, so the whole IDL type-checked nothing regardless of 1 and 2.
   `.luarc.json` promotes them, and `setup.rs`'s `luarc_json` writes the same promotion into every
   config `oblisk init` creates.

4. **`just types` had never run on the machine it was written on**, because `lua-language-server` is
   not on `PATH` there — Zed's Lua extension downloads its own copy — and the recipe skips silently
   when it is missing. It now falls back to that copy. It also printed nothing on failure: the
   report block read a `check.json` that needs `--check_format=json`, which was never passed, so a
   failure surfaced as `set -e` and a bare exit code. The human-readable output it was discarding is
   better than the JSON anyway, because it carries the offending source line.

What this does not buy: Lua is not Rust. `any` still flows out of any unannotated helper, a class
stays permissive in the table direction, and `list`'s `itemfn` cannot infer its item type from
`source`, so those five callbacks carry a hand-written `---@param`. The engine's own parsers remain
the real gate; this moves the common mistakes to edit time.

## 0087. The panel host is a layer surface, and a staged layer request needs its own commit

ADR-0085 decision 4 shipped a bar that took the whole keyboard for as long as any panel was open,
to reach one password field. That was the honest cost of the design it was written under, and the
design was the mistake: the constraint came entirely from `panel_host` being an `xdg_popup` with a
grab, and the reference config this shell mirrors never took that path. `Modules/Shell/MainScreen.qml`
is one screen-tall `PanelWindow` holding the bar and the panel host together with no `xdg_popup`
anywhere, which is exactly why its `WlrLayershell.keyboardFocus` can follow a per-panel
`needsKeyboardFocus` (`NetworkPanel.qml`'s is `showSsidInput || showPasswordInput || ...`).

1. **`panel_host` becomes a `panel`.** Screen-tall under the bar, `visible` still bound to
   `panel_open`, and its single root node holds a full-fill click-outside catcher with the panel
   card stacked over it. `hit::descend` walks children in reverse and stops at the first that
   contains the point, so the card shields itself from the catcher without a handler of its own.
   Click-outside-to-close is now ours rather than the compositor's `popup_done`, which retires the
   ambiguity `lib/ui_state.lua`'s `toggle_panel` was written around — `on_dismiss` carried no token
   saying which popup it dismissed, so switching panels directly sometimes took a second click.
   It is one click now, because the bar is left uncovered: `exclusive = false` reserves nothing but
   still respects what the bar reserved, so niri configures this surface at 1920x1161 starting under
   the bar rather than over it.

2. **`keyboard_interactivity` binds to `network.password_ssid`, and the bar goes back to `"None"`.**
   The whole point. There is no grab to break, so the claim can be as narrow as the fact that wants
   it: this surface holds the keyboard exactly while a `network:connect` is waiting on a password,
   and nothing else on the bar ever asks for it. `lua-meta/oblisk.lua`'s own field doc has said this
   is what the shell binds focus to since ADR-0084; it is true now.

3. **`constraint_adjustment` is the one thing paid for, and it is arithmetic.** A popup got
   `"FlipY"` and `"SlideX"` from the compositor; a layer surface gets neither. `"FlipY"` needs no
   equivalent, since this surface starts below the bar and extends down. `"SlideX"` is a `math.min`
   against `oblisk.screens[1].width` in the `computed` that places the card — the same single-head
   guess `config/theme.lua`'s `main_screen` already makes, except that this one follows the signal.

4. **A staged layer-shell request needs its own commit, and did not have one.** Found while
   measuring 2, and the reason the first attempt looked like a compositor refusal.
   `apply_resolved_state` stages double-buffered `wl_surface` state and leaves the commit to
   `paint_surface`'s `swap_buffers` — but `paint_surface` returns early when the display list is
   byte-identical to the last one, which is the whole point of that check and true of most surfaces
   on most passes. So `set_keyboard_interactivity` was sent and then sat pending: `panel_host`
   raised itself to `Exclusive` over an already-drawn card and niri never gave it the keyboard,
   while editing an unrelated border width delivered the focus change instantly. The bar never
   showed the bug because its clock redraws it once a second. `apply_spec_change` now commits when
   anything moved, guarded on `Mapped` (a bufferless commit on an unmapped surface is the protocol's
   re-map) and skipped for a PBA Candidate. This was always a bug for `margin`, `size` and
   `exclusive` too; nothing had bound them to a signal that moved without also changing the paint.

5. **A bar indicator is a toggle now.** `open_panel` became `toggle_panel`: clicking the indicator
   of the panel already showing closes it, clicking a different one replaces it, clicking any of
   them with the host closed opens it. Set-only was not a style choice before — under the grab a
   toggle would have closed the panel it was opening, since niri delivers the opening click to the
   bar as the popup's own parent, and it would have fought `on_dismiss`, which already wrote false
   on every click landing elsewhere. With no grab there is no second writer to fight. The pending
   password prompt is answered by `close_panel` rather than by its callers, so the click-outside
   catcher and a toggle-close cannot drift apart.

Measured end to end on niri: a non-visual flip to `"Exclusive"` on the mapped surface now takes the
keyboard and arms `network/connect` in the same turn, and the flip back releases it. The card lands
at `bar_height + panel_gap` from the top and clamps to the output's right edge, both read off a
pixel scan rather than believed.

## 0088. Hiding a `panel` destroys it, because the layer-shell re-map is not honoured

ADR-0038 decision 2 made `visible` on a `panel` a map or unmap of an object that lives for the
generation, on the reasoning that toggling a launcher should cost a commit rather than a Wayland
object. `zwlr_layer_surface_v1` supports exactly that: attach a null buffer to unmap, and "the
client can re-map the surface by performing a commit without any buffer attached, waiting for a
configure event and handling it as usual." niri does not bring such a surface back.

Measured on the wire with `WAYLAND_DEBUG=1`, and the trace is the whole argument, because every
request is the one the specification asks for:

```
-> wl_surface#19.attach(nil, 0, 0)                     unmap
-> wl_surface#19.commit()
-> zwlr_layer_surface_v1#20.set_anchor(9)              re-map: state is reset, so re-send it
-> zwlr_layer_surface_v1#20.set_size(355, 90)
-> zwlr_layer_surface_v1#20.set_keyboard_interactivity(0)
-> zwlr_layer_surface_v1#20.set_margin(50, 11, 0, 0)
-> wl_surface#19.commit()                              the bufferless commit
   zwlr_layer_surface_v1#20.configure(9255, 355, 90)   the compositor answers
-> zwlr_layer_surface_v1#20.ack_configure(9255)
-> wl_surface#19.attach(wl_buffer#57, 0, 0)            a fresh dmabuf
-> wl_surface#19.damage_buffer(0, 0, INT_MAX, INT_MAX) full damage
-> wl_surface#19.commit()
```

Nothing is on screen afterwards, and nothing later brings it back: further repaints attach further
buffers to the same surface and none of them appear. Ruled out along the way: a stale display-list
cache (`last_painted` is cleared on hide), a race (an 80ms delay before the swap changes nothing),
and a missing commit for the staged layer-shell state (ADR-0087 decision 4, fixed separately and
still needed).

1. **`visible = false` destroys the `zwlr_layer_surface_v1` and its `wl_surface`; `visible = true`
   builds new ones.** `TrackedRole::Panel::layer` becomes an `Option`, the role keeps the
   `wl_output` so the rebuild targets the same one, and the teardown order is `hide_window`'s:
   child popups, then the EGL surface and `wl_egl_window`, then the role object. This is what the
   `window` and `popup` roles have always done (ADR-0049 decision 1) and what the Qt shell this
   config mirrors does for a `PanelWindow`, so it is one rule for all three roles rather than a
   fourth behaviour.

2. **A panel declared `visible = false` at startup is still created.** It has never been mapped, so
   there is no compositor state to fail to restore, and keeping it means PBA staging still sees
   every declared surface (§ 15.2). `show_panel` tells the two cases apart by whether the role still
   holds a `LayerSurface`: if it does, the surface goes straight to `Mapped` and the next paint's
   first buffer maps it; if not, it is rebuilt and waits for its initial configure.

3. **The size guard runs again on every show.** `width`/`height` are `Signal`-bindable, so a spec
   that has since resolved to a `Fill` on a singly anchored axis would be a protocol error that
   kills the connection. `create_panel` already refuses that; `show_panel` refuses it the same way
   and leaves the surface hidden.

What this cost: an EGL surface and a `wl_egl_window` are rebuilt per toggle rather than reused, and
the first frame after a show waits for a configure round trip instead of going out immediately.
Both are per-toggle, both are what the other two roles already pay, and neither is measurable
against a surface that does not appear at all.

How long it had been broken: since panels could be hidden. The first notification of a session drew
and every one after it was invisible, because `notification_area` is unmapped between notifications
— nobody had noticed, because a shell is usually restarted more often than it is watched. ADR-0087
made it constant rather than intermittent: opening and closing a bar panel is the most frequent
hide/show in the shell, so the power menu opened once and never again.

## 0089. A `text` can wrap, and an unwrapped one now measures the line it draws

`text` shaped through cosmic-text, which wraps, and painted through femtovg, which does not. The
measure callback handed the shaper the box width, counted the layout runs that came back, and
returned `line_count * line_height` as the node's height. Paint then made one `fill_text` call with
the whole string. So a fixed-width `text` reserved three lines of height and drew one clipped run
into the top of it, and the two disagreed silently — the clip made it look like elision.

Everything a notification card wants is downstream of fixing that: a body over two lines, a summary
that expands, a group whose rows are readable. It is also the reason every panel in this shell is
one elided line per field.

1. **The shaper returns the lines, not just their count.** `ShapeResult` gains
   `lines: Arc<[String]>`, filled from the same `layout_runs()` walk that already produced the
   height. Behind an `Arc` because `ShapingHandle::shape`'s memo hands a clone back on every hit and
   a hit is the common case, so a clone has to be a refcount bump rather than a `Vec` copy.

   The trap this walks into, worth recording because the first implementation fell in it:
   cosmic-text's `LayoutRun::text` is *the original text line* — the whole source paragraph, handed
   back again for every visual line the wrap broke it into. Collecting it directly yields the entire
   string N times over. Only the glyphs delimit a run, via the `start`/`end` cluster indices, read
   as min/max rather than first/last because a bidi run's glyphs are in visual order.

2. **`wrap` is opt-in, so `"None"` measures one line.** This is the part that is not purely
   additive. Measurement already wrapped unconditionally, so making wrapping the default would have
   started *drawing* into height every fixed-width `text` was already reserving, changing the whole
   shell at once. `wrap = "None"` now passes the shaper no width at all, so it measures the single
   line it will paint, and the box matches the paint in both modes. A config that never says `wrap`
   sees no change to what is drawn, and shorter boxes where it was over-reserving.

3. **`max_lines = 0` means uncapped, and so does absent.** Zero is refused nowhere and clamped
   nowhere: the property exists to be driven by a signal, an expander is
   `max_lines = expanded:map(function(e) return e and 0 or 2 end)`, and a `Bound` has no way to
   spell "absent". A negative is still an error — there is no reading of it, and clamping would
   swallow a sign slip in a config's own arithmetic.

4. **`elide` under `wrap` applies to the last line kept, over the text that did not fit.** Keeping
   the lines allowed and ellipsizing the last one *as it stands* would read as a sentence that
   happens to stop; the last kept line is rebuilt from everything below the cap so it reads as
   truncated. That remainder is the dropped lines joined back with single spaces rather than sliced
   out of the source, because cosmic-text hands back a line's text and not its byte range into the
   original. The difference is a run of collapsed whitespace, inside text that is already being cut
   off.

5. **Line breaking stays in `Scene::finish`, next to elision.** Paint is a pure display-list build
   with no shaping worker in reach, and the box width is not known until the node is sized, so
   `finish` is the only place both are available. `PaintStyle::Text::content` therefore carries `\n`
   by the time it reaches the display list, and `TextPainter::draw_text` walks `lines()` making one
   `fill_text` per line. femtovg draws a `\n` as a glyph and has no line breaker, so splitting there
   is not a convenience.

What this does not do: no per-line alignment (each line is aligned by the node's own `text_align`),
no hyphenation, and no `wrap` at a character boundary as its own mode — cosmic-text's word wrap
already falls back to a glyph boundary for a word wider than the box, which is the case that
motivates one.

Left alone, and noted here because line advance now depends on it: `TextPainter` hands femtovg the
*logical* font size against a canvas whose dpi is 1.0, while every box around it is snapped to
physical pixels. On a 2x output that draws every glyph in this shell at half size, and has since
text existed. Lines advance by the same unscaled step, so they are spaced correctly around whatever
size the glyphs come out — wrong together rather than wrong apart. The fix is to scale the font
size, and it needs a HiDPI output to verify against.

## 0090. A notification's actions are kept, and a config can invoke one

`Notify`'s `actions` array — the flat `[key1, label1, key2, label2, ...]` list every notification
button in every shell comes from — was read by one predicate, `actions_have_reply`, and dropped on
the floor. `GetCapabilities` advertised `actions` and `action-icons` and neither was true, and
`Notification`'s own doc comment said the array "is never stored, only the `has_reply` bool it
collapses into". A config could draw a notification and never offer Archive, Snooze, Mark as read,
or Reply-by-button.

1. **`actions` is a typed list, and the two keys that are not buttons are not in it.** `"default"`
   is the whole notification's activation — clicking the card — and becomes `has_default_action`;
   `"inline-reply"` becomes the `has_reply` that already existed. Both are real action keys on the
   wire, and both draw as nonsense if a config repeats them in a button row, so the split happens
   once here rather than in every config that renders a card.

2. **An icon action carries a theme name, not a path.** Under `hints["action-icons"]` the base spec
   says the key doubles as an icon name, so it is carried as one — but `icon` accepts an absolute
   path as readily as a theme name (ADR-0054 decision 2), so a key holding a path separator is
   refused as an icon rather than passed through. Without that, any application on the session bus
   could name a file on this machine and have the shell draw its contents.

3. **`invoke_action` checks the key against what the notification declared.** The same rule
   `reply` already applies through `has_reply`, for the same reason: a key the sender never offered
   means nothing to it, and forwarding one only produces a signal the application has to field and
   ignore. An undeclared key is a logged no-op.

4. **Invoking removes the notification unless the sender said otherwise.** That is the base spec's
   default and matches what `reply` does. `hints["resident"]` is the spec's own exception and is
   honoured, because a media notification whose prev/next buttons closed the card on first press
   would be useless. A removal here also emits `NotificationClosed(id, reason=3)`: an action-invoked
   close is a close, and a sender tracking its own ids has to hear about it.

5. **Both caps are on the same footing as §1.1's text caps.** At most 8 actions, and a label
   truncated to 64 bytes on a character boundary. The array arrives from an unprivileged sender and
   a config draws every entry of it; eight is past anything real, and a label is a button rather
   than a paragraph.

Dropped from scope: `x-kde-reply-placeholder-text`. The placeholder is only worth carrying once
something can type into the field it labels, and nothing can — `zwp_text_input_v3` is unwired, so
the unmasked half of `textfield` has never received a keystroke. It belongs with that work.

Verified against a live `Notify`: a notification declaring `default` and `archive` reaches Lua as
one action plus `has_default_action = true`; invoking `archive` makes `notify-send -A` print
`archive` and exit; invoking a key the sender never offered prints nothing and logs the refusal;
and the same notification sent with `resident` stays mapped across repeated invocations where the
plain one is gone after the first.

## 0091. The attached picture and the sending application's icon are two fields

`Notify` offers four ways to say "here is a picture", and ADR-0033 collapsed all four into one
`icon_path` on a single precedence chain: `image-data` > `image-path` > `app_icon` > `icon_data`.
Three of those four are the same thing under the spellings the spec accumulated across 1.0, 1.1 and
1.2. The fourth is not: `app_icon` is the *sending application's* icon, and it lost every race
against a picture the sender attached.

Worse, it lost the races it won. The whole chain terminated in `validate_trusted_path`, which
requires an absolute path to an existing file under a small allowlist. `app_icon`'s documented and
overwhelmingly common form is a bare theme name — `"firefox"`, `"org.telegram.desktop"` — which is
not an absolute path, so it resolved to nothing. Every notification in the shell that did not ship
raw pixel data drew the same generic fallback, and had since notifications existed.

1. **`image_path` is the picture, `app_icon` is the sender.** `image-data`/`image_data` >
   `image-path`/`image_path` > `icon_data` feed the first; the positional argument feeds the second.
   A card can now show both, which is what the Qt shell this config mirrors does: the app's mark in
   the header, the attachment beside the summary.

2. **`app_icon` may be a theme name, and is carried as one.** `icon { name = ... }` resolves theme
   names in the renderer (ADR-0054 decision 2), so there is nothing to validate and nothing to
   spool — the value travels as text and the renderer's own icon lookup decides. A path still goes
   through the trusted-root check every other client-supplied path does.

3. **The two forms are told apart by a path separator, not by `is_absolute`.** A relative path is
   neither: `"../../etc/passwd"` is not absolute, so an `is_absolute` split would hand it to the
   renderer as a "theme name" and let the icon lookup take it from there. Anything containing a `/`
   must be an absolute, trusted path or it is refused.

4. **`icon_path` is renamed rather than kept as an alias.** It has always held the picture, and
   keeping a name that says "icon" for the field that is not the icon is the mistake this ADR is
   fixing, not a compatibility surface worth preserving. Nothing outside this repo consumes the
   payload yet (there is no released version — see `Cargo.toml`'s versioning note), so the rename
   costs one line in `notification_history.lua`.

Not done here: `hints["desktop-entry"]`. It is the better grouping key than `app_name` and a decent
third icon source, and it is additive whenever a config wants it. Adding a field for a consumer that
does not exist yet is how §2.7 got four picture sources in the first place.

## 0092. An ordinary `textfield` reads the keyboard too, because text-input-v3 types nothing

ADR-0027 decision 3 said an ordinary `textfield` fires `on_submit` from `zwp_text_input_v3`'s
protocol-native submit action, "IME-correct: it works with CJK composition, which raw keystroke
detection does not." Its own amendment then found the flaw and applied it only to the masked half:
text-input-v3 needs a compositor-side input method bound, and with none running `commit_string`
never arrives, so no byte ever reaches the client.

That is not a corner case. There is no input method on this session — no fcitx5, no ibus, no
`XMODIFIERS` — which is the default state of a fresh Wayland desktop. A `textfield` built on
text-input-v3 would take a click, draw a caret, and swallow every keystroke, and the config author
would have no way to tell that from a bug in their own code.

1. **Both field kinds read `wl_keyboard` and xkb.** The plain half now shares `key_action` with the
   masked half, which already reads the keyboard for reasons of its own (a password must not route
   through an input method — swaylock and hyprlock read xkb directly for the same reason). This
   supersedes ADR-0027 decision 3.

2. **What that costs is composition, and it is a real cost.** No CJK, no dead keys, no compose
   sequences: `KeyEvent::utf8` is one character per key press. Latin text, including every accented
   character a keyboard layout produces directly, works. The upgrade path is to bind text-input-v3
   *alongside* this and let a `commit_string` win when an input method is actually present; that is
   worth building when someone needs it, and it is strictly additive.

3. **A press focuses a plain field; there is no arm-on-`enter` fallback.** The masked half has one,
   because `sole_secure_submit_in_scope` can pick the single password prompt on a surface. A card
   with one reply box per notification has no sole field, so that rule cannot serve this one, and
   clicking into a text field is what every toolkit asks for anyway.

4. **A plain focus is addressed by its box.** `ResolvedNode` carries no `NodeId` — `to_resolved`
   drops it — so the field's absolute rect stands in, exactly as `input::ArmedClick` already does
   for a press. A re-resolve that moves the field detaches the caret from it, which is what a real
   identity would give for a field moved out from under the user. Upgrade: put the `NodeId` on
   `ResolvedNode` and key both halves on it.

   The rect is not sufficient on its own, and a test caught why: a plain focus and a *masked* node
   can coexist on one surface, so paint's plain arm also requires the node to declare no
   `secure_submit`. Without that, a password field whose box happened to match would have drawn
   another field's plaintext.

5. **Both callbacks carry the whole text, not the delta.** A config binding a `state` signal to a
   reply box wants the value; reassembling a string from edits is work every caller would repeat.
   `on_submit` leaves the field focused and empty, so a reply box takes the next message without
   another click, and Escape clears and stays — the same answer the masked half gives. Dropping
   focus on Escape is the more conventional behaviour and is not available: a config cannot observe
   focus, so a field that silently stopped taking keys could not say so on the glass.

6. **A field that can report nothing is never focused.** Masked with no destination has nowhere to
   send a submit; plain with neither callback has nobody to tell. Focusing either takes the keyboard
   away from a field that could have used it, in order to buffer keystrokes nothing will read.

7. **A press that focuses a `textfield` arms no click.** `textfield` is a leaf -- § 5.2 gives it no
   `children` -- so any `button` on the hit path is an ancestor of it, and clicking into a text
   field inside a clickable row is not a click on the row. This is the notification card exactly:
   its whole surface activates the sender's default action (ADR-0090) and its reply box sits inside
   that, so without this rule every attempt to reply would fire the notification's default action
   and take the card away. Found by trying it: the probe card dismissed itself on the click meant
   to focus its field.

A plain field's text lives in `App::focused_text_field`, outside the retained tree, for the same
reason a masked field's bytes live in `secure_buffer`: typing marks no property dirty, so
`field_input_changed` (renamed from `secure_input_changed`, since it is now both kinds) drives a
repaint without a re-resolve.

## 0093. A notification carries when it arrived, because nothing else can work it out

The Qt shell this config mirrors draws a relative age on every card — "5m ago" — off a `timestampText`
its own wrapper records. § 2.7 has no such field, and until now the answer was "a config can note
the clock the first time it sees an id."

It cannot. There is exactly one place in the Lua API that runs when the feed changes and does not
need a click first, and that is a `computed`/`map` callback, which ADR-0021 requires to be
side-effect-free and caps at 5ms across the whole graph. Recording arrival times there is writing
to the world from inside a pure function, and a reload re-runs it against a feed that already has
notifications in it, which would date all of them to the reload. So this is not a convenience field
standing in for a workaround; there is no workaround.

1. **Unix epoch seconds, matching `oblisk.system`'s `time` exactly.** Age is `system.time -
   timestamp` and nothing has to reconcile two clocks or two units. § 2.11 already settled that
   argument once — it calls its field "system time epoch" with no unit, and picked seconds because
   `os.date` wants seconds and a millisecond reading is silently wrong by 1000×. A second field on
   the same clock in a different unit would re-open it. `capabilities::system::controller::
   epoch_seconds` is reused rather than reimplemented, which is what its own doc comment asks for.

2. **Wall clock, not monotonic.** The consumer is a human-readable age rendered against
   `system.time`, which is wall clock; a monotonic reading cannot be subtracted from it. The cost is
   that stepping the system clock re-dates the feed, which is the same cost every "5 minutes ago" in
   every application pays, and the alternative — carrying both — is a second field for a case
   nobody has.

3. **A replacement gets a fresh timestamp.** `replaces_id` reuses an id to put *new content* at it,
   and the timestamp describes the content. "3 new messages" arriving now is not four minutes old
   because "1 new message" was. This falls out of building a whole `Notification` per `Notify`
   rather than patching the queued one, so it is a decision only in that it could have been
   undone deliberately.

4. **Set in `Notify`, not at push.** The two are microseconds apart and the difference is not
   observable, but `Notify` is where the content is assembled and the field belongs with the content
   it dates.

Not done here: an expiry deadline alongside it. A card could draw a countdown ring from
`resolve_expiry`'s answer, and the Qt shell does, but that number is the supervisor's own timer and
publishing it invites a config to believe it — see ADR-0094, which makes the timer pausable and so
makes any published deadline a lie the moment a pointer enters the card.

## 0094. Expiry is held off by a deadline, not paused by a flag

A notification with an inline reply arrives with a 5-second timeout, and typing a reply takes
longer than that. The card goes away mid-sentence and the reply goes nowhere. The Qt shell this
config mirrors solves it with `pauseTimers`/`resumeTimers` on the card's hover.

That shape does not survive being handed to a config. A paused/resumed pair needs both edges to
arrive, and the config is a process that gets reloaded on every file save — a reload between the
pause and the resume leaves the Supervisor paused with nobody left who knows it, and nothing can
tell that state from a legitimately long one. The feed stops expiring for the rest of the session.

1. **`hold_expiry(seconds)`, one call, self-releasing.** The config asserts "somebody is
   interacting, don't expire anything for the next N seconds" and the assertion lapses on its own.
   `0` releases early, so the both-edges shape is still available to a caller that has both edges;
   it just is not required for correctness. The natural use is the opposite: re-place a short hold
   from an event that is already repeating, which a reply field's `on_change` is — it fires per
   keystroke (ADR-0092 decision 5), so typing holds expiry off for exactly as long as typing lasts
   and no config state tracks it.

2. **Clamped to five minutes.** The cap is what makes decision 1's guarantee real rather than
   rhetorical: a config asking for a week gets five minutes. Five minutes of continuous
   interaction with one notification is past anything real.

3. **Global, not per-id.** A hold means "the user is looking at the shell", which is not a property
   of one notification. Per-id would need the config to enumerate what is on screen and re-place a
   hold per entry, and to do it again after every reload, which is the bookkeeping decision 1
   exists to delete.

4. **The clock stops; it does not restart, and it does not fire on release.** A notification held
   at 2s of 5 has 3s left when the hold lapses. Restarting would make a passing pointer reset every
   card in the stack. Expiring immediately is worse: the card would disappear at the instant the
   pointer left it, which reads as the pointer having dismissed it.

5. **The countdown stays one task per notification.** `Notify` already spawned a task that sleeps
   and then re-checks; giving that sleep a `watch` receiver is the whole change. The alternative —
   a deadline stored on the queue entry, decremented on hold and re-armed on release — adds shared
   state and a second place that has to agree with `find_expiring_entry` about what is still
   pending, to buy nothing.

6. **Not readable from Lua.** `NotificationsState` gains no field. The config is the only writer
   and already knows what it asked for; publishing it would invite a second reader to decide
   things from a value that is stale the moment it is pushed. A transition is logged instead,
   because a feed that has stopped expiring and a broken timer look identical from outside.

7. **`tokio`'s clock, not `std`'s.** `tokio::time::pause` moves only the former, and the countdown
   is the thing under test — the five tests here assert exact durations (5s served, 2 + 60 + 3,
   an extension winning over the hold it replaced) and run in about ten milliseconds. Written
   against `std::time::Instant` first, where they passed by sleeping through a real minute.

Only half of what this unblocks is reachable today. A reply field can hold expiry off from
`on_change`, which is the case where something is actually lost. Holding it off merely because the
pointer is resting on a card needs a hover *callback*, and § 5.2 has only `hover(name)`, a
read-only signal that a `computed` cannot act on — the same wall ADR-0093 hit. That is its own
edit.

## 0095. `on_hover`, because a config could see a hover but not act on one

ADR-0094 gave the Supervisor a way to hold a notification's expiry off, and then could only wire
half of it: a reply field can hold expiry from `on_change`, but holding it merely because the
pointer is resting on a card had nothing to fire from. `on_click` was the only pointer callback in
§ 5.2. `hover(name)` is a read-only signal, and the one place a config runs code when a signal
moves is a `computed`, which ADR-0021 requires to be side-effect-free — so a config could *draw*
differently on hover and could not *do* anything: no capability call, no `state` write.

1. **It fires on the crossing, not on the motion.** `wl_pointer` reports motion at device rate, so
   a per-event callback would run a config handler a few hundred times for one pass across a
   button. `sync_hover` already computes the exact edge — `set_changed` returns whether the value
   moved (ADR-0062 decision 4) — so the callback rides on that answer and costs one branch.

2. **It requires a `hover` slot on the same node, and is refused without one.** A callback has no
   memory and a `ResolvedNode` has no identity to hang one off (`to_resolved` drops the `NodeId`),
   so something has to remember whether this node was hovered last pass. The `hover` signal already
   does, keyed by a name the config chose, which survives a reload and keeps working when a `list`
   churns its cards underneath it. The alternatives were both worse: keying the memory by absolute
   rect lets a new card inherit a dismissed card's state, and keying it by position in the walk
   breaks the moment a notification leaves the middle of a stack.

   Refused rather than left inert, in `resolve_properties` where the whole property map is in
   reach. A silently unreachable handler is exactly the failure `deserialize_lua_table`'s
   unknown-key rejection was added to end, and the message names the fix.

3. **The argument is `hovered`, and nothing else.** `on_click` passes its rect because there is no
   other way to get it; here there is — `hover_rect(name)`, which decision 2 already obliges the
   config to have a slot for.

4. **A raised error is logged and swallowed**, on `fire_on_click`'s terms: a broken handler is a
   config bug and must not take down a shell that is otherwise painting fine. ADR-0046's rescue
   path is for a failed evaluation, not a misbehaving callback.

The pairing rule needed one seam in the type probe: `every_type_the_stubs_declare_is_accepted_by_
the_engine` builds each property in isolation, so `on_hover` alone would have been testing decision
2 rather than the type the stub declares. A per-field `companions` table supplies the slot, next to
the per-kind `required` one that was already there.

Verified live end to end, which also closed ADR-0094's open half: with `on_hover` holding expiry on
a notification card, a 5-second notification that mapped under a resting pointer was still up at
12 seconds, the log showed `expiry held for 60s`; moving the pointer off logged `expiry hold
released` and the card went. Declaring `on_hover` without a slot fails the reload with the message
from decision 2.

## 0096. A theme name in `image-path` is the application's icon, not a picture

ADR-0091 split the attached picture from the sending application's icon and routed
`image-data` > `image-path` > `icon_data` into the first. `image-path` does not only carry
pictures. §1.2 defines it as "an URI (file:// is the only URI schema supported right now) **or a
name in a freedesktop.org-compliant icon theme**", and the picture chain ends in
`validate_trusted_path`, which requires an absolute path to an existing file.

So a theme name there was dropped on the floor. That is ADR-0091's own bug one field over, and it
reaches far more notifications than the original did: `notify-send -i firefox` leaves the
positional `app_icon` empty and puts `firefox` in this hint, which makes it the most common way
anything on a desktop names a notification's icon. Found by building the card that draws it --
every `notify-send -i` in testing came up with the generic fallback while a hand-written `Notify`
carrying the same name in the positional argument drew correctly.

1. **A bare name in the hint feeds `app_icon`, not `image_path`.** It is not an attachment. A
   sender with a real picture sends `image-data` or an absolute path; a sender with a theme name is
   saying what application this is, which is what `app_icon` means and where a card draws it -- the
   small mark in the header rather than the large picture beside the summary.

2. **`image_path`'s contract is unchanged**: still always an absolute path to a file that exists.
   The alternative -- letting it hold either form, since `icon { name = ... }` accepts both -- would
   put `notify-send -i firefox`'s icon in the picture slot, drawn at attachment size beside the
   summary, which is not what the sender meant and would make the two fields mean the same thing
   again.

3. **The positional argument still wins.** A sender that sets both is making the specific statement
   with `app_icon` and a fallback one with the hint.

4. **Told apart by a path separator**, ADR-0091 decision 3's rule verbatim, and for its reason:
   `"../../etc/passwd"` is not absolute either, and an `is_absolute` split would pass it off as a
   theme name for the renderer's icon lookup to open. Anything holding a `/` stays in the picture
   chain, where the trusted-root check refuses it.

`split_image_path_hint` is a pure function beside `resolve_app_icon` rather than a match arm inside
`Notify`, because `Notify` is a D-Bus method and nothing can call it in a test.

Verified live, and it is the first time either half of ADR-0091 has been seen on the glass: a
notification carrying `app_icon = "firefox"` and `image-path = <an absolute svg>` draws the Firefox
mark in the card header and the attached picture beside the summary, and `notify-send -i telegram`
now draws Telegram's icon where it drew a generic fallback.

## 0097. The notification card, and the four things the config had to decide itself

`components/notification_card.lua` is `Modules/Notification/NotificationCard.qml` in this engine's
vocabulary: an application's notifications as one card, with a picture, an app mark, a wrapping
summary and body that expand, an age, action buttons, an inline reply, and per-message and
per-group dismissal. It is drawn in two places -- the popup stack and the history panel -- which is
why it is a component and not two files that drift.

Almost all of it is ADRs 0089-0096 arriving at once, and there is nothing to decide about using
them. Four things were the config's own call.

1. **Grouped on `app_name`, which is the only key § 2.7 carries.** `hints["desktop-entry"]` is the
   better one -- two applications can share a name and one can change its own -- and ADR-0091 left
   it unbuilt for want of a consumer. This is that consumer, and it still is not worth adding: the
   grouping is not visibly wrong yet, and adding a field before it is is how § 2.7 got four picture
   sources. Group order is by each app's newest notification, since the feed arrives newest-first
   and an app that just spoke should not sit below one that spoke an hour ago.

2. **One hover region for the whole stack, not one per card.** The expiry hold (ADR-0094) is placed
   on enter and released on leave, and sibling cards are written in tree order within a single
   `sync_hover` pass -- so a pointer moving from the second card to the first would fire the
   first's *enter* before the second's *leave*, and the leave would release the hold the enter had
   just placed. A region spanning every card has no interior crossings to get this wrong. Cards
   still light up individually; that is a separate slot doing a job with no ordering hazard in it.

3. **The reply field is opened by a button, not by clicking the field.** The surface must hold the
   keyboard before a field in it can receive one, and it cannot hold it unconditionally: niri gives
   an `on_demand` or `exclusive` layer surface focus the moment it *maps*, and this surface maps
   every time anything notifies you -- a constant would take the keyboard away from whatever you
   were typing in, on every notification. So `keyboard_interactivity` follows a signal, and that
   signal needs an event. The field's own press cannot be it: a press that focuses a `textfield`
   deliberately arms no click (ADR-0092 decision 7), which is the rule that stops a reply from
   firing the notification's default action. A Reply button is the ask and the surface follows it.
   The cost is one extra click, and it is the honest one.

4. **Clicking a message activates the sender's default action where there is one, and dismisses
   where there is not.** Both are what the freedesktop spec means by activating a notification, and
   `invoke_action` removes it afterwards on its own unless the sender set `resident` (ADR-0090), so
   the two paths agree about what happens next. The history panel used to only dismiss, with a
   comment saying a row that quietly does something other than what it looks like is worse than one
   that only dismisses -- that was right while the card drew no buttons, and the buttons are what
   make the card's own click legible now.

Two smaller ones. The popup stands down while the history panel is showing, because both anchor
top-right and the overlap draws the same notification twice with the copy you opened the panel to
read underneath. And expansion state lives in `lib/ui_state` as two tables rather than a signal per
group, because a group's key is an application name and so is not known until it arrives:
`state(name, initial)` is a name-keyed registry, and minting one per app at resolve time would grow
it for the life of the session.

What it does without, both waiting on Rust. The body's spans carry bold/italic/underline and an
`href` and `text` has no weight, style or link, so `util.notification_body` still flattens them to
one run. And there is no animation, so a group expands and a card leaves in one frame -- the
roadmap's own first item, and the one thing that will keep this reading as static beside the Qt
card it mirrors.

Verified live, end to end: three notifications from one sender collapse to `Chat (3)` and expand to
three; a body longer than three lines clips with the ellipsis on the last line and expands to its
full six; `notify-send -i` icons and an attached picture both draw; a pointer resting on the stack
logs `expiry held` and leaving logs `expiry hold released`; and Reply, click, type, Enter emits
`ActionInvoked(10, "inline-reply::on my way")` and clears the card and the keyboard behind it.

## 0098. A popup is retired, not hidden, and the config is what remembers

ADR-0097 had the popup stand down while the history panel was showing, so the two would not draw
the same notification twice on top of each other. That was the wrong shape and the bug was
immediate: closing the panel brought the popup back. Something you had already read, in a panel you
opened on purpose, returned to the corner of the screen as though it were new.

The reason there was no better answer available is that the Supervisor has two states and needs
three. A notification is live or it is gone -- `dismiss` and expiry both remove it from the feed
entirely, which is also its removal from the history. There is no "stop popping this up, keep it in
the list", and there should not be: which of the live notifications this shell has already put in
front of you is a fact about the shell's own presentation, not about the notification.

1. **The config keeps the seen-set.** `lib/ui_state`'s `popup_seen`, beside the expansion tables
   for the same reason they are there -- a view fact, shared by the two places the card is drawn.
   Nothing crosses the socket for this and nothing needs to.

2. **Keyed on id *and* timestamp, not id.** `replaces_id` reuses an id deliberately to put new
   content at it, so an id-keyed note would suppress the replacement as though it were the thing it
   replaced -- a chat app editing "1 new message" into "3 new messages" would go silent.
   `timestamp` moves on every `Notify` and stays put otherwise (ADR-0093), which is exactly the
   distinction wanted, and is the second thing that field has turned out to be load-bearing for.

3. **Marked on the panel's open *and* its close.** Opening it is the obvious edge; closing it is
   the one that matters, because anything that arrived while the panel was up was on screen the
   whole time and is owed no second showing.

4. **Replaced wholesale rather than merged**, which is what prunes it: the set becomes exactly the
   feed as it stands, so an entry that has since expired or been dismissed is forgotten, and the
   table cannot outgrow the feed's own cap of twenty (§ 2.7). A merge would accumulate keys for
   notifications that no longer exist, for the life of the session.

ADR-0097's stand-down survives as one clause inside the same filter, because it is still true that
both surfaces anchor top-right and the overlap is worth avoiding while the panel is up. It is no
longer what stops the popup returning.

Not changed here: the card's own close button still calls `dismiss`, which removes the notification
from the history too. Most desktops keep a dismissed popup in the list, and the machinery to do
that now exists -- it is one call site away. It is left alone because "the X means get rid of this"
is a defensible reading and the alternative makes the feed grow until something clears it, which is
a behaviour change worth asking about rather than assuming.

Verified live: with one notification popped up, opening the history panel destroys
`notification_area` and closing it does not bring it back; the notification is still in the panel;
and a notification arriving afterwards pops up normally with the retired one staying out of the
stack.

## 0099. `ResolvedNode` carries its `NodeId`, and a plain field's focus is keyed on it

`layout::paint::FieldFocus::Plain` named the focused `textfield` by its absolute rect, and
`input::focused_field` and `paint::FieldFocus` both carried a `ponytail:` saying why -- `to_resolved`
dropped the `NodeId`, so a box was the only handle either had -- and both named this as the fix.

The bug it produces is narrower than "focus is lost" and worse than it sounds. `prune_text_field_focus`
never checked the rect: it drops focus when the surface dies or leaves the keyboard scope, and
neither happens here. So a re-resolve that moved the field left the focus, the buffer and the
callbacks entirely intact, and only *paint* lost track -- the caret and the typed text disappeared
and the field showed its placeholder again, while every keystroke kept landing in a buffer that
`on_submit` would still have sent. In the notification card that is one notification arriving above
the one being replied to. Found while testing the card, where the screenshots taken to check it were
themselves posting the notification that triggered it.

The other direction was already covered by a test and is the reason the arm also checks
`target.is_none()`: a field that came to occupy the vacated box would have drawn text it never
received.

1. **`ResolvedNode` gains `pub id: NodeId`.** It is a `Copy` u64 and `to_resolved` already clones
   far more than that per node, so the cost is nothing. `reconcile_node` keeps the retained node's
   id and allocates only when there was nothing to match, which is what makes it a real identity:
   it survives the node moving, resizing, and gaining siblings ahead of it -- the last being
   exactly the notification case, since the `list` matches the card by its `key` and the subtree
   below it reconciles by position within a card that did not change shape.

2. **Not the § 5.1 `id` property.** That one is a reconciliation *hint* a config writes and is
   documented as unique among siblings only; this is the answer the engine reached, unique across
   the scene, and no config has to set anything for focus to work.

3. **`target.is_none()` stays, alongside the id check.** An id says which node this is. It does not
   say the node is still the kind of field the focus was taken on, and a `textfield` that gains a
   `secure_submit` between passes is the same node with a new job. The check is free and it keeps
   "a masked field never draws plaintext" local to the arm that would break it.

4. **`ArmedClick` keeps its rect.** The same ponytail named it as the other user of the stand-in,
   and it is not the same situation: a press and its release are one gesture, the tree rarely moves
   between them, and the failure is a lost click rather than a sentence typed into a field that
   stopped showing it. Left alone rather than changed alongside, because it is a well-covered path
   and this ADR has no evidence against it.

`NodeId::test` is `#[cfg(test)]`: production ids come from `Scene::alloc_id` and nothing else, which
is what makes them unique, but a test that builds a `ResolvedNode` without a `Scene` still has to
say which of its nodes are the same node.

Verified live, reproducing the original: a reply typed into a notification card, then a second
notification from another sender arriving above it. The card moves down and the field still reads
`on my way|`, where before it reverted to `Reply`; Enter from the moved position emits
`ActionInvoked(1, "inline-reply::on my way")`.

## 0100. Expiry retires a notification from the popup; it no longer removes it

The history panel showed what was still popped up, not what had happened. A notification that
timed out unread was gone from `notifications.feed` five seconds after it arrived, so "what was
that?" -- the question a history exists to answer -- had no answer. The reference config keeps a
timed-out notification in its list until the user clears it, and so does every desktop that has a
notification list at all; what the sender's timeout ends is the popup, not the record.

A config could not paper over this. Nothing fires a callback on a feed change and a `computed` is
pure (ADR-0021), so once the Supervisor dropped the entry there was nowhere to keep a copy. It had
to change on the Rust side, and the change is small: `expire` used to remove the entry from the
queue and now flips a flag on it.

1. **`expired: bool` on the entry, rather than a second list.** One list and a flag is what
   `ui.popup_seen` (ADR-0098) already reads like from the config's side, and it keeps `dismiss`,
   `invoke_action`, `reply` and the FIFO cap addressing one queue by id. A separate `history`
   array would have meant every id-taking command deciding which list to search first.

2. **The sender still hears `NotificationClosed(id, reason=1)` at the moment of expiry.** From its
   side the notification has closed: an `ActionInvoked` from a history card may or may not reach a
   process that still remembers the id, and a `replaces_id` at that id will be treated as new
   content. That is the base spec's contract and the reference config keeps it too, calling
   `expire()` on the notification while keeping its own wrapper. Nothing about the wire changes.

3. **`hints["transient"]` is honoured, and it is the only reason expiry still removes anything.**
   The spec's meaning of transient is "show this and do not keep it", which was the old behaviour
   for everything. It is carried as `transient` so a history can also leave a live one to the
   popup; the reference config does the same, never storing a transient wrapper.

4. **A repeated or stale expiry is a no-op, checked in the pure half.** `expire_entry` returns
   `None` for an entry that already expired, so the close cannot be announced twice, and for an
   incarnation that has moved on, as `find_expiring_entry` always did.

5. **The queue's caps are unchanged.** One hundred deep with the feed a view of twenty (ADR-0033).
   The feed fills faster now that entries stay, and the oldest falls off the back with
   `NotificationClosed(id, reason=4)` as before; the reference config caps at a hundred stored
   too. Raising the view is a one-constant change if twenty proves short for a history and is
   not made here on no evidence.

6. **A replacement resets the flag.** A `replaces_id` update builds a fresh `Notification` with
   `expired = false` and its own timer, so an updated history entry pops up again -- which is what
   "3 new messages" landing on a retired "1 new message" should do.

The popup filters on `expired` from this commit, since without that a retired entry would pop up
forever. Everything else a config can do with the flag -- an absolute time in the history, a
dimmer card for a read entry, retiring on the X rather than dismissing -- is left to the config
pass that follows.

## 0101. `desktop_entry` and `reply_placeholder` are carried from their hints

Two more `Notify` hints read and carried, each because a consumer now exists.

1. **`hints["desktop-entry"]` becomes `desktop_entry: Option<String>`.** ADR-0091 left it unbuilt
   for want of a consumer and `util.group_notifications` is that consumer: it groups on `app_name`
   and says so is the wrong key. A desktop id is what `oblisk.applications`'s `by_app_id` is built
   to be looked up by (ADR-0061), so a config can also draw the localised `Name=` and the `Icon=`
   of the real application instead of trusting a sender's self-description. Carried as sent up to
   `summary`'s 128-byte cap, and refused entirely when it holds a `/`: a desktop file id never
   does -- the spec turns a subdirectory into a dash -- so one that does is not an id, and the only
   thing it could do is aim a config's lookup at a path.

2. **`hints["x-kde-reply-placeholder-text"]` becomes `reply_placeholder: Option<String>`.** KDE's
   extension and the one Telegram, Fractal and friends actually send alongside `x-kde-reply`, which
   `has_reply` already honours. Capped like a button label, which is roughly what it is. Empty is
   carried as absent, since an empty placeholder is no placeholder and the config's own "Reply"
   should win.

Neither is validated further. A desktop id that names no installed application is a key that
misses in `by_app_id`, and the config falls back to `app_name` exactly as it does today; a
placeholder is text drawn in a field. Both are `nil` for the great majority of `notify-send`
callers, which set neither.

## 0102. `textfield` gains `on_cancel`, and Escape gives the field up when it is declared

ADR-0092 decision 6 made Escape clear a plain field and keep the focus, and gave the reason: a
config cannot observe focus, so a field that silently stopped taking keys would have no way to say
so on the glass. That reasoning is right and it is also exactly the gap. The reference config
closes a reply on Escape (`Keys.onEscapePressed` on the `TextField`), and here Escape emptied the
box and left the user in it, with the Reply button's row still open and the surface still holding
the keyboard `Exclusive`ly -- so Escape, the key that means leave, left nothing.

1. **`on_cancel: fun()` on a plain `textfield`.** Declared alongside `on_change`/`on_submit`; it
   does not on its own make a field focusable, since a field nothing can read is still not worth
   the keyboard. Nothing changes for a masked field, whose Escape is the lock screen's and is
   settled (ADR-0092).

2. **Escape on a field that declared it clears, drops the focus, and then calls it.** In that
   order. The buffer is emptied and `on_change("")` fires if there was text, so a bound draft
   resets; the focus is released; and `on_cancel` runs last, because what it will usually do is
   remove the field or drop the surface's `keyboard_interactivity`, and it must not find the focus
   still pointing at a node about to go. Escape on an empty open field is still a cancel -- the
   field was open and the user asked to leave it -- with no `on_change`, since nothing changed.

3. **Escape on a field without it behaves as before.** Clear and stay. ADR-0092's argument holds
   unchanged for a field that cannot be told; this ADR only adds the way to be told.

4. **Not a general key event.** The reference config also dismisses a bare popup card on Escape,
   which needs a surface-level key handler and a keyboard-holding surface with no field in it.
   Nothing here has asked for that and `keyboard_interactivity` is deliberately bound to "a reply
   is open" (popup.lua), so a bare card never has the keyboard to receive an Escape on. Left for
   whoever first needs a keyboard-driven surface that is not a text field.

`edit_plain_buffer` is the pure half, split out so the rule is tested without a seat: the four
outcomes -- clear-and-stay, clear-and-cancel, cancel-on-empty, and typing/submitting never
cancelling.

In the notification card, `on_cancel` is `ui.close_reply()`: the row goes, `reply_id` returns to
zero, and the surface's `keyboard_interactivity` follows it to `"None"`, which is the same path the
Send button already takes. Verified live: Reply, type, Escape -- the field and the Send button are
gone, the popup no longer holds the keyboard, and nothing was sent.

## 0103. `applications:open_url(url)`, so a link in a notification body can be opened

A notification body's spans carry an `href` and the field's own doc said "carried as text, not
opened: launching it is a config's decision". It was not a decision a config could make. Nothing in
the Lua surface runs a program: `applications:launch` takes a desktop file id and nothing else
(ADR-0061 decision 3, deliberately), and `process.run` pipes and holds a child the shell then owns.
A link a config could draw but not follow is a link, and the reference config opens them with
`Qt.openUrlExternally`.

1. **On `applications`, not a new capability.** It is the capability for running things the user
   asked for, and "open this in whatever handles it" is a launch with the desktop deciding the
   program. `xdg-open`, detached in its own process group like `launch`, so a generation swap does
   not reap the browser it started.

2. **Allowlisted schemes: `http`, `https`, `mailto`.** Not `file:`. Every path a notification hands
   this shell runs through a trusted-root check precisely because a body is untrusted text, and
   "open this local file" is the one thing it must not be able to say -- the reference config's
   `safeUrl` lets `file:` through, and that is the one place this does not follow it. Not
   application schemes (`tg:`, `spotify:`, `steam:`) either: each names a program the URL's author
   chooses, and a body should not choose programs. Extending the list is one constant, when a
   specific scheme is wanted for a specific reason.

3. **No whitespace or control character, and under 2048 bytes.** Nothing legitimate carries them,
   and an argument holding a newline is how one log line becomes two. The cap is far above what a
   512-byte body can hold and exists for a URL a config built itself.

4. **Refused, not sanitised.** A URL that fails is logged with the reason and nothing runs. Fixing
   it up -- prepending a scheme, stripping a space -- would be this shell guessing what the sender
   meant, on the input it trusts least.

The card side, where a link gets a button or an underlined run to press, is the config pass; this
is what that pass presses.

## 0104. `text.content` takes styled runs, drawn in the family's own bold and italic faces

A notification body arrives as spans carrying `bold`, `italic`, `underline` and `href` (ADR-0033),
and `text` took one string, so `util.notification_body` flattened them and every `<b>Alice</b>:
hi` drew as `Alice: hi`. A link was a stretch of body text that looked like the rest. The reference
config renders the body as Qt rich text and this was the largest gap left between the two.

Node-level styling -- a `bold` on the whole `text` -- was considered and does nothing for the body:
the mixing is inside one wrapping paragraph, and a paragraph cannot be a row of nodes because a
`row` does not wrap. So the runs had to go through the pipeline, and the pipeline was a `String`
end to end: parsed as one, measured as one, rewritten by wrap and elide as one, painted as one.

1. **`content` accepts an array of runs beside the string it always took.** `{ text, bold?,
   italic?, underline?, color? }` is a `NotificationSpan` minus `kind` and `href`, on purpose: a
   body's text spans can be handed over as they arrive, with `href` mapped to an underline and a
   colour by the config, which is where "what does a link look like" belongs. An image span has
   no `text` and is refused with a message saying to leave it out, rather than drawn as nothing.

2. **Internally the runs are byte ranges over one string, not a list of strings.** `StyleRun` is a
   range plus the four style fields; `content` stays the joined `String`. Wrapping, eliding and
   `\n`-joining were already string surgery in one place, `fit_text_to_box`, and a range is what
   survives surgery: `Fitted` appends slices of the *source* and re-bases whichever runs overlap
   each slice. `ShapeResult` grew `line_ranges`, parallel to `lines`, because cosmic-text hands back
   a line's text and the ranges are what let a run follow it across the break. An ellipsis takes
   the style of the character it replaced, so a truncated bold sentence ends in a bold ellipsis.

3. **The shaper sees only the half that changes a measurement.** `FontRun` is range, bold, italic;
   an underline or a colour never reaches the worker, so two contents differing only in colour
   share one memo entry. Runs are part of the memo key for the same reason `line_height` is: a
   bold prefix measures wider and a hit that ignored it would return another request's answer.

4. **Real faces, resolved once, for the primary family only.** `fonts::resolve_chain` asks
   fontconfig for `family:weight=bold`, `:slant=italic` and both, and loads each file that comes
   back as the same family; a family shipped as one `.ttc` resolves every variant to the file
   already loaded and costs nothing more. Fallback entries stay regular: they are there for
   codepoint coverage, nothing asks CJK for a weight, and a `Noto Sans CJK` bold is another 16MB
   mapping for no visible glyph. A variant the family does not ship draws in the regular face, on
   both sides, so measurement and paint still agree.

5. **`font_chain_data` is per face, not per file, and picks by weight.** femtovg was handed face 0
   of every file; for a `.ttc` whose face 0 is a Thin, that was a latent disagreement with
   cosmic-text's own weight-400 pick that no installed chain happened to trigger. The painter now
   holds one chain per variant -- the primary's face for it, then every fallback -- so per-glyph
   fallback works the same in bold as in regular. `fonts[0]` is still the regular face.

6. **A styled line is painted piece by piece, advanced by femtovg's own measurement.** femtovg's
   `set_text_align` can place one run; a line of several is anchored from its pieces' total width
   instead, and each piece drawn `Align::Left` at the running x. An underline is a filled rect one
   pixel or `font_size / 16` thick, whichever is more, just under the baseline. A plain line takes
   exactly the path it always did.

7. **Colour fades with `opacity` like the node's own.** A run's colour goes through the same `fade`
   as `foreground`, so a card fading out does not leave its links at full strength.

The stub probe learned to split `string|TextRun[]|Bound` into three members -- it treated any
bracket as an unsplittable spelling, which was right for `("SlideX"|...)[]` and wrong for an
array suffix -- so `TextRun[]` is probed like every other type rather than skipped.

Tests: the parser's joins and refusals; `segments` splitting a line at run boundaries, including a
run that crosses a wrap; runs following their text across a wrap and an elided bold prefix ending
in a bold ellipsis, through the real `Scene`; `line_ranges` slicing the source to each line past an
explicit newline; a bold run measuring wider than the same text regular; and cosmic-text and
femtovg agreeing on a bold run's width to 2%, the same divergence test that guards the regular
chain, with the bold chain confirmed to lead with a face of its own.

Verified live in the config commit that follows: a body sent as `<b>Alice</b>: see <a
href="https://example.org">this</a>` draws the name in bold and the link underlined in the accent.

## 0105. The notification config pass: what the four Rust changes let the cards do

ADR-0100 through ADR-0104 added `expired`, `transient`, `desktop_entry`, `reply_placeholder`,
`on_cancel`, `open_url` and styled runs. This is the config pass that spends them, together with
the things the payload already carried and the cards did not read: `urgency`, `actions[].icon_name`,
`dnd`, `oblisk.lock`, and a timestamp that `os.date` can format. All Lua; nothing here changed the
engine.

1. **The body is drawn as it arrived.** `util.notification_body` maps text spans to `TextRun`s and
   a link to an underlined run in the accent -- the config decides what a link looks like, the
   engine draws runs. Each distinct `href` also gets a button labelled with its host, which calls
   `applications:open_url`. A button rather than a tap on the underlined words, because the engine
   hit-tests nodes and not glyphs, and the whole message is already a button whose click is the
   sender's default action; a link tap that also fired that would open the page and take the card.
   Inline images are drawn under the text, small.

2. **Grouped by desktop id, named and iconed by the installed application.** `group_notifications`
   keys on `desktop_entry` when the sender set one and falls back to `app_name`; the id is looked
   up in `applications.by_app_id`, so a Telegram notification is headed "Telegram Desktop" with
   Telegram's own icon rather than whatever string the sender chose. Transients are left out of the
   history's grouping and kept in the popup's.

3. **Critical first, then newest, then key.** The mirror's `_compareGroups`, and the tiebreak on
   the key is not decoration: two groups with the same second would otherwise swap places from one
   pass to the next, since `table.sort` is not stable.

4. **The border says the urgency.** Low fades into the glass, normal carries the accent, critical is
   red, read off the group's newest notification. The mirror's `_urgencyConfig`, at border opacity.

5. **Icon-only actions.** A sender that set `action-icons` gets its glyphs drawn from the theme
   beside, or instead of, the label -- a media notification's prev/pause/next is three glyphs.

6. **Do-not-disturb is wired.** `set_dnd` exists since ADR-0033 and no config file called it. The
   history panel's header has the toggle; the bell shows the off glyph while it is on; the popup
   stands down for everything but a critical notification, which is the one urgency the mirror lets
   through DND. The Supervisor's flag already muted the sound, so one toggle now quiets both.

7. **No popups while locked.** `oblisk.lock.active` empties the stack. Not marked seen, so what
   arrived while locked pops up on unlock -- except what expired meanwhile, since the Supervisor's
   countdowns keep running and a five-second notification is `expired` long before the unlock.
   That is the right split without any code deciding it: a critical alert waits, a chat ping does
   not.

8. **The history is sectioned and dated.** "urgent", "today", "yesterday", "earlier" -- the mirror's
   buckets -- as heading items in the one array the `list` draws, since a heading is an item; and
   each card shows "Thu 16:32" where the popup shows "5m", because a history is about when.

Not changed: the card's X still dismisses rather than retires, awaiting an answer (ADR-0098); the
popup does not stand down for an open launcher; and there is still no animation.

Verified live: a critical notification with `desktop-entry: zen` and a body of `<b>Alice</b>: see
<a href="https://example.org/some/page">this page</a> and <i>call me</i>` draws under "Zen Browser"
with Zen's icon, a red border, "Alice" bold, "this page" underlined in the accent, "call me" italic,
and Reply / Archive / example.org buttons; a low-urgency media notification with `action-icons`
beneath it draws three glyph buttons and a dim border. The history shows both under "urgent" and
"today" with clock readings.

## 0106. A press on a link's own words opens it: `href` on a run, `on_link` on `text`

ADR-0105 gave a body's links a button each and said why not the words themselves: the engine
hit-tests nodes, not glyphs, and the message is already a button whose click is the default action.
Then the words were underlined in the accent, which is the one affordance every reader knows, and
pressing them dismissed the notification. An underline that does not open is worse than no
underline; the button was right to exist and wrong to be the only way.

1. **`href` is a field of a run, `on_link(href)` a property of the node.** The run shape is now a
   notification span's exactly, `kind` aside, so `util.notification_body` copies `href` through and
   nothing else changes. The engine carries the string and reports which run was pressed; what to
   do with a URL stays the config's (`applications:open_url`, ADR-0103).

2. **The run under a point is found by re-deriving paint's geometry with the shaper.** `\n` splits
   the fitted content into lines a `line_height` apart, each line is cut at its run boundaries
   (`segments`, moved out of the painter so both sides read one function), the pieces are measured
   by the shaping worker and laid from the alignment's anchor. femtovg measures paint and cosmic-text
   measures this; the two agree to 2% (the divergence tests), well inside the slack a press on a
   word has. Measured with the worker rather than on the GL thread because input has no canvas, and
   every measurement is a memo hit after the first frame anyway.

3. **A link beats the buttons above it, a plain word does not.** The same rule as a `textfield`
   press arming no click (ADR-0092 decision 7): a link inside a card whose whole face is the
   default action opens the page and does not also take the card. A `text` with `on_link` whose
   plain words were pressed is transparent, so the message still activates on a press to its body.

4. **The release must land on the same link.** `ArmedClick` carries the `href` beside the rect: a
   paragraph with two links is one rect, and pressing one then releasing over the other is not a
   click on either. The handler takes the `href` and nothing else -- the rect is the paragraph's,
   and a link is not a mouse button.

The link buttons stay. A three-line elide can cut a link's words off before they are drawn, and the
button is the one control that says where a link goes before it is pressed.

Verified live: pressing "this page" in a body of `see <a href="https://example.org/some/page">this
page</a> when you get a moment` opens the page in the running browser and the notification stays;
pressing "when" beside it dismisses the card as before.

## 0107. The pointer takes a shape over what it is on: `cursor` on every node, a default in Rust

Nothing set a cursor before this. The Renderer bound a bare `wl_pointer` and never called
`set_cursor`, so the pointer kept whatever shape the compositor was showing when it crossed onto a
surface. The reference config gives every `MouseArea` a `cursorShape`, and a bar whose buttons never
say they are buttons reads as a picture of one.

1. **`cursor` is a § 5.1 property, on every kind, by CSS name.** `"pointer"`, `"text"`,
   `"not-allowed"`, `"grab"`, the resize edges: `cursor_icon`'s names, which are also
   `wp_cursor_shape_v1`'s, so the string a config writes is the string the compositor reads. An
   unknown name fails the pass like any other property (`node::parse_cursor`, called from
   `LayoutStyle::parse` and kept nowhere: the pointer path re-reads the name off `properties`).

2. **The default lives in Rust, not in the Lua components.** The reference sets `cursorShape` on
   each component because Qt's `MouseArea` has none of its own. Here the decision has to be made at
   hit-test time anyway, on the pointer path, where only the Renderer knows which node the point is
   on; and the three questions it asks on a press are the three that decide the shape. So
   `layout::hit::cursor_under` walks the hit path innermost-first and at each node takes an explicit
   `cursor` if there is one, else what the node is: a `text` with `on_link` whose link words are
   under the point is `pointer`, a `textfield` is `text`, a `button` with a callable `on_click` is
   `pointer`. Nothing else says anything and the arrow is what is left. A `button` with no handler
   is transparent to the shape as it is to a press, so what the cursor promises is what a click does.
   `dev-config` changes nothing: its components already build a `row` rather than a `button` when
   there is nothing to click, so the default already covers them. The property is for the exceptions
   that do not exist yet: a control that is off, a drag handle, something refused.

3. **Innermost wins, explicit ahead of implied, at each node.** A `cursor = "grab"` on a card still
   yields to a link in its body because the walk meets the link first; a `cursor = "not-allowed"` on
   a `button` beats the button's own `pointer` because the explicit check runs before the kind
   check at the same node.

4. **`ThemedPointer` in place of the bare `wl_pointer`.** SCTK's type speaks `wp_cursor_shape_v1`
   when the compositor advertises it (niri does) and paints from the XCursor theme through `wl_shm`
   when it does not, so `wl_shm` is bound for the first time, for that fallback alone; this process
   still draws through EGL. The shape is sent from the `Enter`/`Motion` arm beside `sync_hover`, and
   only when it differs from the last one sent, since a motion arrives per pixel. `Leave` forgets
   the last shape, so the first event after an `Enter` always sends, which is what the protocol
   asks for: the shape is bound to the enter serial.

Cost: one `hit_path` walk per motion event on top of `hover_writes`'s, and a `link_under` shaping
call only when the pointer is on a `text` that declares `on_link`. Not measured; nothing on the
pointer path has needed to be yet.

## 0108. A reply's keyboard is on demand, and a plain field keeps its draft while it exists

Three reports from one afternoon of using the reply box, all one design: the popup's open reply
held every key on the desktop until Escape or Send, clicking anywhere else changed nothing; the
pointer drifting off the card emptied the field; and once a draft existed, the card's X stopped
working. The first two were the design as written (ADR-0092's `Exclusive`, ADR-0050's clear on
`leave`); the third was a bug this ADR's first cut introduced and its second removed.

1. **`OnDemand`, not `Exclusive`, for a reply.** Exclusive is the lock screen's word: the compositor
   keeps the keyboard on the surface whatever is clicked, and a 355-pixel popup has nowhere for a
   click-outside to land, so nothing could ever release it. On demand, niri moves the keyboard with
   the user -- to whatever is clicked, and under `focus-follows-mouse` to wherever the pointer goes.
   That second half is the compositor's rule and applies to this surface as to any window: while the
   pointer rests on the popup, keys go to it, and a card vanishing under a resting pointer
   re-evaluates pointer focus the same way. Measured: the flip to `OnDemand` on a mapped surface is
   honoured (the `enter` arrives on the same pass when niri chooses to give it), and a click into the
   field takes the keyboard when it did not. The network password keeps `Exclusive`; it is raised by
   a click on the panel and the panel's own click-outside catcher ends it.

2. **A plain field holds its text for as long as its node exists.** `FocusedTextField` was the
   field *receiving keys* and was dropped on the keyboard's `leave`, which made every pointer drift
   a discard. It is now the field *holding the draft*, with a `typing` flag a press sets and a press
   elsewhere clears; whether keys reach it is asked at the moment one arrives -- `typing`, and its
   surface in the keyboard scope -- and the caret is drawn by the same question, so what looks live
   is what a key would land in. Keys with the keyboard elsewhere go nowhere; the draft stays. A press
   back into the same node keeps the buffer, a press into another field replaces it, and the draft
   is dropped when the node is gone from the tree (`hit::contains_node`, checked before each key)
   or its surface is dead. Escape with `on_cancel` still drops it, since that is what Escape means.
   `keyboard_interactivity` moving no longer touches the field at all.

3. **The press that arms no click is the one that landed on a field.** The first cut computed
   "focused a field" from "a plain field is held after this press", which the held draft made true
   for every press on the surface: the X, the Send button, the body. It is `hit.field.is_some()`,
   read before the match consumes it. Verified by the bus: the X on a card with a draft dismisses it
   (`NotificationClosed`, reason 2) where it did nothing a build earlier.

4. **The keyboard is asked for while the field is on screen, not while an id is set.**
   `ui.reply_open` is `reply_id` names a notification still in the feed; both surfaces bind to it.
   `reply_id` goes stale by every door but the field's own -- the X, an action, the sender
   withdrawing -- and a surface bound to the bare id mapped on the next notification asking for the
   keyboard for a field it was not drawing. `close_panel` also closes the reply, so the panel's
   click-outside catcher closes both, which is what a click outside a panel means.

5. **The card's body is inert while its reply is open.** The field is one row of a card whose whole
   face otherwise dismisses (or activates), and a click a few pixels off the field took the card and
   the draft with it. The X is still there for someone who meant it.

Not done: shrinking the popup's input region to its cards. The surface is `notification_stack_height`
tall and its `column` fills it, so under focus-follows-mouse the empty space below the cards also
takes the keyboard while a reply is open. The fix is the column sizing to its content and the list
losing its scroll, which is a trade the mirror's popup also makes; deferred until it is felt.

## 0109. The reply field is always there, the keyboard is asked for on hover, and the input region is what is drawn

ADR-0108 left two things: pressing Reply opened a field you then had to click into, and the popup's
empty space below its cards took the keyboard under focus-follows-mouse. Both had one cause and the
mirror had already avoided it.

1. **Every inline-reply card draws its field. There is no Reply button.** `NotificationCard.qml`'s
   `Loader { active: hasInlineReply }` is exactly this, and it removes the problem rather than
   solving it: the one click the user makes is the click into the field, and that click is the one
   that focuses it. `reply_id` and `open_reply`/`close_reply` are gone with the button. What remains
   is the draft, stamped with the card it was typed into (`reply_draft_id`, `reply_draft`), so a
   Send button on card A cannot send a sentence typed into card B. `send_reply(id)` checks the stamp.

2. **The surface asks `OnDemand` while the pointer is on it, or while a draft is pending.** Measured
   twice: niri does not hand the keyboard to a mapped layer surface on the flip to `OnDemand` (three
   seconds, nothing), and drops it at once on the flip from `Exclusive` to `OnDemand`, so acquiring
   through `Exclusive` is out. What niri does honour is a *click* on a surface that is already on
   demand. So the popup's binding is its own hover signal, which is true before the click into the
   field lands, plus `ui.reply_pending` (non-empty draft, card still in the feed), which keeps the
   ask alive after the pointer leaves for a click-to-focus compositor that would otherwise drop the
   keyboard mid-sentence; under focus-follows-mouse the keyboard has left with the pointer anyway.
   The panel host asks on demand for as long as the notifications panel is showing, since the panel
   itself is the thing hovered; the network password keeps `Exclusive`. A surface that is `OnDemand`
   and not clicked takes nothing, which is why the hover binding is safe against the "a notification
   arrived and stole my keyboard" case ADR-0108's comment guards.

3. **The input region is what the tree draws and what it can click.** `overlay_input_regions` was
   the root's visible direct children, which made a full-surface transparent `column` claim the
   whole 523-pixel box. It now walks into a transparent container and claims a node's box when it
   is solid: a background, a border, any text/icon/image/field, or a `button` with an `on_click`
   (the panel host's catcher is invisible by design and must stay pressable). Everything else is
   click-through, and under focus-follows-mouse focus-through, so the space below the cards no
   longer takes the keyboard, and no longer holds expiry either -- the hover column is unchanged,
   but pointer events simply stop arriving there. This is the docstring's own stated upgrade path,
   and it is the mask the mirror sets (`maskItem: popupColumn`) expressed the way this engine
   already computes regions. The scroll is kept: the list still fills the surface, only the region
   shrank.

4. **The body is inert while a draft is pending, not while a field is "open".** Same rule as
   ADR-0108's, re-keyed to the draft since there is no open state left.

Verified live on niri: a card arrives; the pointer parked in the empty box below it holds nothing
and asks for nothing; on the card it holds expiry and flips to `OnDemand`; one click into the field
brings `keyboard focus entered` and typing lands; the pointer leaving takes the keyboard and keeps
the text; returning brings both back; Send removes the card.

Not taken: a `focus` property on `textfield` for focusing a field the pass just created. It would
have set the field typing without the surface having the keyboard, since on niri only the click
brings that, and the click is now the one into the field.

## 0110. A panel is as tall as its content, up to a cap: `max_width`/`max_height`, and the host card centres under its indicator

Every bar panel shared one card height, `theme.panel_height`, sized for the tallest body and worn by
all of them. Four networks in range sat over 200px of empty glass; the notification history needed
a second, taller number and a rule in `panel_host.lua` for when to switch to it; and the two states
of that rule were both wrong for a feed of one card. `PanelHost.qml` has none of this: its surface is
`panelItem.preferredHeight`, and each panel's list is `Math.min(contentHeight, Theme.itemHeight * 7)`.
The engine could not say that. `Content` is always the content and `Fill` is always the box, and a
`Content` column holding a `Fill` list gave the list nothing (ADR-0077's one-pass reasoning).

1. **`max_width` and `max_height` are base properties.** Pixels only, `[0, 8192]`, on any node; they
   are taffy's own `max_size`, so a `Content` node measures its children and is then capped, while
   the children keep the size they were given. That is what leaves `finish`'s `extent_along` a
   remainder for `scroll_offset` to clamp to: a `list` with `max_height` and a `scroll` is exactly
   the mirror's capped `ListView`. Beside a fixed or `Fill` size the cap is inert, which is the right
   reading -- those already say how big. `a_max_height_caps_a_content_sized_column_and_leaves_the_rest_to_scroll`
   pins both halves.

2. **The host card has no `height`.** It is its content; every panel's list carries a `max_height`
   (`theme.panel_list_height`, or `theme.notification_list_height` for the history), and the
   `height = "Fill"` that every body and section wore is gone, because there is no box left to
   fill. `theme.panel_height` and `theme.notification_panel_height` are deleted with the two-step
   rule. `socket.rs`'s panel measurement now asks the one question that remains -- does the card end
   above the bottom of the surface -- rather than comparing fixed rows against a card height that
   the sections, all invisible at startup, never reached anyway.

3. **The card is centred under the indicator that opened it,** the mirror's `calculateX`, then clamped
   a `spacing.sm` inside either screen edge by hand (`SlideX`, since a layer surface has no
   `constraint_adjustment`). It hung off the indicator's left edge before, which under a bar button
   read as belonging to the button to its right.

4. **The network and bluetooth panels are laid out as the mirror's.** A `panel_header` with the
   radio's master switch (the glyph on a plate that goes dim when the radio is off), `panel_toggle_card`
   as the mirror's tile rather than a settings row, `panel_row` with `selected` (accent ring and
   ground) and a composed `leading`, `panel_action_icon` for a row's quiet red actions, and
   `panel_empty_state` with a glyph. A network row draws what it used to spell: the glyph's bars are
   the strength, a coloured "5G"/"2.4" is the band, a lock badge is the security, the ring is the
   connection. A bluetooth device with an empty `name` is now titled by its address (`name or mac`
   was the bug: `""` is true in Lua). Not carried over, each for a stated reason in the file: the
   hidden-network row, the IP address, Saved/Available sections, the Visible tile, the codec picker.

## 0111. A flex item's cross-axis minimum is `auto`, because taffy 0.14 adds the container's margin to it

The first content-sized panel (ADR-0110) came up a line short: the last notification card in the
history had its bottom border and the card's padding under it cut off, and only in a session.
`socket.rs`'s harness built the same tree from the same config and measured it right. The difference
was where the card sat. The harness anchors at `x = 0`; the session centres the card under the bell,
a `margin.left` of 1521 on a 1920 output. `OBLISK_DUMP_LAYOUT` (below) showed the body column 13.2px
shorter than its own children -- one line of `font.sm` -- and a probe on the measure callback showed
why: the wrapped body was measured once at a known width of exactly 1521 (one line), and then laid
out at 378 (two lines). The card's height was taken from the first answer.

That is a taffy 0.14.0 bug, reproducible against taffy alone. In `determine_flex_base_size` and in
`determine_container_main_size`, a flex container measuring its children clamps the cross space it
offers each child to `child.min_size.cross + constants.margin.cross_axis_sum` -- the *container's*
margin, where the child's was meant. With `min_size` written as `Some(0)` that is a floor equal to
the margin; with `min_size` `None` the `maybe_add` is nothing. This engine wrote the zero on both
axes, so that `Fill` items collapse instead of being floored at their content (the comment on
`taffy_style`'s `min_size`), and so every stretched child of a column with a left margin was offered
that margin as its width.

1. **`min_size` is zero on the parent's main axis and on both axes of a stacking cell, `auto` on a
   flex item's cross axis.** CSS gives the cross axis no automatic minimum, so `auto` there is the
   zero that was being written out, and the bad sum has nothing to add to. The main-axis zero stays,
   since that is the floor the comment exists to remove. Not a `[patch]` of taffy: one line of
   mapping against a fork to carry. `a_containers_own_margin_does_not_widen_what_its_children_are_measured_at`
   pins it at the margin that showed it, and
   `the_shipped_dev_configs_history_card_is_as_tall_as_the_notifications_in_it` pins the panel that
   showed it, anchored where the bell is, at the output the session ran on.

2. **`OBLISK_DUMP_LAYOUT=<instance id>` prints that surface's resolved tree after every pass.** Kind,
   rect and a `text`'s content per visible node, to stderr, off unless asked. The harness reproduces
   what it was told to build; the layout that is wrong in a session is the one it was not told
   about -- here the anchor, the output's scale, and a feed the Supervisor had by then. Reading the
   live answer took one relaunch; guessing at it took an hour.

## 0112. A launcher's four missing primitives: `autofocus`, `on_navigate`, `scroll:reveal`, and `oblisk set`

`modules/global/launcher.lua` was a scrolling list of every application, opened from one bar
button. Against `Modules/Global/AppLauncher.qml` it lacked the half that makes a launcher: a
search box that is typable the instant it opens, arrow keys that walk the results, a list that
follows the selection, a subtitle under each name, and a way for a compositor keybind to open it.
Each of those was blocked in the engine, not in the config, and this is a framework the config is
one example for -- so the engine grew the primitives and the config then used them.

1. **`textfield.autofocus = true` takes the keyboard without a press, and takes it empty.**
   ADR-0092 decision 3 declined an arm-on-`enter` for plain fields because the notification card
   has several and the sole-field rule cannot pick one. A property says which. It arms on
   `KeyboardHandler::enter` when no masked field armed and no plain field is already typing in the
   scope, and again from the once-a-turn hook beside `arm_secure_focus_if_the_scope_now_declares_one`
   when the tree changed under a focus already held -- but never to re-take the very field a press
   elsewhere just stopped, since that press was the answer. It arms only on a surface that is still
   a live `wl_surface`: a closed launcher keeps its tree and, with no `leave` owed for a destroyed
   surface, its focus id, and without the check every turn armed and the next prune dropped.
   *Empty* is a deliberate exception to ADR-0108's draft-keeping: that rule is for a field the
   user left and returns to by hand; a field the engine hands over unasked must not open on last
   week's word. `on_change("")` fires when there was text, so a bound `state` follows.
   Two `autofocus` fields on one surface: first in document order, since that is a config mistake
   to pick through rather than a secret to refuse routing (`autofocus_field_in_scope`).

2. **`textfield.on_navigate(key)`, for the keys a single-line field has no edit for.** `key_action`
   now names Up, Down, Page Up, Page Down, Tab and Shift-Tab (`"up"`, `"down"`, `"page_up"`,
   `"page_down"`, `"tab"`, `"backtab"`) as `KeyAction::Navigate`, ahead of the `utf8` arm that had
   been dropping Tab as a control character. A plain field with the callback hears the name; the
   buffer and caret do not move and `on_change` does not fire; repeats fire, so a held Down keeps
   walking. A masked field ignores them, as before. This is still not `on_key` (§ 5.2, ADR-0050):
   the set is closed, the keys carry no text, and a field has to be typing for any of them to
   reach Lua. Tab meaning "down" is the config's call, not the engine's -- the mirror makes it, and
   `launcher.lua` follows, in one line.

3. **`scroll(name):reveal(index)` is the one thing a config may say to a scroll signal.** ADR-0069
   decision 2 keeps the offset engine-owned because the config cannot clamp what it cannot measure,
   and that still holds; what the config *can* know is which child it wants to see. `reveal` stores a
   one-shot ask on the `Scroll` kind and marks the scene dirty; `finish` takes it on the pass that
   positions the viewport, moves the asked offset the least distance that puts the `index`-th visible
   child's border box inside `content_main`, writes it quietly, and lets `scroll_offset` clamp it as
   it would a wheel ask. Already in view moves nothing; past the end lands on the end; no such child
   changes nothing. One-shot, so the wheel is free the moment the pass is done -- a reveal that held
   would snap the list back under a user scrolling away from the selection.
   `a_reveal_scrolls_the_least_distance_that_shows_the_child` pins the four cases.

4. **`AppSummary.comment`.** `Comment=` is the one-line description a launcher draws under a name
   and matches against, and the scan was already holding the parsed group it lives in. `None` when
   absent, which is common, so the config hides the line. Still unlocalized, on ADR-0061's terms.
   `Keywords=` and `GenericName=` are not carried: the mirror matches name and comment, and nothing
   here asked for more.

5. **`oblisk set <name> <value>` and `oblisk toggle <name>` write a `state` from outside.** The
   Supervisor's socket only knew Renderer generations, so the bar button was the launcher's one way
   in and a keybind had none. The CLI connects with `CONTROL_CLIENT_GENERATION` (`u32::MAX`), which
   `handle_connection` neither registers nor replays snapshots to, sends one
   `RendererFrame::SetState` and hangs up; the Supervisor forwards it as `SupervisorFrame::SetState`
   to the authoritative generation, the one fact the client cannot know; `lua::signal::write_state`
   applies it through `Signal::reseed`, the same marshal check and dirty mark as `:set()`, and
   refuses by name to stderr when no evaluation declared that state or a toggle finds no boolean.
   `state(name, initial)` is already name-keyed and already the config's one writable signal
   (ADR-0044 decision 5), so this adds no Lua surface at all: a keybind writes exactly what an
   `on_click` may. A value is JSON when it parses and a string otherwise, so
   `oblisk set panel_kind notifications` needs no quoting inside a compositor config. Rejected: an
   `IpcHandler`-style table of named functions -- a function can do anything, a state write can do
   only what the config already wired to that state.

6. **The launcher is a layer surface, and the example of all five.** A screen-sized `panel` with a
   scrim, a catcher, and `keyboard_interactivity` bound to `launcher_open`, so it takes the keyboard
   on map and gives it back on unmap; the `window` it was is what niri tiled into the layout. The
   ring is one `computed` over the chosen id and the results, which every row asks one `map` of,
   so three hundred rows stay inside the graph's 5ms budget. Not carried from the mirror: the
   calculator and currency rows, since both end in "Enter to copy" and this engine has no
   clipboard; a result that can be read but not taken is half a feature. The web row is kept,
   `applications:open_url` being already there.

**Amendment (same day), after checking the launcher against `AppLauncher.qml`'s pointer handling.**
The mirror forwards every pointer motion to the launcher and lets hover move the selection only once
a motion with a *different* position has arrived since the last open, keypress or query change
(`hoverSelectionArmed`). Ours had no per-motion events by design (ADR-0062), and a live check showed
the one case that mattered: opened from a keybind under a parked mouse, the row under the cursor took
the ring before anything moved, because the compositor's pointer `Enter` on map was treated as a
crossing. The same check found the launcher reopening on its old selection and scroll. Four changes:

7. **`on_hover` fires for a pointer that moved, not for a tree that moved.** `sync_hover` takes a
   `fire` flag: `Motion` and `Leave` fire, `Enter` does not, and neither does the new
   `refresh_hover_after_layout`. An `Enter` with no motion behind it is a surface appearing under a
   resting pointer; a pointer that enters by moving sends a `Motion` a few milliseconds later, and
   that one fires. Hover *signals* still update on every one of them, so what is drawn as hovered is
   always what is under the pointer. The notification stack's expiry hold stops arming when the popup
   appears under a parked cursor, which is the right answer there too.

8. **After a re-resolve, the hover signals are rewritten at the pointer's last position, silently.**
   `App::pointer_at` remembers the surface and position from `Enter`/`Motion` and forgets on `Leave`;
   `refresh_hover_after_layout` runs after each `re_resolve_if_dirty` that did work. A `reveal` or a
   filter change slides rows under a still pointer, and without this the row that slid away kept its
   tint off the viewport while the one now under the pointer had none. No callback, since nothing
   crossed anything.

9. **Every `autofocus` arm calls `on_change("")`,** not only when the field held text. It is the one
   moment a config can call "the field just opened", and the launcher resets its selection and
   scroll in it -- the mirror's `processInput("")` on `active`. A config-side reset had no other
   hook: a launcher closed by a row click or the scrim never saw Escape's clear.

10. **Two-stage Escape is the config's, and costs it two locals.** With text, Escape clears and
    stays; empty, it closes. The engine already clears and lets go of the keyboard before
    `on_cancel` runs, and the autofocus arm takes it straight back, so "stay" is free.

## 0113. What a code review is worth: four fixes out of two hundred findings, and the two that were the review's own doc drift

An outside pass over the whole tree (`reviews/pass1`, twelve files, roughly 240 findings) was
verified finding by finding (`reviews/pass2_confirmed`). Most of it held up as description and
almost none of it was worth acting on: the single largest category quotes a `ponytail:` comment and
reports its content back as a discovery, which is what those comments are for. Its `[DEAD_CODE]`
label was wrong three times in five, its one P0 self-heals in about a second, and one of its P3s
was a few thousand file copies per update check. Four things came out of it worth doing, and two of
them turned out to be one bug wearing two hats.

1. **`updates` links the pacman `local/` db in, rather than copying it.** `checkupdates` itself is
   `ln -s "${DBPath}/local" "$CHECKUPDATES_DB"`, because `syncdbs_mut().update()` only reads that
   directory. The copy came from ADR-0034's own throwaway prototype -- the program written to prove
   the sync needs no `fakeroot` copied `/var/lib/pacman` wholesale, and the copy shipped with the
   answer. It walked ~1,500 package directories off disk on every scheduled check. What the copy
   did buy is now stated where it was only implicit: a check reads the live directory, so a
   concurrent real install can be seen mid-write, the same window `checkupdates` lives with. The
   link is refused when `local/` is not a directory: a dangling link is not an error to `alpm`, it
   is an empty installed set, and every package on the mirror would read as an update.

2. **`sysinfo` is configured by the module that reads it.** All three pollers start dormant and
   wait for an interval (ADR-0035), and no file in `dev-config` ever named one, so the settings
   panel's two readouts sat at their pre-first-sample `0%` for the life of the process: wired,
   started, and never asked for a number. The call goes in `system_info.lua`, not `shell.lua` --
   the module that wants the samples is the one that says how often. `temp_interval` stays at zero
   on purpose, since nothing reads `temp_cores` or `temp_gpu`. **`updates` has the identical gap**
   and is deliberately left alone: starting periodic pacman network checks is a decision a config
   author makes, not a default a framework example should smuggle in.

3. **`lua-meta` stopped refusing code the engine accepts.** Six types were narrower than the parser:
   `margin`/`padding` took `Edges` but not the bare number `parse_edge_insets` broadcasts;
   `border_color` took `Edges`, which is integers, where the engine wants per-edge hex *strings*
   (now `BorderColors`); `border_color`/`border_width` took no `Bound`; `list.source` took only
   `Bound` where a literal array is legal; `PanelProps` redeclared `margin` and dropped both;
   `PopupProps.offset` demanded both axes where each defaults to `0`. All latent, because
   `dev-config` happens not to write any of those forms, so `lua-language-server --check` stayed
   green over a stub that would have failed the next person to try one.

4. **`Screen.scale` is an integer scale factor, and `theme.lua` believed the stub instead of the
   engine.** The stub called it "the fractional output scale, e.g. `1.25`. Divide by it once", and
   `dev-config/oblisk/config/theme.lua` dutifully divided -- by a `screen.height` that
   `wayland/output.rs` had already divided, as its own test says in as many words ("a 3840x2160
   panel driven at scale 2 is 1920x1080 of compositor space"). A 4K HiDPI panel therefore read as a
   540px-tall desktop, which floors the responsive factor at `0.75`, so every HiDPI session drew the
   entire shell at its smallest tokens. Invisible on the 1x display this is developed on. This is
   the one finding in the whole review that was worth the exercise, and the review only got halfway
   to it: it caught the wrong stub and not the config that had already acted on it.

Also fixed: `share/starter/shell.lua` indexed `s.time` in a `map` closure that runs once against a
nil `s`, so a new user's first boot printed a Lua error and "no scene was applied at startup;
surfaces still bind, and paint nothing" before recovering a second later on the first `system`
push. The file's own header already teaches the nil rule the body broke.

### Amendment, same day: `updates` is wired up, and the bar button stops installing

Decision 2 left `updates` dormant on the argument that starting network checks is the config
author's call. Asked for it, so: `updates.lua` invokes `configure({ interval = 3600 })`, and two
things fell out of switching it on.

11. **The first check runs when one is due, not one interval later.** `run_check_task` consumed
    `tokio::time::interval`'s immediate first tick with the comment "consume it unused", copied from
    the CPU sampler, where it is needed because a percentage is a delta between two reads. An update
    check is a point query. Consuming it meant an hour of blindness after every login -- and worse,
    because the controller outlives the generation that configured it, every config reload
    reconfigured the interval and restarted that hour, so a day of editing never checked at all.
    `first_check_is_due` now decides: nothing checked yet in this process, or the last success is at
    least an interval old. A fresh boot checks now; a reload inside the hour does not re-sync. This
    is `sysinfo`'s SYS-03 in another file, and the reason `sysinfo`'s own copies of that line stay
    is that they are the same bug -- to be fixed when someone reads those numbers.

12. **The bar button is a readout, because installing belongs in front of the package list.** It was
    an `icon_button` whose click invoked `install`, guarded only by `count == 0` -- which is `false`
    when `count` is nil, so the one case the guard existed for was the one it let through. That was
    unreachable while the module was invisible. Switching the module on made it reachable, and the
    first click on the new badge launched a real `pkexec pacman -Syu`; it died at "Error creating
    textual authentication agent" with nothing upgraded, which is luck, not design.

    `ArchChecker.qml` never installs from the bar: a left click with nothing pending re-checks, and a
    left click with something pending -- or any right click -- opens `UpdatePanel.qml`, which lists
    every package with its old and new version, the total download size, the last check time, and an
    "Update" button under all of it. `UpdateService.qml` polls every 15 minutes, persists
    `lastSuccessfulCheck` so a restart resumes the remainder rather than re-syncing, and notifies
    only when a package appears that was not in the last set.

    Neither of the mirror's two click paths is reachable here: the capability has `configure` and
    `install` and no `check` (so nothing to re-poll with), and there is no panel to open. Passing
    `nil` where `icon_button` takes an `on_activate` returns a `row` instead of a `button`, so there
    is no click to land at all, and the `slot` goes with it -- a readout that lights up under the
    pointer is a button that is lying. The badge says how many; installing waits for the panel.

13. **`check`, and the facts a panel needs.** Wiring the module up (decisions 11-12) left it with no
    way to ask a question and no way to describe an answer, so: `updates:check()` runs one check
    now, answered under a schedule *and* while dormant -- a config wanting the button and never the
    timer is a shape to allow, not to work around. It is refused while a check is running, the way
    `install` refuses a second transaction, and the ticker arm and the manual arm now share one
    `run_one_check` that raises `checking` with a push before the sync and lowers it with another
    after, because a "checking" that is only visible afterwards is not visible at all.

    `install_error` used to be `"pkexec pacman exited with exit status: 1"`, which is the engine
    writing English into a config-facing field: unreadable to a user, unrewordable by a config,
    untranslatable. It is now two facts. `install_exit_code` is what pacman answered.
    `install_log` is the last 200 lines of both streams, which is where pacman says *why*.
    `install_error` keeps only the case where the Supervisor never got an answer at all -- it could
    not spawn `pkexec`, or could not wait on it. The mirror's `_detectErrorMessage`, which maps
    those lines to "Network error" / "Insufficient disk space" / "Authentication failed", is
    wording, and stays in the config where it can be changed and translated.

    The same line puts `consecutive_check_failures` here as a count and leaves "warn after five" in
    the config; leaves "completed until dismissed" to a Lua `state()` rather than the service, which
    is where `UpdateService.qml` keeps `dismissResult()` only because QML has no seam there; and
    keeps `pkexec pacman -Syu` fixed rather than taking a command from the config, because the
    capability owns what is privileged and `process.run` already owns what is not. The log push
    rides the progress lines rather than every line: one `Changed` re-resolves every surface in the
    generation, and pacman writes a download meter.

14. **`system:write_state`, and where a remembered value can and cannot come from.** The IDL row
    existed and nothing implemented it. It stores one scalar under one key, rewrites `state.json`
    through a temp file and a rename, and pushes so the config reads back what it stored. §3.2 says
    a key is "alphanumeric", which forbids `updates.last_check` and would push every config with two
    modules towards `updateslastcheck`; widened to allow `_`, `-` and `.`, since namespacing is the
    actual use and none of the three is any less safe as a JSON object key. Values stay §3.2's three
    scalars: a config wanting structure has `json.encode` and a string to put it in, and `state.json`
    stays a file a person can hand-edit.

    `updates:configure({ interval, checked_at })` takes the remembered time beside the interval, as
    a seed and never an override -- a check this session ran is fresher than anything a config can
    say, and this must not move `last_successful_check` backwards.

    **The loop does not close yet, and the missing piece is a hook, not a field.** A config can only
    cause a side effect from an input callback (ADR-0044: a config is a pure function of state), so
    nothing can write `state.json` when a *check succeeds* -- there is no "on change". And the read
    fails from the other end too: `configure` runs at module load, which is the first evaluation,
    where `oblisk.system` is still nil and the remembered value cannot be read at all. Both halves
    want the same thing, a way to run a side effect once after a capability's first push, and that is
    a design decision about purity rather than a field to add, so it is not taken here. `write_state`
    is useful today for what is already input-driven -- a launcher's frecency counter is written on a
    click, which is exactly the shape that works.

15. **The panel, and what it proved about the split.** `modules/bar/panels/update_panel.lua` is the
    sixth panel in the host and is entirely wording, formatting and thresholds over facts the
    capability publishes unchanged. The mirror's `_detectErrorMessage` is a table of eight phrases
    matched against `install_log`; its `_failureCount >= 5` is one comparison; `dismissResult()` is a
    Lua `state()`; the install duration is `install_finished_at` minus a start the *click* stamped,
    which is the one moment a config is allowed to write anything at all (ADR-0044). None of that
    needed a line of Rust, and none of it should have had one.

    Two things the mirror has are absent rather than faked: a spinner, because nothing animates
    without a per-frame property (ADR-0021), and a copy-the-log button, because there is no
    clipboard primitive. The bar badge opens the panel and the panel header re-checks, which is
    `ArchChecker.qml`'s split minus its click-to-recheck -- that one needs the button to exist while
    nothing is pending, and this bar hides it (decision 12).

    `components/action_button.lua` came out of `notification_card.lua`, whose own comment said one
    call site is a local and two in agreement are a component. The second caller agreed about all of
    it but the ground, so the component took one option: an action being offered is accent, and a
    "close" that tidies away a result already read is quiet.

## 0114. `polkit` joins the roster, and the agent holds its reply until the prompt is answered

The polkit agent registered, received `BeginAuthentication`, and returned from it at once, forwarding
the challenge to a log line. polkit's own docs for that method say the agent "should not return
until after authentication is complete" and must return `org.freedesktop.PolicyKit1.Error.Cancelled`
when the user dismisses the dialog; polkitd reads an early return with no
`AuthenticationAgentResponse2` as a finished, failed authentication. Nothing could have authorised
through this agent, and nothing in Lua could have drawn the prompt: there was no `oblisk.polkit`.

1. **`polkit` is a roster capability.** `PolkitState { active, message, action_id, icon_name,
   authenticating, error }` and one action, `cancel`. ADR-0070 decision 5 kept it off the roster
   because it pushed nothing; now it pushes what a dialog is made of, and the roster is what gives it
   a Lua member, stubs, a schema and hydration for free. The `secure_submit` start path stays, so a
   prompt registers the agent whether or not the config reads the member. Built in `main.rs` and
   pushed from its loop like `lock` (`without_channel`), since all three of its inputs land there.
2. **The reply is held.** `begin_authentication` awaits a `oneshot` the controller answers: `Ok(())`
   on a success, `Err(Cancelled)` on the dialog's cancel or polkitd's `CancelAuthentication`. zbus
   spawns a task per method call, so the cancel is delivered while the begin is still waiting.
3. **One challenge at a time.** A second `BeginAuthentication` while one is open is answered
   `Cancelled` on the spot; one password field cannot be typing for two callers, and the refused
   caller retries or fails on its own. ponytail: a queue is the upgrade if two mechanisms ask at once
   in practice.
4. **A failed password keeps the prompt open.** `error` carries the lock screen's words for the same
   `PamOutcome`, the field stays, polkitd keeps waiting. Cancel is the way out; there is no attempt
   cap here because `pam_faillock` already has one.
5. **PAM runs in polkit's setuid helper, not this crate's worker.** The first cut ran
   `pam_worker`'s re-exec'd worker and then called `AuthenticationAgentResponse2` itself; polkitd
   answered "Only uid 0 may invoke this method". That is the whole reason libpolkit-agent ships
   `/usr/lib/polkit-1/polkit-agent-helper-1`: it runs PAM as root and makes that call before printing
   `SUCCESS`. `pam_worker::run_polkit_helper` speaks its line protocol (cookie in, every
   `PAM_PROMPT_*` answered with the one password, `SUCCESS`/`FAILURE` out), spawned like the lock's
   worker with the same `Drop` backstop, so a wrong password no longer stalls the loop for
   `pam_unix`'s delay. The Supervisor no longer needs polkitd's Authority proxy for anything but
   registration. The re-exec'd worker stays for `oblisk.lock`, which has no polkitd to satisfy.

6. **`button { submit = true }`.** The mirror's Authenticate button. A click cannot hand a password
   to Lua (ADR-0005), so the button does what Enter does: the release sends the scope's armed
   `secure_submit` field. Clickable with or without `on_click`.
7. **A destroyed surface gives up the keyboard focus it held.** The compositor sends no `leave` for
   a surface its client destroyed, so after the prompt closed `keyboard_focus` still named it, the
   tree still declared its field, and every pass re-armed the field and pruned it again -- a log
   line and a scrub per frame. `unmap` clears the focus; arming also refuses a surface that is not
   live. A `textfield`'s one line is drawn in the middle of its box while here, which it was not.
8. **Only a field takes a masked field's focus.** ADR-0050 decision 4 had a press anywhere but
   the field clear it, which scrubbed the buffer, so a click on the dialog's scrim or card threw the
   password away, and the Authenticate button first needed a carve-out. QtQuick moves focus only to
   something focusable, and the prompt has three controls: the field and two buttons. A press on
   no field now leaves the masked focus alone; the field still draws its dots, so nothing is hidden,
   and a secret still cannot reach another field without the scrub the A-to-B retarget does. The
   `leave` and `unmap` clears stand: those are the user demonstrably elsewhere.

Not mirrored from `PolkitDialog.qml`: Escape-to-cancel (a masked field's Escape clears and stays,
ADR-0092; the Cancel button is the way out) and the `●` mask (`mask_character` is one byte).
`isResponseRequired`/`inputPrompt` have no equivalent under ADR-0028's one-shot protocol, where a
password is always the answer.

## 0115. A capability push can run a handler: `on_change`, and the five things it unblocked

ADR-0044 made a config a pure function of pushed state: a `map` callback runs during scene
resolution, may rerun on the same inputs after a rollback, and so may not act. Input callbacks
(`on_click`, `on_hover`, `on_submit`) were the only place a side effect could start. That left a class
of thing the reference shell does that no amount of Lua here could: BatteryService.qml's low-battery
`notify-send`, OSDService.qml's "charger connected", PowerManagementService.qml's suspend at 8%,
UpdateService.qml's "Updates Available", and the `state.json` write ADR-0113 decision 14 stopped short
of. All five wait for a *push*, not a click. ADR-0113 named the gap ("a hook, not a field") and
declined to take it there; this takes it.

1. **`oblisk.<capability>:on_change(fn)`.** The handler runs once per `StateSnapshot`, from
   `apply_state_snapshot` right after the value lands and before any layout pass, with
   `(current, previous)`. `previous` is `nil` on the first push and nothing else. It is not a `map`:
   it is not called during resolution, cannot be rolled back, and so may do what an input callback may
   do, `invoke`, `process.run`, write a `state` signal. Purity in the tree is untouched; side effects
   moved from "on input" to "on input or on push", which is where they already lived in the mirror.
2. **Every push, no filtering; the config finds its edge.** The engine hands over each snapshot and
   the one it replaced, and Lua compares them. A threshold ("low is 20%") is an opinion, and ADR-0113
   decision 15 already put opinions in the config. `util.battery_at_most(b, percent)` is the
   comparison the four battery edges share; `previous == nil` is the mirror's `initialized` guard.
3. **Each evaluation re-registers, so each evaluation first clears.** `shell.lua`'s module-level
   code registers the handlers, and an in-place reload re-runs it on the same VM (ADR-0044 decision
   4). Without a clear, one config save would double every notification. `clear_change_handlers` runs
   before both `evaluate_and_specs` calls. Known cost: an evaluation whose topology changed leaves the
   new config's handlers in the old generation until the swap, so a push in that window fires in
   both processes. Short, and a duplicate OSD line is the worst of it.
4. **A handler runs under a `map` callback's 5ms budget and cannot break the push.** `CpuBudget`
   wraps each call; a raise or an overrun is logged with the capability's name and the next handler
   still runs. The value is already in the signal by then, so the screen is right whatever the
   handler did.
5. **What it unblocked, all in `dev-config`.** `modules/global/power_events.lua` (new): the charger
   OSD off `oblisk.power`'s `on_battery` edge plus the mirror's 10/100 brightness step, the charge
   limit and fully charged OSD lines, the low and critical `notify-send`s, and `systemctl suspend`
   at 8%. `modules/bar/indicators/updates.lua`: `configure` moves from load time to
   `oblisk.system`'s first push, which is what carries `state.json`, so `checked_at` is finally read
   back and a restart inside the hour does not re-check (verified: a second start touched neither the
   file nor the network); the time of a successful check is written when it differs from the file's;
   and "Updates Available" fires for names not in the remembered `updates_notified` key, the
   mirror's `notifiedPackagesKey` (verified: six new packages announced once, not again on restart).
6. **The pill's thresholds moved to match.** The bar coloured at 30/15 while the notifications
   would have fired at 20/10. `util.battery_thresholds` is now the one table both read, at the
   mirror's values.
7. **UPower's zero-percent glitch is held in Rust.** Unrelated to the hook and found on the same
   review: UPower reports a spurious `Percentage` of 0 on mains for one push, and only
   BatteryService.qml, not Quickshell's C++ layer, guards it. `hold_through_glitch` keeps the previous
   percent when a zero arrives with the battery not draining after a non-zero reading; on battery a
   zero passes through, as it does there.

8. **The OSD is push-driven, and is the mirror's card.** `modules/osd/service.lua` replaces
   `ui_state.arm_osd`: the card used to be armed by the bar click that changed a level, so a volume
   key or a `wpctl` in a terminal showed nothing, and the bar button showed you your own click. Now
   `on_change` handlers on audio, brightness, network, bluetooth, notifications and keyboard call
   `osd.show(kind, entry)`, and `power_events.lua` does for the charger. Two layouts decided by
   whether the entry carries a `level`, `OSDCard.qml`'s slider and toggle rows at its sizes (80 tall,
   300 wide, a 48 tile, a 12 track). Not the mirror's queue: a card that arrives while a more
   important one is up is dropped, one as important or more replaces it, which is what its suppress
   list was for (the brightness step the charger edge triggers, under "charger connected"). Verified
   live: a terminal `wpctl` and a layout switch each raised the right card, centred, for two seconds.

Not mirrored: keyboard backlight on the charger edge (no capability), the `--wait -A` actionable
update notification (a config could read the action from `process.run`'s stdout callback; not worth
it until someone wants the button), the mirror's 15-second notification dedupe, which an edge does
not need, and three OSD kinds with no fact to read: the Wi-Fi radio toggle (`NetworkState` has no
`wifi_enabled`), microphone mute (`AudioDevice` has no `muted`) and screen recording.

## ADR-0116: Pointer drags and wheels on a button, and the microphone's volume

**Status**: Accepted (2026-09-04)

**Context**: Reviewing `Volume.qml`/`AudioPanel.qml` against the bar's volume pill found the mirror
is mostly a slider: drag the pill to set the volume, roll the wheel over it to step, and a panel of
four more sliders (output, microphone, one per stream) with device pickers. None of it could be
written. The pointer model (ADR-0050) hands a config a click's rect and button name and nothing
else; the wheel (ADR-0069) writes `scroll()` signals and reaches no handler. On the Supervisor side
`AudioState` carried the default sink's volume and mute and nothing of the default source's, a hole
§ 3.2 had noted beside `set_muted` since ADR-0053, and an `AudioDevice` had no way to say it is a
headset.

**Decision**:

1. **`button` takes `on_drag(rect, pointer, phase)`.** A left press on the innermost `button`
   declaring it holds the drag until the release or a `Leave`; every `Motion` while held calls the
   handler with `"move"`, the press with `"start"`, the release with `"end"`. `pointer` is `{ x, y }`
   in the button's own coordinates and unclamped, since every handler divides by the rect and the
   config's own `min`/`max` is the clamp; a drag past the end stays pinned at the end because the
   handler keeps hearing about it. Left only: a drag is one gesture and carries no button name, and
   the other two buttons stay free for a click on the same control, which is what the pill wants
   (middle mutes, right opens the panel). A press that focused a `textfield` drags nothing, as it
   clicks nothing (ADR-0092). The left `on_click` still fires on a release inside the rect, after the
   drag's `"end"`, so a control taking both sees its value committed first. Not a `slider` node: the
   same two hooks are a seek bar, a colour pad or a resize handle, and a kind per shape is what a
   general engine must not grow.
2. **`button` takes `on_wheel(rect, steps)`.** `steps` in notches, positive away from the user, the
   direction every volume and brightness control reads as "more" and the opposite of Wayland's axis
   sign; a touchpad swipe arrives as fractions of a notch through the same `wheel_delta` a scroll
   uses. Vertical axis only. Against a scrollable container the innermost of the two under the
   pointer wins and nothing chains, ADR-0069's rule extended to a second kind of taker. The
   "no `scroll()` registered" early-out is gone with it: the wheel handler now walks the tree
   unconditionally, since a button's handler is not in any registry, and a wheel event is rare
   beside a motion event.
3. **A button with either handler is solid to input** (`takes_input_as_a_box`), as one with
   `on_click` is: invisible by design and still has to be pressable.
4. **`AudioState` gains `source_volume` and `source_muted`, and three actions.** An `Audio/Source`
   node is bound exactly as an `Audio/Sink` is, `Props` param and `device.id`/`card.profile.device`
   route, since PipeWire gives it the same shape (this machine's mic is device 51 route 0 beside the
   speaker's route 7); `SinkEntry` became `DeviceEntry` and the write path takes a direction.
   `set_source_volume(vol)`, `set_source_muted(bool)`, `toggle_source_mute()` mirror their sink
   twins through one `set_default_volume`/`set_default_muted` pair. Verified live: the panel's
   microphone card read 15% off the source's own `Props`.
5. **`AudioDevice` gains `icon`**, the node's `device.icon-name` as PipeWire spells it, absent when
   the node carries none. A hint for a glyph (`AudioService.deviceIconFor`'s `headset`/`headphone`
   words), not an icon lookup this side performs.
6. **No headroom.** The mirror allows 150% with a marker at 100%; `set_volume` keeps its `[0, 1]`
   clamp. It complicates every meter for a feature few use, and can be lifted in one place.
7. **In `dev-config`**: `components/slider.lua` (a `button` with the two hooks over a `"NN%"`-wide
   fill; a held drag draws from a `pending` `state()` and commits once on release, `Slider.qml`'s
   `committed`; drags and notches quantise to `steps`, default 20, the mirror's 5%), the volume pill
   rebuilt on it with the mirror's bindings, and `panels/audio_panel.lua`: output and microphone
   cards with device pickers, and the application mixer. Verified live by hand on the pill and by
   screenshot on the panel.

**Consequences**: A config can build any drag-set control without an engine change. Snap-back:
between a drag's commit and the capability's next snapshot the fill reads the old value for a frame
or two; the PipeWire round trip is milliseconds and it has not been visible. Not built: a
`source_volume` OSD line (the OSD service could add one in three lines when wanted) and the mixer
stream's desktop-entry icon lookup beyond `oblisk.applications`' `app_id` heuristics.

## ADR-0117: A workspace knows whether it is empty and what runs on it

**Status**: Accepted (2026-09-04)

**Context**: Reviewing `WorkspaceStrip.qml` against the bar's strip. The mirror is an `ExpandingPill`
of full-size circles, collapsed to the focused workspace and widened on hover, each circle drawing
the app icon of what runs on that workspace, or its number when nothing does, and dimmed when
empty. § 2.9's `WorkspaceEntry` was `{ id, idx, name }`, which draws numbers and nothing else;
ADR-0056 had kept window lists out on purpose and the spec noted `Window.workspace_id` as the
additive path. The strip itself had been written as always-open dots, with a note that a pill
needed a collapse timer the engine lacks; the power menu (`f38051b`) since showed it does not,
because a `hover` region on the row answers containment and a pointer crossing the gap between two
circles never leaves the row.

**Decision**:

1. **`WorkspaceEntry` gains `populated: bool` and `app_id: string?`.** One window, not the list:
   the window that stands for the workspace is the focused one when focus is there, else the one
   with the lowest window id, since niri's map has no order and "first tile" is not on the wire. An
   empty `app_id` on the wire becomes an absent key, so `entry.app_id == nil` and "draw the number"
   are one test. The reduction stays compositor-neutral: `WorkspaceRow` carries the two fields and
   `workspaces::niri` fills them from `Window.workspace_id`.
2. **Still no per-workspace window list.** The roadmap row narrows to what it is now for: a window
   switcher. A strip has one circle per workspace and one icon fits in it.
3. **In `dev-config`**, `workspace_strip.lua` becomes the mirror's pill: `item_width` circles on the
   power menu's pattern, a `hover` on the row, every circle but the active one `visible` only while
   hovered, the focused ground accent, a populated one glass and an empty one `DISABLED` at
   `opacity.disabled`, an `icon` from `oblisk.applications` over the number when the `app_id` maps
   to a desktop entry. The old strip's reasons for small dots (twelve bordered circles too wide)
   are answered by the collapse, which is what the mirror answers them with.

**Consequences**: A third field a niri upgrade could rename (`workspace_id`), covered by the
adaptor's wire-JSON fixtures. A workspace whose only window has no `app_id` is populated with no
icon, drawn as its number at full strength, which is what the mirror does too. No width animation
and no opacity fade; the engine has neither.

## ADR-0118: `workspaces` speaks Hyprland, as a module behind the same publisher

**Status**: Accepted (2026-09-04)

**Context**: `oblisk.workspaces` was niri-only (ADR-0056 decision 1), and on a Hyprland session
printed "no implementor yet" and never pushed. ADR-0075 had already moved the reduction onto
compositor-neutral rows and named the line a second implementor would sit on: a sibling module plus
two match arms. The reference config's `Impl/Hyprland/WorkspaceImpl.qml` shows the whole of what
Hyprland needs: no state on its event socket, so re-read `hyprctl -j`'s three lists on every event;
the workspace number as the id; a `windows` count for populated; the activated toplevel's class.
`keyboard` already had a `HyprlandLink` over the same two sockets, built to the protocol without a
Hyprland machine to test on, which is the position this ADR is in too.

**Decision**:

1. **`workspaces::hyprland` is the second implementor, and there is still no trait.** It plugs
   into `StatePublisher` for reads and two exhaustive-match arms for writes, which is everything a
   trait would give two implementors, and ADR-0075 decision 4 tied the trait to a *live-tested*
   second compositor. This one is built to Hyprland's documented IPC with hand-written fixtures in
   `hyprctl -j`'s shape; replacing them with a capture is the first job on a Hyprland machine.
2. **The loop is re-read on trigger.** `.socket2.sock` is listened to on one blocking OS thread,
   like niri's reader; a line whose event name (before `>>`, `v2` suffix dropped) is in a `TRIGGERS`
   table causes `workspaces`, `monitors`, `clients` and `activewindow` to be read over
   `.socket.sock` as `j/<name>`, one connection per request, then reduced and published. No
   `hyprctl` subprocess, unlike `keyboard`'s link: the socket takes the same command and spawning
   four processes per window event is the wrong cost. A burst re-reads once per event and the
   publisher drops the equal results; coalescing waits for a measurement.
3. **The number is both `id` and `idx`; `name` only when it is not the number.** Hyprland has no
   per-monitor position, and the number is what a keybind and `dispatch workspace N` mean, so
   `focus(id)` keeps § 2.9's meaning and focusing a number with no workspace creates one. Workspaces
   with a non-positive id, specials and Hyprland's named ones, are dropped: neither fits a `u64` id
   or a number-keyed focus, and neither is modelled. A special showing on the focused monitor
   leaves that monitor's regular active workspace the focused row.
4. **Active is the monitor's `activeWorkspace`, focused is that of the monitor with `focused`,
   the focused window is `activewindow`.** The first two are niri's per-output/global split by
   another name. `activewindow` rather than `clients[].focusHistoryID == 0`, because the history
   still names the last toplevel while a layer surface holds focus and the reply is `{}` then.
   `app_id` is the class of the lowest `focusHistoryID` on the workspace, ADR-0117's "focused, else
   first" with a real order behind "first".
5. **`hyprland_socket_path` moves to `compositor.rs`**, the one thing beyond the probe both
   capabilities genuinely share; ADR-0075's "detection only" widens to "detection and where
   Hyprland's sockets are". Moving it found the names wrong: the link opened `socket2.sock` and
   `socket.sock`, and Hyprland's files are `.socket2.sock` and `.socket.sock`, so `keyboard`'s
   Hyprland layout reporting could never have connected. Fixed in both callers.

**Consequences**: A Hyprland session now pushes `oblisk.workspaces` and the shipped strip draws it
sparse, one circle per existing workspace, with numbers as labels. Padding empty slots to ten, the
optional `special` list, `is_fullscreen` when known and a session-level `compositor` field are the
next ADR, since they are payload and display policy, not the adaptor. Until a capture replaces the
fixtures, a Hyprland field rename is caught by the "reply did not parse" log line and nothing else.

## ADR-0119: What one compositor has and the other does not is an absent key

**Status**: Accepted (2026-09-04)

**Context**: With two implementors (ADR-0118) the payload met the first features one compositor has
and the other lacks: Hyprland's special workspaces and its fullscreen flag, niri's unbounded
workspace count against Hyprland's create-on-focus numbering. The reference config answers these
with capability flags on each backend (`supportsSpecialWorkspaces`, `fillsEmptyWorkspaceSlots`,
`hasOverview`) and a service layer that pads display slots to ten when the flag says so. § 2.9
already had a convention for a fact one compositor cannot state: `focused_workspace` is present only
where it is true, `is_fullscreen` was left out rather than fabricated (ADR-0056 decisions 4 and 5).

**Decision**:

1. **A feature the compositor lacks is a key the payload lacks.** No flag table. `special` is
   present on Hyprland, an empty list when none exist, and absent on niri, so `w.special == nil`
   is the "has scratchpads" test and `#w.special == 0` is "none right now". `active_client.
   is_fullscreen` is present when Hyprland says and absent on niri, which turns decision 5's
   omission into "absent means unknown" without fabricating anything. The same shape a config
   already reads `focused_workspace` by.
2. **`special` is a top-level list keyed by name.** `{ name, populated, app_id?, shown_on? }`:
   Hyprland addresses specials by name and gives them negative ids, so the name is the identity and
   `toggle_special(name)` takes it. `shown_on` is the output currently showing it, since a special
   is shown on one output at a time, and the payload's per-output structure is for what an output
   *has*; a special belongs to none. `name` is the compositor's full `special:term`, and the
   adaptor strips the prefix the dispatcher would double.
3. **The payload names its compositor.** `compositor: "niri" | "hyprland"`, because one policy is
   display, not state: a Hyprland strip pads empty slots to ten and a niri strip must not, and no
   key carries "focusing a missing number creates it". The adaptor still fabricates nothing; the
   padding is `workspace_strip.lua`'s, in Lua, keyed on this string, with the padded slot shaped as
   an entry (`{ id = n, idx = n, populated = false }`) so the button reads it as one and
   `focus(n)` is the click. niri's trailing empty workspace gives the same picture unpadded.
4. **`toggle_special` is the second action**, dispatched to Hyprland and logged on niri, where a
   config that checked `special` never calls it.
5. **In `dev-config`**, `special_workspaces.lua` is the mirror's `SpecialWorkspaces.qml`: a circle
   per special, accent while shown, the standing app's icon or the name's first two letters, the row
   absent when there are none. No tooltip: one popup per dynamic special is more `shell.lua` than
   two letters are worth.

Not built: an overview action (niri only, nothing asks) and urgency (both have it, nothing draws
it). Rejected: a `supports` table on the payload, because a config then has two things to check
where the key's presence already answers; and padding in the adaptor, because the payload lists
what exists and a strip's slot count is not the compositor's fact.

**Consequences**: A config written against niri sees one new string field and nothing else changes.
The Hyprland half is built to the documented IPC and not live-tested, as ADR-0118. § 2.9's "two
gaps" note is now one, the window list.


## 0120. A watched folder is a capability, `oblisk.files`

A config that wants a folder's contents asks the Supervisor to follow it: `files:watch(path,
extensions?)` lists it once and re-lists it after every settled burst of inotify events, and
`oblisk.files.folders[path]` is the result. The first caller is the wallpaper picker; the shape
is a folder watcher, not a wallpaper scanner.

1. **Supervisor-side, through inotify, not a Lua read.** ADR-0048 took `io` out of the config VM
   so no evaluation can block Wayland dispatch on a filesystem, and `process.run("ls")` would put
   a line parser in the config for a listing the Supervisor can hand over as a table. `watcher.rs`
   already follows the config directory the same way; this is that shape for a folder the config
   names.
2. **Keyed by the path the config wrote**, trailing slashes stripped, so `folders[folder]` reads
   back with the string that went in. `ready` is false until the first listing lands, `error`
   carries a listing failure in words, and both are on the folder rather than the capability,
   since two folders can be in two states.
3. **One level, files only, hidden skipped, sorted by name folded**, filtered to the extensions
   `watch` named so a `Downloads` folder does not ship five thousand entries per push. A
   subfolder is not listed and not descended; a picker that wants a tree has not been asked for.
4. **A repeat `watch` with the same filter re-pushes and starts nothing.** A generation swap
   re-evaluates the config, which calls `watch` again, and the new generation reads the snapshot
   it already has. A different filter replaces the watch, since the held listing was made under
   the old one. `unwatch` aborts the task and drops the key.
5. **`MODIFY` is not in the mask.** A copy in progress fires it per chunk; `CLOSE_WRITE` marks the
   end and the 200ms debounce folds a forty-file copy into one listing. `DELETE_SELF`/`MOVE_SELF`
   list once more (recording the error) and stop; watching the parent for the folder to reappear
   is the upgrade, unasked for.

Rejected: a `system:list_dir` returning through `system.state` (a listing is not user state, and
it would not follow changes); a per-file `stat` payload with size and permissions (the picker
reads name, path and mtime; the rest waits for a caller).

**Consequences**: `shared::Capability` gains `Files`, the stubs gain `FilesState`, `Folder` and
`FileEntry`, § 2.17 and two § 3.2 rows describe it. `applications` still scans on `refresh`
rather than watching, per ADR-0061 decision 4; nothing here changes that.

## 0121. A `panel` or `lock` may build its child per output

`child` on a `panel` or `lock` may be a function of the output's connector name. `Scene::apply`
calls it once per surface instance, per pass, and the node table it returns takes `child`'s
place before the walk begins. A `window` or `popup` has one instance wherever the compositor
places it and no output to hand over, so a function there is refused.

1. **Per instance, where the instance is known.** ADR-0038 decision 3 made one `monitor =
   "All"` declaration one surface per output, all resolving the same tree against their own
   `available`. Nothing in that tree could tell which output it was on, so a wallpaper that
   differs per screen had to be one `panel` per screen, declared from `oblisk.screens` at
   evaluation, and a monitor plugged in later got nothing until a reload. The function is called
   in `apply_one_instance`, the one place the instance id, the output and the tree meet.
2. **Per pass, on `list.itemfn`'s terms.** The function runs on every apply, and its return is
   reconciled by id and position like any child. A `state("wallpaper_" .. output)` inside it is
   registry-stable by name (ADR-0044), so the retained tree survives; the cost is the same
   rebuild-and-throw-away `itemfn` already pays (§ 5.2's "fast-reconciling virtual repeater").
3. **`nil` maps the instance empty**, the same as no `child`, so a function may decline an output.
4. **The eval-time probe calls it with `"PROBE"`.** `lua::nodes`' validation applies every surface
   once against a single fake output, and a function child is validated on that output's return.

Rejected: an `oblisk.output` signal resolved per instance (a signal is one value; resolution
reads it once per property with no instance in scope); a `child` table keyed by output name (a
function is the general form and a table is one line of Lua inside it).

**Consequences**: `lua-meta/surfaces.lua` types `child` as `Node|fun(output: string): Node?` on
`panel` and `lock`; § 6.1 and § 6.4 say so. `dev-config`'s wallpaper is one panel again, with
per-output source and fit, and its choice persists through `system:write_state`, which closes
ADR-0055 decision 2's "does not persist".

## 0122. Images decode to their box, and off the frame through the thumbnail cache when asked

Three changes to the Renderer's image path, for a grid of files where the old path was a second
of frozen shell and a gigabyte of textures.

1. **A raster is stored scaled down to cover its box, never up**, and the cache key carries the
   box in physical pixels for every file, where before it did for SVG alone (ADR-0054 decision
   4's key). A 4K file drawn as a 230px tile is a 230px texture; the same file drawn full-screen
   is a screen's worth. The `image` crate's `thumbnail` (a triangle filter) does the scale. The
   same rule for every `fit`: `contain` could go smaller, but one rule keeps one slot per box.
2. **`image.async = true` decodes on a pool and draws nothing until it lands.** The pool is
   `available_parallelism` capped at four threads, fed from one queue, answering on one channel.
   The main loop polls it once per turn (the same drain-then-act turn as ADR-0044 decision 2's
   dirty flag) and repaints; the texture is created at the start of the next paint, where
   `release_evicted` already runs, since only the Wayland thread holds the context (ADR-0039).
   The default stays inline, since a wallpaper's first frame must be whole for the candidate's
   presentation evidence (ADR-0003) and an icon's decode is microseconds. A landing changes no
   display list, since a list names the file and not the texture, so the landing names its files
   and only the surfaces whose last list draws one forget it: `DisplayList::draws_any_of` is what
   keeps the wallpaper from repainting for a tile.
3. **A pool decode goes through the freedesktop thumbnail cache.** For a box a spec size covers
   (`normal` 128, `large` 256, `x-large` 512, `xx-large` 1024 on the longest edge), a current
   thumbnail at `$XDG_CACHE_HOME/thumbnails/<size>/<md5 of the file URI>.png` is decoded instead
   of the file, current meaning its `Thumb::MTime` is the source's mtime and its `Thumb::URI`,
   if present, is this file. A file decoded in full leaves a thumbnail behind when it was larger
   than one, written the spec's way (temp file beside the final name, `0600`, directory `0700`,
   rename). Nautilus and every GTK file chooser keep the same cache, so a folder the file manager
   has shown opens with no full decode, and one the picker decoded shows in the file manager
   likewise. The URI is escaped as GLib escapes it, since GLib hashed what is already there.
4. **WebP decodes**, one feature flag on the `image` crate, because a wallpaper folder is full of it.

Not built: the spec's `fail/` directory (a file that does not decode is `Slot::Failed` for the
generation); `Thumb::Size`; the shared repository under `/usr/share/thumbnails`; a byte budget
for the cache (ADR-0054's entry count stands, and an entry is now at most its box); a crossfade
when an inline `source` changes (ADR-0002's transition branch, still waiting on an animation
model, so a wallpaper change is a stalled frame rather than a flash of the ground).

Rejected: thumbnails for inline decodes too (a tray pixmap in `/dev/shm` or a notification image
is not a user file, and would litter the cache); a separate `thumbnail = true` property beside
`async` (nothing wants one without the other, and QML's `asynchronous` is the one switch a
picker sets).

**Consequences**: `Draw::Image` and the cache key carry a box, `ImageCache::image` takes a `Load`,
`renderer/src/image/thumbnails.rs` is new, `md-5` and `png` are direct dependencies. § 5a gains
`async`; `CONTEXT.md`'s Image cache term says what the key is now.

## 0123. Idle textures have a byte budget, and the allocator's mmap threshold is pinned

Measured after ADR-0122, on the 1920x1200 laptop output, debug build: the Renderer booted at 89
MB resident and reached 132 MB after six wallpaper changes, with 123 MB of GPU memory charged to
the process by then (`drm-total-system0` in the DRM fd's `fdinfo`) and climbing 12 MB per change.
Two causes, two changes.

1. **A texture no mapped surface is showing is evicted once the idle total passes 16 MB.**
   ADR-0054's cache evicted by entry count alone, oldest insert first, so every wallpaper a user
   left behind stayed for the next 128 inserts: 12 MB each here, 33 MB on a 4K output, invisible
   to `ps` because a GEM buffer is not in the process's RSS and is system RAM all the same. Now
   each `Ready` slot carries its byte size and the tick of its last ask; after every paint
   `wayland::App` hands the cache the `(path, box)` pairs its surfaces' last display lists draw
   (`DisplayList::drawn_images`, the walk `draws_any_of` already does), and `ImageCache::trim`
   evicts the least recently asked-for unpinned textures until under budget. Pinned means shown:
   a texture some mapped surface last painted is never evicted, whatever the total, so a working
   set larger than the budget is over budget rather than thrashing through inline decodes. 16 MB
   holds a closed picker's tiles (54 files at a tile's size, 6.5 MB) and the last wallpaper on
   this output, and nothing older. Measured: the same six changes end at 79 MB of GPU memory
   instead of 123 and no longer climb. Icons are not pinned, since a list carries a theme name
   where the cache has a path; one is a few kilobytes and, evicted, one inline re-raster.
2. **`mallopt(M_MMAP_THRESHOLD, 1 MB)` at Renderer startup.** glibc serves an allocation over
   the threshold from its own mapping, returned to the kernel on free, and one under it from
   the heap, which shrinks only from the top. The threshold is dynamic by default: freeing a
   mapped 10 MB decode buffer raises it to 10 MB, so the next change's buffers (the decoded file,
   the cover-sized copy, the RGBA copy) land on the heap and stay resident after they are freed,
   trapped under whatever small allocation came after. The six changes grew the heap from 22 MB
   to 64 MB that way; with the threshold pinned it stays at 23 MB, and the process at 90 MB
   resident. The cost is one `mmap` per allocation over a megabyte, which nothing here does per
   frame. `libc` becomes a direct dependency for the one call.

Not changed: the wallpaper's texture size, which is already the cover of its output (2133x1200
here, 10 MB) and the least that draws sharp; the decode's transient peak, which is the whole
file at once (a 6024x3401 PNG is 82 MB of RGBA for the second it takes) because the `image`
crate decodes whole and downscales after, so a row-streaming decode that scales as it reads is
the next step if that peak matters; the Supervisor's allocator; ADR-0043's 50 MB per-monitor
figure, which this is the first measurement against.

**Consequences**: `Slot::Ready` carries bytes, `ImageCache::trim` and `DisplayList::drawn_images`
are new, `App::paint_surface` calls `trim` after recording its list, `main.rs` opens with the
`mallopt`. `CONTEXT.md`'s Image cache term says what is evicted when.

## 0124. A hidden subtree is frozen, the loop wakes on an fd, and an idle turn does nothing

Measured on the dev config, debug build, one 1920x1200 output, nothing open: the Renderer's main
thread used 8% of a core and woke 64 times a second. Two pushes a second reach it (`system`'s
clock tick every second, `sysinfo` every second and a half), and each cost a 45 ms re-resolve of
the whole scene, 37 ms of it in three surfaces that were closed: the wallpaper picker (17 ms,
fifty-four tiles), the panel host (11 ms) and the launcher (9 ms). The rest of the wakeups were
the 15 ms poll finding nothing to do, and on an open picker each of those turns copied the
focused surface's whole tree out of the scene to look for an `autofocus` field.

1. **A node that is not `visible` keeps its subtree frozen.** `prepare` stops at it: no child
   resolved, no signal read, no `list` item function called, no text measured. The retained
   children it had come through `finish` untouched, ids and last geometry included, so showing it
   again pairs the fresh children against them the way ADR-0001's reconciliation always did. Not
   retired: retiring would free the ids and rebuild from nothing on every open. Nothing outside
   `layout` ever read a hidden subtree's geometry (`taffy_style` already gave it `Display::None`;
   `paint`, `hit` and `overlay_input_regions` stop at a hidden node), and `hover_writes` still
   walks the frozen nodes, so a slot under a closed panel is written false the way it was. The
   three closed surfaces now cost 20 µs each; a push is a 5 ms re-resolve, all of it the bar.
2. **The loop blocks in `poll` with no timeout, on the connection fd and one eventfd.** The
   socket thread writes the eventfd after every frame it hands over (`wake::Waker`), a decode
   worker after every result, and a guard on the socket thread writes it when the thread ends
   however it ends, so a dead Supervisor is still read as `Disconnected` (ADR-0059) and not
   waited for forever. Nothing in the loop body keeps time: every check it makes reads state that
   only a Wayland event, a frame, a keystroke or a landed decode can change. Sixty-four wakeups a
   second become two, the pushes.
3. **An idle turn skips the focus housekeeping.** The three once-a-turn checks (a secure field
   whose surface went, a secure field that became typable under a held focus, an `autofocus`
   field to arm) run only on a turn that dispatched an event, drained a frame, took a keystroke
   or landed a decode. With decision 2 there are no idle turns left, but a Wayland event that
   changed nothing is still most turns, and `arm_autofocus_if_nothing_is_typing` copies a tree.
4. **The Supervisor's runtime has two worker threads.** `Runtime::new` gave it one per core,
   twenty here, for tasks that every one wait on a socket, a D-Bus signal, inotify or a timer;
   the blocking pool is separate. Twenty-six threads become eight.

Measured after: 1.3% of a core and two wakeups a second, the same debug build. What remains is
the bar's 4 ms per push, which a release build makes a fraction of a millisecond.

Not built: per-surface dirtiness (a push re-resolves every visible surface, and only the bar
reads what `sysinfo` pushes), which needs each surface to record the signals its resolve read
and the clock's fresh `map` on every resolve defeats until computeds compare values; a
row-streaming image decode (ADR-0123). Both wait on a measurement that says they matter, which
this one does not.

**Consequences**: `PreparedNode` carries `frozen`, `renderer/src/wake.rs` is new and
`main.rs` threads its `Waker` into `socket::spawn_client`, `wayland::run` and
`ImageCache::with_waker`; `wayland::run`'s poll has no timeout; `supervisor/src/main.rs` builds
its runtime by hand.

## 0125. A panel shown in the turn that created it waits for its first configure

The dev config's `osd` shows itself during boot, and roughly one boot in four died there: niri
answered the first buffer with `zwlr_layer_surface_v1: must ack the initial configure before
attaching buffer`, which takes the connection down, and the next `eglCreateWindowSurface` failed
with no EGL error for `khronos-egl` to report, so it panicked on its own `get_error().unwrap()`.
The Supervisor restarted the generation and the shell came up, which is why this read as a slow
boot rather than a crash.

`App::show_panel` had two paths and the wrong one ran. A panel hidden after being shown rebuilds
its `zwlr_layer_surface_v1` and waits in `MapState::AwaitingConfigure`; a panel that never showed
still holds the object `create_panel` made and went straight to `MapState::Mapped`, on the
reasoning that a surface committed at startup has long since been configured. True for a panel
shown by a click, and false for one shown in the same dispatch turn it was created in.

**Decision**: which state a kept layer surface goes to is read from `configured_size`, in
`map_state_for_kept_layer`. `bind_and_clear` is the only writer of a real one and runs on the ack,
`unbind` resets it, and no configure carries a zero on both axes, so `(0, 0)` is exactly "not
acked yet". Waiting costs nothing: the configure is already on its way and `bind_and_clear`
finishes the show when it lands. Fourteen consecutive boots clean afterwards, against three
crashes in the twelve logged before it; the `osd` now comes up at its configured 280x75 instead
of the 1x1 an unconfigured surface binds at.

**Not changed**: the panic itself. `khronos-egl` unwrapping an absent error is its bug, and the
only way to reach it is a connection already dead, which is not a state to keep running in.

**Consequences**: `map_state_for_kept_layer` in `wayland/layer.rs`, read by `show_panel`.

## 0126. The release build is the optimisation, and `target-cpu=native` is not

ADR-0124 left the bar's 4 ms re-resolve as the remaining cost and said a release build would make
it a fraction of a millisecond. Measured on the dev config, one 1920x1200 output, the same commit
built both ways:

| | debug | release |
| --- | --- | --- |
| launch to the bar's first frame | 799 ms | 198 ms |
| CPU to boot (Renderer) | 0.91 s | 0.12 s |
| CPU to open the picker, 54 tiles | 0.73 s | 0.13 s |
| idle, both processes | 1.6% of a core | 0.4% |
| Renderer RSS at rest | 82.7 MB | 69.4 MB |
| Supervisor RSS | 38.2 MB | 23.3 MB |
| binary, each | 238 MB | 7 MB |

Four to seven times on every axis of speed, and 28 MB across the two processes. Nothing in the
tree changed to get it.

**Measured and rejected**: `-C target-cpu=native`, built into its own `--target-dir` (hence
`/target-*` in `.gitignore`). Boot 198 ms against 198 ms, 12 ticks of boot CPU against 13, the
picker 11 against 14, memory identical -- inside the noise of a two-tick counter, for a binary
that only runs on the machine that built it. Also `malloc_trim(0)` after an eviction, for the
3.6 MB the picker leaves on the heap: it returned nothing, so that memory is fragmentation below
the top of the heap rather than free pages waiting to be handed back.

**Where the Renderer's 69 MB is**, by mapping, after a picker cycle: 19 MB heap, 19 MB
`libLLVM`, 15 MB `libgallium`, 7 MB the binary, 6 MB anonymous, the rest fonts and small
libraries. Half of it is Mesa's, and the shell's own share is the heap and the binary.

**Consequences**: none in the tree. The release profile was already tuned (`lto`,
`codegen-units = 1`, `panic = "abort"`, `strip`, `overflow-checks`); this is the measurement that
says to use it.

## 0127. The update check hands its pages back, and the rest of the memory is where it should be

ADR-0126 measured the release build and stopped at RSS. RSS is the wrong number to stop at: it
counts a shared page in full in every process that maps it, and half the Renderer's is Mesa's
`libLLVM` and `libgallium`, mapped by the compositor and every other GL client on the machine.
By PSS, which divides a shared page among its mappers, the release shell at rest is **33.7 MiB
Renderer plus 13.3 MiB Supervisor**, against RSS's 69 and 23 -- inside ADR-0043's 50
MiB-per-monitor budget rather than doubling it. Mesa's 33 MB of Renderer RSS is 6.8 MB of PSS.
Read PSS here; RSS is the number to quote at a stranger who wants to know how big the process
looks, not the number that says what the shell costs the machine.

**The one real find, and the change**: the hourly `updates` check runs `libalpm` against a
throwaway copy of the pacman database inside `spawn_blocking`, and parsing the whole sync set
costs about 52 MB. All of it is dead the moment the diff is built, and none of it came back:
the Supervisor sat at its 84 MB peak until tokio reaped the idle blocking thread ten-odd seconds
later, and settled 10 MB above where it started. glibc had it, not us -- a per-thread arena is
never trimmed on its own. One `malloc_trim(0)` at the end of the blocking closure, which is
`memory::return_free_pages_to_the_kernel`:

| after the check | before | after |
| --- | --- | --- |
| Supervisor RSS while the check runs | 84 MB | 84 MB |
| ... 3 s after it finishes | 84 MB | 32 MB |
| ... steady state | 33.4 MB | 31.9 MB |

The peak is `libalpm`'s and stays: it holds the parsed database while it diffs, and that is the
work. What goes is the plateau after it, which was the peak held for no reason at all.

**Measured and rejected**, all three on the Renderer:

- *A full Lua GC after every evaluation.* The whole dev config's VM is **675 KiB** and a
  `gc_collect` recovers 93 of them. The 16 MB heap is not Lua, so there was nothing to collect.
- *`M_ARENA_MAX = 2`*, to stop the eleven threads spreading slack across eleven arenas: 330 KiB
  of anonymous memory, 380 KiB of PSS. Noise, for a non-obvious allocator knob.
- *A smaller texture budget.* Not measured against, because the number it would trade against is
  ADR-0123's, and the picker's thumbnails are the thing it exists to keep.

**Where the rest of it is**, so the next person does not re-derive it: the wallpaper is **27.5 MB
of the Renderer's 41 MB of GPU memory** -- one fullscreen `Background` surface, its swapchain and
its texture, measured by booting the same config with the wallpaper declaration removed (13.9 MB
left). That is what a wallpaper costs, not a defect. The Renderer's 16 MB heap is femtovg,
cosmic-text and the retained scene, and no single allocation in it; the fonts are `mmap`ed by
`fontdb`, not on the heap (NotoColorEmoji alone is a 10 MB file and almost none of it is
resident). Neither is worth chasing without an allocation profiler, and this machine has none.

**Consequences**: `memory.rs` acts as well as reports now, which its module doc says. Every
future memory claim in this tree quotes PSS.

## 0128. The camera scan runs when a camera opens, not when PipeWire renames one

Cloudflare's DNS-cache write-up (`blog.cloudflare.com/dns-cache-memory-optimization-1111`) is mostly
about shrinking a struct that exists ten million times over, which is not a shape this tree has --
its per-entry techniques (`Box<[T]>` over `Vec<T>`, one list with offsets over three, boxing the
big enum variant) buy bytes per instance, and the instances here are counted in hundreds. What does
transfer is the first thing they did: they wrapped the allocator and measured, rather than guessing.

Done both ways here, on the Renderer:

- **A `GlobalAlloc` shim keeping a live-bytes histogram by size class.** The Renderer's live Rust
  allocation is **3.2-4.7 MiB**, against 16 MB of `[heap]` in `/proc`. The shell's own data is a
  fifth of its heap; the rest belongs to Mesa, LLVM and fontconfig, which allocate through the same
  `malloc` and answer to nobody here. That closes ADR-0127's open question, and it closes the
  Cloudflare-style question with it: there is no struct in this process worth 64 bytes a copy.
- **DHAT** (`valgrind --tool=dhat --trace-children=yes`), which needs `CPU_CAP` raised to survive
  emulation -- a 5 ms budget on an emulator that runs 20x slow refuses every `computed`, and the
  scene never applies. Worth knowing before the next person tries. Under it, evaluating the whole
  dev config allocates 2.36 MB with an 843 KiB peak, and the Renderer never reaches a steady frame
  in four minutes, so the GL side stays unprofiled by this route.

**What DHAT found, in the Supervisor**: `privacy::video::find_device_openers` was **58% of
everything the Supervisor allocates during a boot** -- 19.4 MiB of `opendir` buffers and 37,364
`readlink` calls, a `fuser`-equivalent walk of every process's every fd. The walk itself is right;
it is how you learn who holds `/dev/videoN` without a kernel interface for the question. What was
wrong is when it ran: the camera task rescanned on *every* arm of its `select!`, including the
PipeWire one, and a `Video/Source` node appearing says an app registered with PipeWire, not that
the set of processes holding the device changed. PipeWire's snapshot is used for one thing --
turning a pid into a name -- so it was spending an 11,000-syscall scan to relabel a string.

Split into `scan_camera_pids` (inotify's arm, and startup) and `name_camera_users` (both arms,
against the pid set that stands). Openers still come from inotify `OPEN`/`CLOSE` on the device
node, which is the only event that can change them, so nothing is detected later than before.

**Consequences**: the Supervisor's boot allocation drops by roughly the PipeWire-triggered scans,
which on this machine is the pair that fire as PipeWire enumerates its existing globals at startup.
Idle is unaffected -- it was never scanning at idle, and this ADR is not a claim that it was.

## 0129. Measured against the mirror and against Noctalia, and what their renderer has that this one does not

Two comparisons, both asked for and both worth writing down before the numbers rot.

**The mirror.** `~/.config/quickshell` is the QML config this tree's `dev-config` is written from, so
Quickshell running it is the closest thing to a like-for-like there is. Same machine, same
1920x1200 output, both shells at rest, PSS (ADR-0127's rule) and one 35.5 s window for idle CPU:

| | Quickshell + the QML config | oblisk + `dev-config` |
| --- | --- | --- |
| PSS | 169.0 MB, plus 9.8 MB of helpers | 11.0 MB Supervisor + 36.5 MB Renderer |
| RSS | 243.6 MB, plus 24.1 MB | 21.2 MB + 72.7 MB |
| GPU (DRM resident) | 214.2 MB | 51.5 MB |
| threads | 34, plus 6 | 8 + 11 |
| idle CPU | 4.65%, plus 1.30% for `cava` | 0.34% |
| helper processes at rest | `inotifywait`, `bluetoothctl`, `cava` | none |

Roughly a quarter of the memory, a quarter of the GPU, a fifteenth of the idle CPU. **Not
feature-identical**, and the gap is not all architecture: the QML config runs an audio visualiser,
which is real animation work nothing here does, and its `cava` is a process this shell has no
equivalent of. Both shells were up together during the CPU window, which taxes both. The honest
claim is the order of magnitude, not the digits.

**Noctalia** (`github.com/noctalia-dev/noctalia`) is no longer a Quickshell config: it is 11.8 MB of
C++ on Wayland and OpenGL ES with no Qt or GTK, configured in TOML. That makes it a peer of this
tree rather than of the mirror, and its `src/render/` is the first outside renderer worth reading
against ours. What it has:

- `GlSharedContext`: a root surfaceless `EGLContext` that is the share parent of every other
  context, so a texture uploaded in one is usable in all -- their lock screen reuses the wallpaper
  already in VRAM. **We have the stronger form by construction**: `wayland::egl::init` builds one
  context for the whole Renderer and every window surface is made current against it, so the glyph
  atlas and the image cache are shared without a share group, and the lock surface is in the same
  process. Their machinery exists because they run several renderers; the process boundary is where
  this tree splits instead (ADR-0006).
- `SharedTextureCache`: path-keyed and refcounted, decoded and uploaded once. Ours is keyed on path
  *and* box, pins what a mapped surface shows, and bounds the rest by bytes (ADR-0123). Refcounting
  answers "is anyone using this"; a byte budget answers "how much may sleep here", and the second is
  the question a shell with a wallpaper picker actually has.
- `CachedLayer`: an FBO plus a scratch FBO and a dirty flag, rendered through a callback and
  re-blitted while it is unchanged. Read at a distance this looked like a general subtree cache and
  ADR-0130's line-by-line pass corrected that: its only two callers are `blur_cache` and
  `backdrop_surface`, so it is the blur pipeline's scratch buffer and not a way to skip re-painting
  arbitrary subtrees.
- `blur_cache`, its one real caller: nothing here blurs, so neither has anything to cache. The pair
  is the shape to copy on the day `roadmap.md`'s backdrop-blur item lands, and nothing before then.
- A shader program per primitive (rect, glyph, image, gradient, spinner, ring) instead of a general
  canvas. Leaner per draw than femtovg's path pipeline, and every primitive is yours to write. No
  evidence femtovg is a bottleneck at 0.34%; not a rewrite this tree has earned.
- Context-loss handling (`resetNotificationEnabled`, `videoMemoryPurgeNotificationEnabled`,
  `abandonGpuResources`, `recreateRootContext`): the in-process answer to a GPU reset or a
  suspend-time VRAM purge. Ours is a generation that dies and a Supervisor that swaps a fresh one
  in, which is the same recovery without the bookkeeping.

**An idea neither shell appears to use**: real damage regions. Both post the whole surface every
paint, so a compositor re-composites a full-width bar for one clock digit.
`eglSwapBuffersWithDamageKHR` would narrow it. It spends the *compositor's* GPU, not ours, which is
why it has stayed unmeasured -- an upgrade path, not a finding.

**Consequences**: none in the tree. Nothing here says to change the renderer; two of the four ideas
we already have in a stronger form, and the other two would spend what this shell is short of.

## 0130. Noctalia read line by line: their animation model, and the two pieces of it this tree already has

**Status**: accepted

**Context**: ADR-0129 compared the two shells from the outside and skimmed four headers. This is
the pass that cloned the tree (`noctalia-dev/noctalia`, shallow, 307k lines of C++ across 1,837
files) and read the render, animation, scripting and reconcile layers against ours, prompted by
`roadmap.md` ranking the animation model as the largest unbuilt item. Two of ADR-0129's claims did
not survive the closer look and are corrected there.

**Decision**: take the frame-loop shape and the wall-clock rule. Take nothing else yet.

1. **Their animation system is 364 lines, and its core is a `float` setter closure.**
   `animate(from, to, durationMs, easing, setter, onComplete, owner)` pushes an entry onto a
   `std::vector`; `tick` walks it, interpolates, and calls each setter. Seven easings. No property
   system, no binding graph, no interpolation of anything but a scalar -- a colour fade or a slide
   is a scalar the setter spends. That is the whole model, and it is worth noticing how small the
   thing at the top of our roadmap is when someone else builds it.

2. **Progress comes from wall time, not from an accumulated delta.** `tick(deltaMs)` takes a delta
   and deliberately ignores it, computing `now - startedAt` instead. Their comment gives the
   reason: a Wayland compositor delivers `wl_surface.frame` sparsely right after a cold boot, so a
   delta-accumulated animation runs visibly slow exactly when the shell is being watched hardest.
   This is a correctness rule, not a preference, and it is the cheapest thing on this list to get
   wrong. Adopted as written.

3. **The frame loop stops when idle, and animating does not mean repainting.**
   `queueRenderIfNeeded` splits two cases: something is dirty, so render; or nothing is dirty but
   an animation is live, so `continueAnimationFrameLoop` commits *only* the frame-callback state
   and retains the current buffer. The callback chain stays alive with no pixels drawn. Combined
   with `hasActive()` gating whether the callback is re-armed at all, this is how a shell gets a
   vsync clock without paying for one at rest.

   This is the answer to the constraint `roadmap.md` already names -- "build the gate so a reason
   can be added rather than replacing the condition". Ours is stricter than theirs to begin with:
   `wayland::run` polls two fds with `PollTimeout::NONE` and has no time source anywhere in the
   loop (ADR-0124), which is why idle costs 0.34% of a core. `CompositorHandler::frame` is a stub
   in `wayland/output.rs`. So the animation clock is an addition to that loop and not a rewrite of
   it: arm a frame callback while an animation is live, let `poll` keep blocking when none is, and
   the idle number survives the feature.

4. **Their declarative layer cannot animate, and ours would not have that excuse.** This is the
   finding that matters most. `luau_host.cpp` and `ui_tree_reconciler.cpp` contain no reference to
   animation at all: every `animate()` caller is imperative C++ inside a control (`toggle.cpp`,
   `collapsible.cpp`, `button.cpp`) or a shell surface. A plugin author gets controls that happen
   to animate themselves and no way to animate anything else. They sidestepped the hard question --
   where an interpolated value lives when the tree that declared it is rebuilt -- by never letting
   the declarative layer ask it. In this tree the config *is* the declarative layer, so that
   sidestep is not available and the question has to be answered.

5. **We already own the mechanism their answer would need.** Their reconciler matches children by
   `(type, key)`, updates a match in place, and drops the subtree on a mismatch -- so a control's
   `m_animId` survives a declarative update precisely because the C++ object does.
   `Scene::apply`'s `pair_children_by_id_then_position` is the same mechanism and a stricter one:
   an explicit `id` pairs only against that `id`, id-less children fall back to position
   (ADR-0023, amended by ADR-0045), and an `id` appearing or vanishing is an honest change of
   identity rather than a silent reuse. We built it to key GPU resources on `NodeId`; it is also
   exactly the stable identity an animated value needs to be hung off. `CONTEXT.md`'s Lease, which
   has had no caller since it was written, is the other half.

**Rejected, with the measurement or the reading that rejects it**:

- **`mallopt(M_ARENA_MAX, 2)`**, which they set unconditionally in `main`. Tested here before
  reading their tree and it recovered 330 KiB, because the Renderer runs 11 threads and they run
  many more. The knob is right in principle and does not pay at our thread count.
- **jemalloc** (`background_thread:true,narenas:2,dirty_decay_ms:1000,muzzy_decay_ms:5000`), auto-on
  for their glibc builds. It is the systematic form of what ADR-0127 does with one `malloc_trim`
  call: a background thread returning pages on a decay schedule instead of one trim after one known
  spike. Declined for now on two grounds -- our measured problem was a single transient the one
  line already fixed, and a permanent background thread spends the idle CPU that is this shell's
  best number against every peer.

**What the read confirmed rather than changed**:

- **They call `malloc_trim(0)` too**, as a named `allocator_trim` startup phase after the last
  init phase, for the same reason ADR-0127 landed it after the update check: glibc keeps what a
  transient spike touched. Two trees arriving at the same one-line fix from separate measurements
  is the strongest evidence either has that the fix is the right one. (Theirs also covers startup;
  ours does not yet, and their placement is worth copying if boot ever shows a spike.)
- **Neither shell tracks damage.** Their `wl_surface_damage_buffer` calls are two one-pixel pokes
  in an output probe and a click shield; nothing in their render path narrows a commit. ADR-0129's
  note stands unchanged.

**Consequences**: nothing changes in the tree today. `roadmap.md`'s item 1 gains a decided shape --
scalar setters keyed on `NodeId`, wall-clock progress, a frame callback armed only while something
is live -- so the work starts from a design rather than from a survey. ADR-0129's `CachedLayer`
paragraph is corrected there: it is the blur pipeline's scratch buffer, its only callers being
`blur_cache` and `backdrop_surface`, not the general subtree cache a distant reading suggested.

**Not a claim about size**: their 11 MB binary and our 6.8 MB Renderer are not comparable numbers.
`meson.build` names 39 shared dependencies -- pango, cairo, glib/gobject/gio, harfbuzz, freetype,
librsvg, libjxl, libwebp, curl, libxml2, libical, polkit, pipewire, wireplumber, sdbus-c++ -- so
their text stack, image codecs and D-Bus layer live in `.so` files outside that 11 MB. Our Renderer
links ten shared objects, none of them a text stack, an image codec or a D-Bus library, and carries
all three inside its own 6.8 MB. Per-binary we are already smaller; the interesting comparison was
never the file size.

## 0131. What Noctalia has that is worth taking for memory, CPU and latency, measured

**Status**: accepted

**Context**: ADR-0130 read their render and animation layers and concluded "nothing changes in the
tree today", which answered the animation question and not the one that prompted the comparison.
This is the sweep for memory, CPU and latency technique specifically, with the numbers that decide
each item. Measurements are on this machine, `dev-config`, one 1920x1200 output, release build
unless stated.

**The measurement that reframes the animation work**: a temporary probe around
`RendererClient::re_resolve_if_dirty` puts one re-resolve at **median 1.38 ms, p95 3.38 ms, max
5.48 ms** (n=62, release; the same probe in debug reads 5.12/16.70/21.25). At the 1 Hz `system.time`
push that is 0.17% of a core, which is most of the 0.34% idle this tree quotes, and it is fine.

At 60 Hz it is not: 1.74 ms mean x 60 is **10.4% of a core spent re-resolving before anything is
painted**, and p95 alone is a fifth of a 16.7 ms frame budget -- measured with the wallpaper picker
*closed*. `lua/signal.rs` says why: `computed`/`map` recompute fresh on every `:get()`, with no
memoization and no dependency-invalidation graph, so any push re-runs every `computed` in the tree.

So the rule for ADR-0130's animation work is now a measured one rather than a matter of taste:
**an animation must interpolate on the retained tree and repaint, never by re-resolving per frame.**
Noctalia gets this for free -- a setter writes a float into a node and nothing reconciles -- and
this tree does not, because here the config *is* the declarative layer. Routing animation through
re-resolved Lua properties would spend a tenth of a core before drawing a pixel. Memoizing
`computed` is the alternative and a much larger change; it is not needed for animation if the
retained-tree rule holds, and it is worth revisiting only if a push cadence ever rises on its own.

**Taken, in the order they are worth doing**:

1. **An env-gated idle profiler in the poll loop.** Their `app/main_loop.cpp` reports, on an
   interval: loop iterations, CPU split process/thread/background, poll wakeups by cause
   (fd/timeout/immediate), per-source wake and dispatch counts with total and max dispatch time,
   and a spin detector that names a source which "keeps voting timeout=0". Every number this
   session cost a day of ad-hoc probes, scratch scripts and a DHAT run, and they read theirs off a
   log line. Ours has two fds and no self-measurement. This is the highest-value item here and the
   cheapest, and a spin detector becomes load-bearing the moment a frame callback can re-arm
   itself forever.
2. **`eglSwapInterval(0)`, on the day animation lands and not before.** EGL defaults to 1, which
   blocks `eglSwapBuffers` until the compositor releases the buffer; this Renderer is
   single-threaded, so a blocking swap stalls Wayland dispatch, Supervisor frames and input
   together. Probed today it costs nothing -- swaps measured 0.24, 0.30, 0.37, 0.44, 0.89 ms, five
   of them in 25 s, because a shell that paints this rarely never contends for a buffer. At 60 Hz
   it contends every frame. Their comment gives the same reasoning and pairs it with pacing from
   `wl_surface.frame`, which is ADR-0130's item 3.
3. **List virtualisation.** `ui/controls/virtual_grid_view.h` materialises a pool sized to the
   visible rows plus overscan and recycles tiles through `bindTile` as the data or scroll offset
   moves. `wallpaper_picker.lua` builds every row for every file in the folder, and each tile
   carries a `computed` that the 1 Hz push re-runs. Their header notes the adapter was shaped so a
   script-side "tile template" callback could drive it, which is the same API this tree would need.
4. **Alpha out of the text cache key.** They pack rgb into the top 24 bits and force alpha to
   `0xFF`, applying the caller's alpha at draw time through `u_opacity`, explicitly so an opacity
   animation on one string reuses one raster instead of allocating a fresh one per frame. A
   fade-out at 60 Hz with alpha in the key churns 60 rasters a second. Worth knowing before the
   first fade exists rather than after.
5. **A real LRU on the shape memo.** Theirs is doubly bounded (entry count *and* bytes) with one
   sharp detail: never evict the LRU front, or a single entry larger than the whole budget walks
   the list, evicts everything including itself, and returns a dangling pointer.
   `ShapingHandle::shape` clears the map wholesale at `SHAPE_CACHE_CAPACITY`, which its own comment
   already flags as the thing to replace if lists grow past 500 rows.

**Rejected, each with the reason**:

- **`mallopt(M_ARENA_MAX, 2)`** (they set it unconditionally in `main`): measured 330 KiB here.
  Right knob, wrong thread count -- the Renderer runs 11 threads and they run far more.
- **jemalloc with `background_thread:true,dirty_decay_ms:1000`**: the systematic form of ADR-0127's
  single `malloc_trim`. A permanent background thread spends the idle CPU that is this tree's best
  number against every peer, to solve a transient one line already solves.
- **Redundant-GL-state elimination**: they cache only blend mode, not program or texture binding,
  so there is no technique here to take.
- **A whole-run text raster cache**: theirs exists because Pango/Cairo rasterises on the CPU and
  uploads. femtovg keeps glyphs in a GPU atlas and a draw is quads, so the same cache would buy
  much less and cost a texture per unique string, size and colour.

**Where this tree is already ahead, so the sweep is not one-directional**: capabilities start
lazily on first config read, where their tray and polkit are merely staggered behind 500 ms and
1000 ms timers; the emoji font is mapped shared rather than read (49.7 MB to 22.7 MB private-dirty);
the image cache is byte-budgeted with pinning where theirs is refcounted; and
`pair_children_by_id_then_position` is a stricter reconcile than matching on `(type, key)`.

**Consequences**: no code changes in this commit. Items 1 and 3 are independently useful now; items
2 and 4 are prerequisites filed against ADR-0130's animation work, and item 5 is filed against the
500-row ceiling `text/shaping.rs` already names. The re-resolve figures are the baseline any of it
should be measured against.

## 0132. Checking ADR-0131's five items against the tree, and building the two that survived

**Status**: accepted

**Context**: ADR-0131 named five things worth taking from Noctalia and filed all five as future
work without touching code. This is the verification pass: each item read against what this tree
actually does, with the ones that survive built. One did not survive contact, and two measured out
smaller than the survey implied. Measurements are on this machine, `dev-config`, one 1920x1200
output, release build.

**Item 4, alpha out of the text cache key: refuted.** femtovg already does structurally what their
Cairo path needs a trick for. `femtovg-0.26.0/src/text.rs:88`'s `RenderedGlyphId` keys a rasterised
glyph on `glyph_index`, `font_id`, `size`, `line_width`, `render_mode`, `subpixel_location` and
`variation_hash`, and on nothing else: no colour, no alpha. A glyph rasterises once as coverage and
the `Paint` tints it at draw time, so a fade over a string reuses one atlas entry per glyph with no
work on our side. `ShapingHandle`'s own key is a measurement key with no colour in it either. The
item existed because their renderer rasterises full-colour surfaces on the CPU; ours does not, so
there is nothing here to take.

**Item 5, an LRU on the shape memo: real, and still not worth it.** `ShapingHandle::shape` does
clear wholesale at `SHAPE_CACHE_CAPACITY`, as ADR-0131 said. What that costs is one full re-shape
of the live working set, and the working set is about twenty text nodes; the clock is what fills
the map, at one dead entry a second, so the clear lands roughly hourly and costs on the order of
the 6.64 ms `ShapingHandle::shape`'s own comment measures for 500 rows. An LRU trades that for
recency bookkeeping on every hit, which is the path the memo exists to make cheap. The comment
already names the condition that would change this -- a working set genuinely larger than the cap
-- and it is not met. Left alone deliberately, not by omission.

**Item 3, list virtualisation: real, but the cheap half of it is worth a fifth of what the survey
implied.** ADR-0124 already froze hidden subtrees, so a closed picker costs nothing and only a
visible list is in question. A fixture of fifty tiles (a `column` per tile, each holding a `rect`
with an `image` and a `text`) re-resolves in 0.916 ms median. The same tree written as literal
children, which is what a perfect memo of `itemfn` would leave behind, re-resolves in 0.783 ms:
`itemfn` plus `deserialize_lua_table` is 19% of the pass, and `resolve_properties`, `LayoutStyle`,
taffy node creation and text measurement are the other 81%. Adding two derived signals per tile
takes the list to 1.153 ms and moves the split to roughly a third, since a `:map()` per tile
allocates on both sides of it. So the fix `node/spec.rs`'s ponytail note describes -- compute keys
first, skip `itemfn` for unchanged ones -- buys 20-30% of a visible list, not the bulk of it,
because the signals behind every property must be re-read each pass whatever happens to `itemfn`.
Viewport virtualisation is the item that takes the other 81%, by never resolving an off-screen row
at all, and it needs the scroll offset to reach `prepare`, which today it does not. Filed as the
real shape of this work; the memo alone is not worth building first.

**Item 2, `eglSwapInterval(0)`: built.** `wayland/egl.rs` never called it, so every surface carried
EGL's default of 1 and every `eglSwapBuffers` was free to wait on the compositor. This thread also
dispatches Wayland, drains Supervisor frames and services input, so that wait is not confined to
painting. Set once per surface in `bind_surface`, immediately after the `eglMakeCurrent` that first
makes it current, because `EGL_SWAP_INTERVAL` is state on the current context's draw surface. A
driver that refuses the hint logs and keeps the blocking default. Nothing paces the loop in its
place because nothing needs to yet: it paints only when `re_resolve_if_dirty` reports a change, so
the push is the pacing, and `wl_surface.frame` becomes the pacer when ADR-0130's animation work
gives it frames to run ahead of. Measured today it changes nothing, as ADR-0131 predicted.

**Item 1, an env-gated idle profiler: built**, as `wayland/idle_profile.rs` behind
`OBLISK_PROFILE_IDLE=<seconds>`. It reports, per window: loop turns; turns that woke and did
nothing; process and main-thread CPU as a percentage of one core, from `getrusage(RUSAGE_SELF)` and
`RUSAGE_THREAD`, whose gap is the shaping worker, the socket thread and tokio; wakes attributed to
the Wayland fd, the waker fd, both, or neither; and per-kind work counts. A `SPIN` marker is
appended when most turns of a busy window did nothing, which since ADR-0124 is the only shape a
runaway can take in a loop that polls without a timeout. Off costs two branch tests a turn and
touches no clock. `render` is a pure function of one window's counters so its thresholds are tested
rather than eyeballed in a log.

Its first run answered a question nobody had asked. Three windows on an idle bar:

```
idle 10.1s: turns=34 idle=11 cpu proc=1.37% main=1.30% | wake wl=10 wake=20 both=2 none=0 | work dispatch=2 resolve=23 type=0 decode=0 draw=0 paint=23
idle 10.4s: turns=18 idle=0  cpu proc=0.24% main=0.23% | wake wl=0  wake=18 both=0 none=0 | work dispatch=0 resolve=18 type=0 decode=0 draw=0 paint=18
idle 10.0s: turns=18 idle=1  cpu proc=0.25% main=0.24% | wake wl=0  wake=18 both=0 none=0 | work dispatch=0 resolve=17 type=0 decode=0 draw=0 paint=17
```

Steady state is 18 turns per 10 s, not the 10 a 1 Hz clock would explain, and every one of them is a
waker wake that re-resolves and repaints. The arithmetic is exact rather than mysterious:
`system_info.lua` configures `cpu_interval = 2` and `ram_interval = 5`, so 10 clock plus 5 CPU plus
2 RAM is 17 pushes per window against `resolve=17`. No spin, no bug -- but it makes the cost
visible, because each of those 17 re-resolves the whole tree at ADR-0131's 1.38 ms median, and a
CPU-percentage push re-resolves the clock, the battery and the workspaces along with it. That is
`lua/signal.rs`'s missing dependency graph seen from the other end, and it corroborates ADR-0131's
rule for animation from a direction that did not assume it. Idle CPU reads 0.24-0.25% of a core,
consistent with the 0.34% measured by other means in ADR-0129.

**Decision**: build items 1 and 2. Drop item 4 as already satisfied by femtovg. Leave item 5 where
its own comment leaves it. Re-file item 3 as viewport virtualisation with the scroll offset reaching
`prepare`, not as an `itemfn` memo, and record the 19/81 split as the reason.

**Consequences**: `nix` gains the `resource` feature for `getrusage`, read only when the profile is
on. The profiler is the instrument the next performance question gets answered with instead of a
temporary `eprintln!`, and its per-window counters are the baseline for ADR-0130's animation work:
turns should rise to the frame rate while `idle` stays at zero, and any `SPIN` line means the frame
callback is re-arming without work to do. Live-verified: the bar renders under non-blocking swap
with a correct clock.

## ADR-0133: `oblisk.battery` reads UPower uncached, because its wake-up races zbus's cache

**Status**: accepted

**Context**: the battery glyph showed `Discharging` with the charger physically connected, while
`upower -i` on the same device already read `pending-charge`. It then corrected itself minutes
later, with no cable event in between.

The first explanation offered for this was hardware: EC charge qualification, the fuel gauge's ADC
sample window, and `drivers/acpi/battery.c`'s `cache_time` (which is indeed `1000` on this machine).
That explanation reads the Lua and the controller correctly and is wrong about the cause. It
predicts a *slow* reading. What we had was a *stale* one, and the difference is the whole bug.

`battery::controller` woke on a `zbus::fdo::PropertiesProxy` signal stream and then re-read the
payload off `DisplayDeviceProxy`. zbus caches proxy properties by default (`CacheProperties::Lazily`
-- see `zbus-5.19.0/src/proxy/builder.rs`), and refreshes that cache from a task of its own
listening to the very same `PropertiesChanged`. Two independent consumers, one broadcast message,
and no ordering between them: when our stream won the race, `read_state` read the cache as it stood
*before* the change and returned the old state.

The lost push is the part that makes it last. `run_battery_task` only pushes when
`current != previous`, so a stale re-read compares equal and pushes *nothing*. The reading then
stands until the next `PropertiesChanged` drags it along one change behind. On a battery parked at
its charge limit there may not be one for minutes: a 40-second `dbus-monitor` capture of every
UPower `PropertiesChanged` on this machine, at `pending-charge` and 0 W, caught zero signals.

`power::controller` never had this, and the asymmetry is the proof. It wakes on
`receive_on_battery_changed()`, and zbus drives a `PropertyStream` off an `EventListener` on the
cache entry itself (`proxy/mod.rs:225`), so by the time that stream yields, the cache already holds
the new value -- ordered by construction. That is exactly why the charger OSD in
`modules/global/power_events.lua` was instant and correct while the bar's own glyph sat behind it,
and why the symptom looked like two different subsystems disagreeing about the same cable.

`mpris/proxies.rs:48` already carries a note about this caching hazard for `Position`, so the tree
knew the shape of it in one place and not the other.

**Decision**: build the `DisplayDeviceProxy` with `CacheProperties::No`. Each `read_state` is then a
real `Get`, which is what the function's own doc comment always claimed it was. Five round trips on
an event that fires a few times an hour is the cheap side of this trade, and it keeps the single
whole-object subscription the wake-up was written around rather than splitting into five property
streams the way `power::controller` did.

The alternative -- wake on `receive_state_changed()` and friends -- is also correct and is the
in-tree precedent, but it trades one subscription for five and gives up the property that
`org.freedesktop.DBus.Properties` batches a device's simultaneous changes into one message.

**Consequences**: verified live by physically unplugging and reconnecting the adapter; the glyph now
tracks the cable, and the pill reads `69%` against UPower's `Percentage=69, State=5`.

Everything the hardware explanation said about *latency* remains true and unmeasured -- once the
state does change, some of the delay to the fuel gauge is real. It was simply never the reason the
icon was wrong.

One sibling is unfixed and deliberately left so. `tray/registry.rs`'s
`spawn_item_signal_forwarder` wakes on StatusNotifierItem's custom `NewTitle`/`NewIcon`/`NewStatus`
signals and then re-reads a cached `StatusNotifierItemProxy` whose properties are declared plain
`#[zbus(property)]`. That is the same race in a worse form: an SNI implementation that emits only
the custom signal and no `PropertiesChanged` would never refresh the cache at all, and the item's
title and icon would be frozen at their first read. Unconfirmed against a real tray application, so
it is recorded here rather than fixed blind.

## ADR-0134: `oblisk.updates` is a schedule with a package manager behind a trait, and says which one

**Status.** Accepted.

**Context.** `updates` was written against `pacman` and never pretended otherwise. `UpdatesController::new`
took `/etc/pacman.conf` and `/var/lib/pacman` as arguments; `run_install` hardcoded
`pkexec pacman -Syu --noconfirm`; `run_one_check` called `check_against_a_throwaway_copy`, which
symlinks `local/` and syncs through `libalpm`; `needs_reboot` matched `linux` and `linux-*`. That is
four different places knowing which distribution this is, none of them next to each other, in a
capability whose Lua-facing surface -- `check`, `configure`, `install`, a count and a package list --
has nothing distribution-specific in it at all.

The second problem was what a machine without `pacman` was told. Nothing detected one. `Capability::Updates`
built the controller unconditionally; the first scheduled check then failed inside `link_local_db`
with `/var/lib/pacman/local is not a directory`, and that sentence -- a path error, phrased as if
something were broken -- was the whole of the answer. A config could not distinguish it from a
mirror being down. `UpdatesState` had no field for "this machine has no package manager", the way
`BatteryState` has `present`.

That gap reached the bar. `dev-config`'s `updates.lua` hid itself whenever its state read `idle`,
and `idle` covered three different situations: no updates pending, no check ever run, and no
package manager at all. So did the argument written into the file for hiding it, which was sound as
far as it went -- a permanent circle whose one meaning is "no action available" is a control that
never does anything, and its idle click was a no-op that proved it.

**The mirror.** `~/.config/quickshell` gates the whole module on the answer to this question and has
from the start. `Services/MainService.qml` runs one shell probe at startup whose first line is
`isArchBased "$(yn command -v pacman)"`. `UpdateService.ready` is `MainService.isArchBased &&
_checkUpdatesAvailable && Settings.isStateLoaded`, where the middle term is a second probe,
`command -v checkupdates`. `Modules/Bar/LeftSide.qml` then wraps `ArchChecker` in a
`Loader { active: UpdateService.ready }`, which is the same shape it uses for
`BatteryService.isLaptopBattery` -- a module that does not exist on a machine it does not apply to.
`ArchChecker.qml` itself carries no such test, because by the time it is instantiated the question
is settled.

Two things follow from reading it. The detection is a binary probe, not `/etc/os-release` parsing --
`ID=arch` was never consulted, because what the code needs is the command, not the distribution's
name. And once the indicator is unconditionally present whenever the manager exists, its idle click
has somewhere to go: `ArchChecker.qml`'s `onClicked` falls through to `UpdateService.doPoll()` when
nothing is pending. That is the half this tree was missing, and the reason its own comment gave for
hiding the button ("the idle click was already a no-op") was true only because the re-check had
never been wired to it.

**Decision.**

1. **`backend::Backend`, a trait with five methods**: `name`, `check`, `install_command`,
   `parse_install_step`, `needs_reboot`. Everything a package manager knows and the scheduler does
   not. `controller.rs` holds `Option<Arc<dyn Backend>>` and no longer contains the strings
   `pacman`, `alpm`, or `pkexec` outside prose.

2. **`backend::detect()` picks the implementation, once, at capability start.** `command -v pacman`
   without the shell: a walk of `PATH` looking for an executable file, which is the same question
   the mirror asks and costs no subprocess on the path to the first frame. Ordered, so a second
   entry is a line rather than a redesign.

3. **`pacman/` is one backend, not the capability.** `check.rs`, `install.rs` and `pacman_conf.rs`
   move under it (the last renamed `conf.rs`, since its parent now says which conf), and
   `check_against_a_throwaway_copy` and `link_local_db` move out of `controller.rs` into it. The
   `libalpm` arena release (`memory::return_free_pages_to_the_kernel`) goes with them: it is a fact
   about `libalpm`'s allocation, not about checking for updates.

4. **`UpdatesState.package_manager: Option<String>`**, the name of the command, `nil` when this
   Supervisor speaks none of what is installed. A name rather than a boolean, because a config that
   wants to say "pacman" in a panel header now can, and `nil` is a better "not here" than `false`.

5. **The controller pushes once at construction**, which no other capability's does. On a machine
   with no manager there is no later event to carry the answer -- the scheduler is not even spawned --
   so a config would wait forever to be told it should not be drawing. `last_snapshots` seeds a
   promoted PBA candidate, so the one push survives a reload rather than needing a resend.

6. **Every action refuses with no backend.** `check_now` and `install` log and return;
   `configure` returns silently, because a config naming an interval on a machine with no manager
   has done nothing wrong and does not need a line per reload.

7. **`updates.lua` is present whenever `package_manager` is**, and its idle click re-checks --
   `ArchChecker.qml`'s own fallthrough. It also finally draws `checking`: `UpdatesState.checking`
   has existed since ADR-0034 and `update_panel.lua` has read it all along; the indicator was the
   one place still treating a check in flight and a check never run as the same thing.

**Rejected.** *An enum over backends rather than a trait.* One variant today, and the dispatch would
sit in `controller.rs` -- which is the file whose whole point here is not knowing. *Parsing
`/etc/os-release`.* It answers a different question: `ID_LIKE=arch` on a derivative says nothing
about whether `pacman` is the binary in `PATH`, and a container or a chroot can be Arch with no
package manager reachable. The mirror never consulted it either. *Writing `apt` and `dnf` backends
now.* Neither is testable on this machine, and a backend written blind against a package manager
nobody here runs is a guess with tests that only assert the guess.

**Consequences.** The updates indicator is now on the bar at all times on this machine, dim while
there is nothing pending and accent once there is, and clicking it while idle runs a real check --
which is the one interaction the mirror had that this did not. On a machine with no `pacman`, the
capability starts, says `package_manager = nil`, spawns no scheduler, and the indicator is absent:
the same outcome as before, reached deliberately and explained, rather than through a failed check
reporting a missing directory.

The trait is one implementation wide, and that is the honest state of it. What it buys today is not
`apt` -- it is that the four places that knew about `pacman` are now one file, and that the
capability can answer "not on this machine" as a fact rather than as an error.
