# Decisions

Historical decisions, not current API documentation. Entries may describe proposals, deferred work
or behavior superseded later. Current contracts live in [API](oblisk-idl-api-specs.md) and
[services](oblisk-supervisor-services-dbus.md); open work lives in [roadmap](roadmap.md).

Entry and internal decision numbers are permanent because code cites both. Keep the choice,
constraints, rejected alternatives and amendments when shortening an entry. Add new decisions
sequentially; do not rewrite an old decision to match later implementation.

Early phase numbers and spec section references belong to the historical documents.

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

ADR-0143 supersedes the retained-subtree lease bag and child-first teardown contract.

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

Promotion is atomic per candidate. Evidence is collected per surface, but every expected surface
must report within one shared timeout before any swap occurs. Otherwise the candidate is aborted.
Partial promotion was rejected because aborting after two of three outputs had transferred would
black out those two outputs. This implements ADR-0019 item 5 only partially, not ADR-0003's
independent output timing.

The handshake stages null buffers, sends nonce-bound `ActivateDraw`, then collects
`wp_presentation_feedback`. Wrong generation, type or nonce is dropped. The caller sends input
deselection, candidate promotion and reap in that order because the candidate link owns only one
connection. Timeouts were 2 seconds for readiness, 3 seconds for evidence and 100 ms reap grace.

Rejected: hand-written presentation dispatch. SCTK 0.21.1 already supplied it; the Renderer only
needed to correlate feedback. Discarded feedback falls through to the evidence timeout.

Not built in this pass: scene-to-GPU rendering, arbitrary declared surfaces, real input/promotion
effects, concurrent handshakes, immediate failure on discarded feedback, installed binary lookup,
or NetworkManager/BlueZ hydration. The proof still used fixed surfaces and PipeWire or empty state.

Closes ADR-0019 items 1, 3, 6 and 7; item 5 remains partial and item 4 remains open.

## 0026. `process.run`'s Lua binding and piped stream registry ship; `textfield`/PAM stay deferred

Phase 15 implements the Lua process binding and non-blocking stdout/stderr piping, closing
ADR-0018 items 1–2. Textfield and PAM stay deferred. The claimed missing text-input dependency
was corrected by ADR-0027; the scene node and PAM design were still missing.

Commands reuse `CommandEnvelope` with capability `process` and actions `run`/`kill`.
The Renderer assigns monotonic IDs so it can return a handle before a socket round trip.
At this stage the loader and dispatch loop shared a thread and used an in-thread queue.

The Supervisor tracks children by generation and command ID in its event loop, without a
spawn-registry mutex. Piped spawning is separate from inherited-stdio spawning because Renderer
generations need the latter. Missing or already-exited kill targets are no-ops. Superseding a
generation reaps its children without sending exit events to its closed connection.

Callbacks are `out_cb(line, stream)`, with `stdout`/`stderr`, and `exit_cb(code)`, with an
integer or nil. Output and exit use separate wire events.

Review fixes: EOF is not process exit. Waiting inline can wedge the Supervisor, so removal is
synchronous and waiting/reporting is detached. Malformed runs and failed kills report nil exit
codes so Renderer callback registrations do not leak.

Rejected: the broader claim that the codebase has no shared mutexes. The socket generation
registry already uses one; only spawn tracking avoids it.

Not automated: the real three-process Supervisor/Renderer/child workflow over a socket.

## 0027. Textfield wire shape: secure submit frame and text-input bridge

This entry proposed the text-input wire shape, not a completed implementation. Ordinary fields
were designed around `zwp_text_input_v3`; masked secure fields use `wl_keyboard` directly.

1. **Correction to ADR-0026.** `wayland-protocols` already exposes `text_input::zv3` through its
   unstable feature. The missing pieces were the service and scene node, not a dependency.
2. **Seat binding.** One SCTK-bound `wl_seat`, no multi-seat support.
3. **Ordinary submission.** The original proposal named a text-input `ACTION_SUBMIT` event
   instead of a keyboard listener, intending IME-correct submission. This is the historical
   proposal, not a claim that the protocol or current implementation supplies that event.
4. **Secret wire shape.** A separate `RendererFrame::SecureSubmit` carries generation, capability,
   action and secret bytes. Generic JSON arguments were rejected because they would retain a
   plaintext copy outside `SecureBuffer` zeroization. Zeroize the source after building the frame.
5. **Cross-thread bridge.** The proposed edit diff carried commit/preedit text, delete lengths and
   a submit flag over the existing Wayland-to-socket channel pattern.

Amendment: secure fields bypass text-input entirely. Without a compositor-side IME, no
`commit_string` arrives; passwords must also stay out of IME candidate text. Their values remain
unreadable from Lua and their submit callback is argument-free.

Not built in this pass: `TextInputService`, the textfield scene node, seat binding, secure frame
dispatch or the cross-thread channel pair.

## 0028. PAM: nonstick, a re-exec worker subprocess, one-shot protocol

PAM runs in a re-executed worker, using one password captured before spawning.

1. **Crate: `nonstick`, not `pam-client`.** At selection time, `pam-client`'s last release was
   July 2022; `nonstick` supplied a maintained, programmatic conversation API without terminal I/O.
   Its `OsString` and PAM's C copies cannot be zeroized by the source buffer; construct that copy
   at the last moment and zeroize the source. The observed machine lacked `polkit-1` PAM config,
   so the chosen fallback service was `login`.
2. **Isolation: re-exec, not fork or a third binary.** PAM modules cannot reliably be cancelled
   without terminating their process. Forking the multithreaded Supervisor risks inheriting
   permanently locked mutexes. Re-exec the Supervisor with `OBLISK_PAM_WORKER=1`, branching before
   D-Bus/tokio/audio setup, and reuse process-group spawn/reap helpers.
3. **Protocol: one-shot, not interactive.** Write the captured password once to stdin and close
   the pipe. Every PAM prompt receives that same value. A worker-specific outcome frame on stdout
   distinguishes Success, StartFailed, AuthFailed, MaxTries, PamError and OtherError.
   Interactive prompt relaying and Supervisor/Renderer frame reuse were rejected as unnecessary
   and belonging to a different protocol, respectively.

Still open in this design pass: parse Polkit identities into the uid/Identity required by
`authentication_agent_response2`.

Not built in this pass: worker entry branch, PAM conversation, one-shot framing or identity parsing.
Framing was to reuse the shared JSON-frame helpers.

## 0029. NetworkManager: capability-tagged state snapshot and secure connect flow

NetworkManager introduced capability-tagged snapshots and a separate secure credential path.

1. **Capability tagging.** Add a capability name to snapshots, track revisions per capability,
   and hydrate the corresponding Lua signal. Payloads remain generic JSON until typed payloads
   are needed. Closes ADR-0022 item 1.
2. **D-Bus access.** Choose `rusty_network_manager` over hand-written proxies because it covered
   the required interfaces with a compatible zbus dependency, following ADR-0013.
3. **Listener architecture.** Merge D-Bus event streams into the existing async event loop.
   Unlike PipeWire, no dedicated callback thread is needed. Spawn scan/connect/forget writes
   rather than awaiting them inline, so a hung remote call cannot wedge the Supervisor.
4. **Password via secure submit.** Reject the IDL's plaintext password argument. A normal
   `connect(ssid, hidden)` command stores one pending intent, followed by native secret submission.
   The design used one flow for scanned and hidden networks: an empty secret means open;
   a nonempty secret populates WPA-PSK. It therefore required a password field even for open
   networks, because hidden networks do not advertise security flags.
5. **Ethernet toggle.** Disconnect wired devices when disabled; activate existing autoconnect
   profiles when enabled. A missing profile is a no-op. Software cannot fabricate link carrier.
6. **Push cadence.** No debounce. Rebuild on each relevant event; add coalescing only if measured
   scan bursts justify it.

Still open in this pass: the exact state struct, forgetting every matching saved profile rather
than only the first, and scan options, empty by default.

## 0030. BlueZ controller: hand-written proxies, Just-Works-only pairing, deferred codec control

BlueZ uses hand-written zbus proxies, Just-Works-only pairing and deferred audio codec control.

1. **Proxy choice.** Reject `bluer` because it brings a second D-Bus stack. The zbus alternatives
   reviewed were unmaintained or unreviewed. Hand-write Adapter1, Device1, Battery1, Agent1,
   AgentManager1 and ObjectManager bindings.
2. **Pairing policy.** Register a default `NoInputNoOutput` agent at construction. Reject PIN code,
   passkey requests and PIN display; legacy PIN-only devices cannot pair. Confirmation, passkey
   display and authorization auto-accept because no confirmation UI exists. Cancel/release are
   no-ops. This does not provide a PIN/passkey UI.
3. **Codec control deferred.** Switching needs PipeWire device profiles, `SPA_PARAM_Profile`,
   not the spec's proposed route parameter. The audio thread lacked an inbound command channel;
   that channel must be designed under audio ownership, not Bluetooth.
4. **Device tracking.** An object-path-keyed registry follows ObjectManager additions/removals
   and independent Battery1 changes. Hydrate once, then keep a property forwarder per device,
   aborting it on removal. A scan-and-replace list cannot represent those lifetimes.
5. **Category from Class, not Icon.** BlueZ Icon can be empty. Map computers, phones, headsets,
   headphones and keyboard/mouse peripherals from major/minor class bits; keyboard/mouse combos
   use keyboard. Unknown classes and other audio/video devices stay generic rather than guessed.
6. **No debounce.** Human-scale connection and battery events do not justify it.
7. **Discovery list lifetime.** Clear on start, preserve on stop. No cap for short, human-driven
   discovery sessions.
8. **One adapter.** Use the first found; the config model has no adapter selector.

Reuse ADR-0029's capability snapshot plumbing.

Not built: codec selection, PIN/passkey UI or multiple adapters. Codec selection shares audio's
missing inbound-channel prerequisite with app volume/mute controls.

## 0031. Tray controller: hand-written SNI/DBusMenu host, IconName preference, no cache-busting

The tray uses hand-written SNI/DBusMenu bindings, preferring icon names over pixmap decoding.

1. **Proxy choice.** Reject the reviewed `system-tray` crate: coupled Watcher/Host registration,
   a verified pixmap height-index bug, and raw image bytes that still require our validation and
   encoding. Hand-write the small interface and recursive menu bindings.
2. **Watcher/Host registration.** Request the watcher name without replacement or DoNotQueue.
   Treat NameTaken as success and register our Host against the existing owner, allowing both
   standalone operation and coexistence with a desktop environment.
3. **Registry identity.** Resolve the caller's service to a D-Bus unique name before using it as
   a key or spool filename. Raw service strings would permit filename injection/path traversal.
   The historical spool path was `/dev/shm/oblisk-$UID/tray/{sanitized_unique_name}.png`.
4. **Icon preference.** Pass IconName to the Renderer; decode only as fallback. Choose the largest
   pixmap up to 128 px, with no minimum size, because Lua owns display size.
5. **Menus.** Fetch the full layout at registration and on LayoutUpdated. Refresh lazy submenu
   contents through AboutToShow before rendering, avoiding both first-open latency and empty menus.
6. **Click semantics.** Enforce item-is-menu gating centrally: Activate no-ops for menu-only items.
   Menu selection sends DBusMenu's clicked event.
7. **Deferred actions.** SecondaryActivate, ContextMenu and Scroll were not built in this pass
   because no known consumer needed them; modern items supplied a Menu instead.
8. **PNG encoding.** Choose the encode-only `png` crate over `image`'s unused decoding machinery.

Reuse ADR-0029's capability snapshot plumbing.

Amendment, ADR-0054: the spool still overwrites in place, but the Renderer keys textures by path,
modification time and length. That fixes stale icons without changing this entry's no-spool-suffix
decision.

## 0032. Idle capability splits transport but keeps one controller

One Supervisor controller owns both Wayland idle notification and logind inhibition.

Share one idle-notify listener per distinct duration and fan out to registrations. Reload cleanup
reuses generation-scoped reset rather than adding unregister. Use get_idle_notification; no caller
needs presence-sensor exclusion. Deliver idled/resumed through a dedicated IdleEvent frame because
these are edges, not revisioned state.

Use logind `Inhibit(what="idle", who="oblisk", why=reason, mode="block")` on the existing system
bus. The fd lifetime releases the hold even after a crash. It covers automatic idle actions,
not explicit sleep, shutdown or lid-switch. Refcount generation holds so callers cannot cancel
one another; reset clears the count.

Rejected: Wayland idle-inhibit. It would split ownership and requires a surface the Supervisor
does not own.

Enable wayland-protocols' staging feature for idle-notify. Missing protocol or a failed dedicated
Wayland connection yields a logged, inert notifier. Inhibition still uses the required system bus;
individual inhibit requests may fail.

## 0033. Notifications advertises a real capability set, with Lua-configured sound and DND

Advertise the implemented notification capabilities, sanitize body spans, and let Lua configure
per-urgency sound and DND.

The advertised set is action-icons, actions, body, body-hyperlinks, body-images, body-markup,
icon-static, persistence, sound and inline-reply. Exclude icon-multi because Notify has no
multi-size wire shape. Inline reply follows the KDE x-kde-reply extension.

Keep allowlisted bold, italic, underline, link and image spans rather than flattening the body.
Image paths and action icons share a validator: absolute regular files under the allowed system
or user icon directories, within the image-data size cap. Bare theme names and arbitrary markup
are not accepted. Renderer span support and full theme lookup were outside this pass.

Keep the standard two-argument ActionInvoked signal. Encode reply text as
`inline-reply::<text>`; a bare inline-reply key is malformed. A third signal argument was
rejected because it breaks client introspection.

Sound priority is suppress-sound, trusted sound-file, configured urgency sound, then silence.
Ignore sound-name without theme resolution. Playback uses an internal PipeWire channel, not a
Lua round trip. DND is Supervisor-global, gates only sound and resets on Supervisor restart.
Critical notifications bypass DND and automatic expiry; Lua owns popup policy.

Use snapshots because each mutation changes feed or DND state, unlike idle's edge events.
A 20-entry feed views a 100-entry FIFO so actions can still resolve entries outside the feed.
Replacement without a fresh image deletes the old spool; eviction deletes the evicted image.
The historical spool was `/dev/shm/oblisk-$UID/notifications/notif-{id}.png`.

Add reply, per-urgency sound and DND writes alongside dismiss; expose urgency, reply availability
and structured body spans.

## 0034. Keyboard backlight, locks, layout, camera privacy, and Arch update checking

Add domain-named hardware capabilities rather than one generic adapter abstraction before any
caller needs polymorphic dispatch.

### 0034.1. Keyboard backlight rides UPower, not sysfs

Use the fixed UPower keyboard-backlight object, verified on the development machine.

1. Convert cached raw steps to rounded percentages and back, clamped to range.
2. Missing hardware yields -1 and no-op writes, with one diagnostic.
3. Reuse the system-bus connection.

### 0034.2. Keyboard lock state uses evdev, keyboard layout gets its own narrow compositor trait

1. Evdev supplies initial/live lock LEDs; sysfs is a static fallback. Physical Caps Lock changes
   updated sysfs but emitted no inotify events. Missing access defaults false with a diagnostic.
2. Keep a narrow keyboard-layout trait for Hyprland/niri, not a guessed workspace abstraction.
3. Select one primary keyboard and expose index-based switching; Lua computes cycling.
4. Correction: niri reports layout index directly, Hyprland then lacked reliable name-to-code
   correlation. Hyprland index read-back remained last-known, a disclosed cycling gap.

### 0034.3. Camera privacy: kernel-level detection primary, PipeWire supplementary

1. Detect raw V4L2 opens through device events plus fd inspection. The proposed streaming sysfs
   flag was absent on the real webcam despite a recent kernel.
2. Use PipeWire only for application-name enrichment; raw camera clients never appear there.
   Fall back to process names.

### 0034.4. Arch update checking uses the `alpm` crate, not `checkupdates`/`expac` subprocesses

A user-owned database prototype synced as uid 1000 without fakeroot, downloading 8.9 MB and
finding the same three updates as checkupdates. This was a behavior test, not an independent C-source audit.

1. Keep updates separate from sysinfo scheduling; interval zero suspends it.
2. Publish packages, versions, sizes, last success and errors, not only a count.
3. The capability owns privileged installation and progress, reusing process helpers and Polkit.

Reject combining these unrelated hardware mechanisms into a flat capability bucket.

## 0035. Sysinfo capability: five IDL fields, two hwmon preference lists, watch-driven suspend

Expose the IDL's five fields using three independently scheduled tasks. RAM and swap share one
read/interval; CPU and GPU temperatures share one scan/interval.

CPU uses busy/total deltas from procfs, counting idle and iowait as idle. Discard the prior sample
on resume so the first result is not an average across the dormant period. RAM uses MemAvailable
instead of reimplementing its estimate; swap uses SwapFree.

Resolve hwmon chips once because onboard sensors do not hotplug. CPU preference is k10temp,
then coretemp, with acpitz fallback; GPU preference is amdgpu, nouveau, then nvidia.
Core temperatures are sorted by core index and exclude package aggregates. No matching GPU
returns -1, not a fabricated zero. Wi-Fi, NVMe and battery sensors are deliberately excluded.

Intervals start at zero. Zero awaits only configuration changes, with no timer and no wakeups.
Three producers update their own fields under one shared state mutex and signal one push channel.
Each push bumps the capability revision; no snapshot is sent before a real sample exists.

Configure takes a table of whole-second intervals. Missing keys preserve values; any wrong-typed
present key rejects the whole call. Scheduling belongs to the Supervisor, not a generation.

Parameterize procfs and hwmon roots so tests use real temporary directory trees. Split CPU, RAM,
temperature and controller modules by concern, without a speculative cross-controller trait.

## 0036. Mpris capability: playerctld excluded, track-identity caching, strict seek state

MPRIS excludes duplicate/non-controllable players and keeps metadata and seek state tied to
real player events.

1. **Discovery filter.** Exclude the exact playerctld bus suffix because its Identity duplicates
   the proxied player, and exclude CanControl=false at registration. Other clients can still use
   playerctld directly.
2. **Selection policy.** Publish all players; Lua chooses which to display. No native active player.
3. **Album art.** Accept canonicalized existing file URLs without a directory allowlist because
   player caches vary. No spool copy, HTTP fetching or network image loader.
4. **Identity and unavailable length.** Use the bus suffix as ID and reconstruct it on writes.
   Missing or wrong-typed length is -1.
5. **Seeking.** Use SetPosition with a known track ID, otherwise relative Seek. Clamp the target
   to cached bounds in the Supervisor. Live mpv-mpris testing accepted a wrong track ID, so the
   upstream staleness guard was insufficient. Update position only from real signals.
6. **Metadata caching.** Track identity combines track ID, URL and title. Any change starts a new
   track; unchanged identity preserves previous art/length when an update omits or malforms them.
7. **Failure and discovery.** A transient read failure degrades the affected field, not the player
   entry. Discover through startup ListNames and subsequent NameOwnerChanged.
8. **Ownership.** One capability with per-player producers sharing a state mutex, following
   ADR-0035. Split watcher, player and controller on the session bus.

## 0037. Capability roster: generic push, per-module dispatch, no merged channel

The 2026-08-27 review found capability logic well-contained but snapshot and dispatch edges
duplicated, with only four Renderer seeds for nine snapshot capabilities.

1. **Generic push.** Replace per-capability snapshot helpers with one Serialize-based helper.
   The capability name selects the payload, following ADR-0029.
2. **Shared roster.** Pre-seed one nil-valued Lua signal per roster entry. Assert roster membership
   on Supervisor pushes to catch omissions before config boot. Unrostered names retain lazy lookup.
3. **Module-owned dispatch.** Each capability parses arguments, selects actions and spawns work;
   main keeps a literal capability match. Controllers own network/Bluetooth state and pending
   network intent. Scan-start/discovery-clear events use their normal channels for FIFO ordering.

Rejected: merge the single-variant capability channels. One select arm per capability was cheaper
than controller-side serialization plus permanent idle/audio exceptions. Revisit only when a
producer cannot reach the main loop's select.

Amendment, ADR-0076: the shared Capability enum replaces the string roster and membership assert.
One exhaustive capability-module match replaces main's string match. The channel rejection remains;
ADR-0070's lazy-start wrappers had already grown each one-line select arm to six lines.

## 0038. Surfaces come from `shell.lua`, not a fixed role enum

Lua declarations replace the engine's fixed surfaces. This settles the model; delivery depends
on sharing the scene and Wayland thread in ADR-0039.

1. **Declarations are the source.** Remove the fixed SurfaceRole enum and creation calls.
   Bar, overlay and wallpaper names become ordinary config IDs.
2. **Fixed declared set per generation.** The original design creates objects at startup.
   Adding/removing declarations or changing layer/anchor/monitor/namespace requires a swap.
   Visibility and protocol-mutable margins, exclusive zones, keyboard interactivity and size
   update in place. Object lifetime was later amended below.
3. **Per-output instances.** Expand declarations to `{id}@{output}`. Monitor hotplug adjusts
   instances without a generation swap because the declaration itself has not changed.
4. **Role properties.** Add namespace for compositor rules, keyboard interactivity for typing,
   and margin for edge offsets. Padding cannot replace margin.
5. **Input regions.** Keep the bounding-box union per surface when content is smaller than it.

Rejected: put all popups in one fixed overlay. That cannot provide independent namespaces,
keyboard focus, layering, per-output content or exclusive zones. The claimed zero-overhead
advantage did not justify those restrictions.

Amendments: ADR-0078 adds exclusive Ignore without changing the in-place property list.
ADR-0049 makes popup/window visibility create and destroy protocol objects because popup creation
needs an input serial and consumes its positioner. ADR-0088 applies create/destroy to panels too
because layer-shell remapping did not work in practice. The declared set remains fixed.

Scope reversals: ADR-0040 adds the previously deferred window/popup roles and corrects the claim
that click-outside dismissal has no portable answer, using popup grabs. ADR-0042 reverses the
Supervisor-owned lock-client plan: the compositor stays locked after client death and hides
non-lock surfaces, so an ordinary Renderer surface cannot paint the lock UI.

## 0039. The Lua VM, retained scene, and paint pass share the Wayland dispatch thread

Move Lua, Loader, retained Scene and painting to the Wayland dispatch thread. Lua and its values
make the scene non-Send; it must share the GL context's thread. The socket thread becomes framed
I/O forwarding only.

1. Construct Loader, signals, rescue state and Scene on the Wayland thread, not by cross-thread
   handoff.
2. Replace readiness/presentation/activation channels with direct calls; secure submission sends
   an outbound frame.
3. Share one ShapingHandle between sizing and painting, avoiding two roughly one-second
   FontSystem startups. Shaping itself stays off-thread.
4. Delete placeholder output sizes and use real per-surface sizes. Deferred as amended below.
5. Give overlay input-region calculation its production caller, following ADR-0038 decision 5.

Amendment: decision 4 needed ADR-0038's unified surface IDs, not just shared-thread access, so it
moved to Phase 20. Decisions 1–3 shipped with this refactor; 5 was unaffected. The claim that the
5 ms Lua CPU cap was already enforced was also wrong: pcall could catch hook errors and coroutines
escaped per-thread hooks. Those gaps were found in review and were being closed.

Trade-off: slow config evaluation now blocks Wayland dispatch. Off-thread shaping and evaluating
only on config edits limit that cost; snapshot pushes do not rerun the config. The CPU-cap
qualification above remains part of that assessment.

Rejected: ship resolved trees between threads. It adds bidirectional crossings to input and
surface creation, must resolve/drop Lua values and reopens cache invalidation. Evaluation errors
already reach rescue in-process. Moving EGL to the socket thread merely moves the same lifetime
problem because configure events and EGL surface lifetime depend on Wayland dispatch.

Scope: thread ownership only. Painting, declared-surface management and input remain separate work;
the refactor initially retains the same fixed surfaces and handshake.

## 0040. Four surface roles: panel, window, popup, lock

Replace ADR-0038's window/popup non-goals with four Lua constructors matching Wayland roles.

1. **Separate constructors.** Panel, window, popup and lock map to layer-shell, xdg_toplevel,
   xdg_popup and session-lock surfaces. Reject a shared kind-discriminated schema because most
   properties are disjoint. Rename the old surface constructor to panel without an alias;
   surface remains the umbrella term, not one role.
2. **Native popups.** Parent to a panel or window before first commit, using the null-parent
   creation path. A real popup grab provides focus and click-outside dismissal, correcting
   ADR-0038. Denied grabs are normal. Grabs require a real input serial before mapping;
   the engine destroys nested popups in reverse order.
3. **Click-derived positioning.** Pass the clicked node's parent-surface-relative rect to Lua.
   Popup size and anchor rect must be nonzero. Default adjustments are flip-y and slide-x;
   protocol precedence is flip, slide, then resize. No new node identity mechanism is needed.
4. **Reuse staging.** Windows follow null-buffer commit, configure, ack and attach like panels.
   Window state arrays and xdg_surface acknowledgments differ; min/max hints are advisory and
   fullscreen configure is binding. Expose title, app ID, size hints and a declinable close callback.
   Request server decoration but build no client frame; revisit only for a real titlebar need.
5. **Use SCTK with one escape hatch.** SCTK 0.21.1 wraps the required shell/window/popup/positioner
   operations. Reach through to xdg_popup.grab where it lacks a wrapper; the engine owns bookkeeping.

Lock-client process ownership is deferred to the ADR-0010 reconsideration. Content requires the
paint pass and declared-surface manager; popups also need input routing. Adding/removing any role
still requires a topology swap, with instancing following ADR-0038.

## 0041. `oblisk.screens` is Renderer-sourced; variants are a Lua loop

1. Lua loops already provide per-screen iteration; no variants/repeater constructor.
2. Screens are a Renderer-local signal from existing output bindings, not a second Supervisor geometry source.
3. Identity is the declared ID set. Monitor All changes instances in place; an explicit loop can change IDs and require a swap.
4. Hotplug reuses evaluation, topology comparison and rollback from the file-edit reload path.

Screens own geometry; workspaces reference connector names without duplicating it.

## 0042. The Renderer holds `ext_session_lock_v1`; the Supervisor supervises the lock client

Supersedes ADR-0010's lock-client ownership, not its idle-notify ownership.

The Renderer must hold the lock to paint its connection-scoped lock surfaces. The compositor
stays locked after client death. The Supervisor retains lock decisions, authentication and
client supervision; secrets use secure submission to the PAM worker.

No overlapping generation swap while locked; topology edits queue until unlock, but in-place
reloads continue. Maintain one lock surface per output. Handle denied and subsequently finished
locks distinctly; only successful authentication permits unlock, with a display sync before exit.

Recovery after client death depends on compositor lock-restore policy; respawning is not a
portable guarantee of recovery.

## 0043. Memory budget: declared fonts, atlas eviction, and PSS as the measurement

Target: 50 MB PSS per monitor for shell processes at steady state, not a measured achievement.

1. Report three measurements separately:
   1. Steady-state summed PSS across Supervisor and Renderers, compared with the budget.
   2. Per-Renderer USS, its private clean and dirty pages.
   3. Handoff peak while both generations live, not folded into steady state.
   GPU memory comes from DRM fdinfo, not smaps; deduplicate by device/client ID.
2. Load only config-declared font families and fallbacks at startup. No per-node family or live
   chain reload; missing coverage can render tofu. Lazy system discovery remains an upgrade option.
3. Clear the whole glyph atlas above a page threshold while idle, then rebuild on demand.
   Pages are 512×512 RGBA8, 1 MiB each; no per-glyph eviction API exists.
4. Size buffers to surfaces. A 2560×1440 RGBA8 buffer costs about 14 MiB before double/triple buffering.

No allocator replacement before measurement.

First measurement, niri/i915, one 1920×1200 output, Mesa 26.2.1: 149.7 MiB steady PSS,
196.9 MiB handoff, exceeding the target. Renderer PSS was 137.0 MiB with declared fonts versus
2207.9 MiB with 2648 system faces. LLVM accounted for 82.3 MiB; mapped fonts only 0.1 MiB.
The system-font fallback remained a hazard. Atlas growth was not tested by this short run.
Whether to exclude driver libraries from the target remained undecided.

## 0044. Signals resolve at layout time, and a push marks the scene dirty

1. Resolve property signals at layout time; passing a handle is reactive, calling get during
   evaluation is a snapshot. Nil uses the property default. Topology fields still reject signals.
2. A push marks one scene-wide dirty bit and reapplies the retained tree without rerunning Lua.
   Per-surface invalidation is an upgrade only if profiling justifies a dependency graph.
3. No memoization or dependency graph. Value-cloned computed dependencies can grow exponentially;
   shared identity would be needed for caching, and the evaluation CPU cap bounds the work.
4. Keep one Lua VM per generation and drop retained values before it. In-place reload does not
   reset it. The corrected reason is weak Lua references and invalid-state access, not a refcount leak.
5. Named writable state survives in-place reload, not a generation swap. Changed scalar seeds
   reseed it; unchanged seeds preserve runtime writes. Fresh table identities do not count as edits.

Rejected: rerun config on each push, which adds evaluation cost and changes callback identities.
List expansion remained deferred in this pass.

## 0045. Nodes reconcile by scoped `id`, and `list` items by `key`

1. Optional node IDs are parent-scoped reconciliation hints, not global addresses. Duplicate
   sibling IDs are errors.
2. Match explicit IDs only to the same ID; anonymous nodes match only anonymous nodes by position.
   Never let unmatched IDs inherit positional nodes. Retire unclaimed subtrees child-first.
3. List keys are optional functions of source elements returning strings. Duplicate keys are
   errors; no key means index matching, with rebuild cost after insertion.

List implementation was still deferred. Top-level surface IDs remained required and unique.

ADR-0143 supersedes decision 2's child-first retirement requirement; identity matching stays.

## 0046. Rescue renders out of band when no scene survives

1. Reload failure retains the working scene and reports through oblisk.rescue. Startup failure
   has no scene and needs an independent display path.
2. The Supervisor re-execs a rescue process with hardcoded Rust drawing, no Lua, capabilities,
   generation ID or authority. Reap it once a real generation presents.
3. Display the error and config location only, not a fallback shell or recovery UI.

Rejected: a default Lua config depends on the failing machinery; stderr alone is invisible to
users launching from a session.

## 0047. The config is a directory, not a file

1. Restrict require to the config directory's ?.lua and ?/init.lua paths, not system Lua modules.
   Native module loading remains disabled.
2. Clear required-module caches on re-evaluation because the generation's VM survives reload.
3. Recursively watch Lua files and filter unchanged content hashes. Watching only successfully
   loaded modules would prevent recovery from a broken first evaluation.

Lua's module cache provides singletons; parent-scoped IDs make modules reusable.

## 0048. The config VM drops the blocking parts of the Lua stdlib

Use an explicit Lua library set. Omit io and os, then restore only time, date, clock and getenv.
Keep coroutine/string/table/math/utf8; debug, FFI and native module loading remain unavailable.

Blocking syscalls stall Wayland dispatch and evade instruction-based CPU limits. Process commands
must use the managed callback API; os.exit would terminate a generation outside its lifecycle.
Direct file I/O is a deliberate loss, covered for now by require or subprocess helpers.

Rejected: merely document the hazard, or restore a separate Lua thread just to accommodate it.
A native asynchronous file reader waits for a caller.

## 0049. Popups and windows are created when shown, not at generation startup

1. Separate declaration lifetime from protocol objects: panels originally lasted a generation,
   locks a lock session, and popups/windows only while shown.
2. Visibility creates/destroys popup/window objects through dirty re-resolution.
3. Opening a declared popup is a value change, not topology; unopened declarations allocate no
   Wayland/EGL objects.

No live popup repositioning; recreate per open. Destroy nested popups child-first.
Amendments: keep the input serial armed through the poll-loop apply, then disarm it.
A requested grab without a serial refuses creation. Build PopupSpec from resolved properties so
anchor signals follow clicks, while validating literal mistakes during evaluation.

## 0050. Pointer hit-testing walks a path, a click is press-and-release on one node, and focus attributes the secret

1. Hit-testing returns the ancestor path. Half-open bounds and ancestor clipping must agree with
   painting; accumulate parent-relative rects for surface-local coordinates.
2. Press arms; release must match surface, rect and button. Moving the target cancels the click.
   Support left/right/middle, not an ambiguous catch-all button.
3. Call on_click(rect, button) with surface-local logical geometry and a button-name string.
   Config routes the rect to its chosen popup; callback errors are logged, not fatal.
4. Focus selects the secure submission target. No focused field means no secret frame; zeroize
   either way. Leaving focus clears it. All supported pointer buttons qualify as real input.

No portable panel click-outside grab. Existing button-agnostic callbacks now also receive
right/middle clicks; left-only policy belongs to config.

## 0051. A popup anchors to one parent instance, and a compositor dismissal latches

1. Anchor to the parent instance receiving the arming click. A non-grabbing open without a click
   uses the first parent instance. Never expand one popup across every monitor.
2. Compositor dismissal drops the handle, calls on_dismiss and latches recreation until new
   pointer input. A false/true visibility cycle may occur within one batch and cannot be the latch.
3. A requested grab without a serial means no popup, not a silently ungrabbed popup. Compositor
   denial follows normal dismissal handling.

Drop nested handles child-first; latches die with the generation.
Known limit at delivery: release-triggered grabs work on niri but fail wlroots serial validation
on sway/Hyprland. An on-press hook was not yet available.

## 0052. The session lock is commanded through a capability, and its surfaces live for the lock

1. Expose lock through generic capability invocation, with no Lua unlock action. Only PAM success
   authorizes unlock; a script-callable unlock would bypass authentication.
2. Declare one root lock node. Retain it from startup, but create per-output protocol surfaces
   only for the lock. Visibility, monitor selection and geometry are protocol-owned.
3. Refuse acquisition without a working lock tree and exactly one lock/authenticate secure field.
   Veto in-place edits that remove that field while locked; otherwise the user can be stranded.
4. Acquisition failures use rescue; authentication failures use lock state with an attempts
   counter so repeated identical errors remain observable.

Secure fields read the keyboard directly. The Supervisor returns an authenticated unlock command;
the Renderer does not interpret PAM outcomes itself. Compositor-initiated teardown uses the
protocol's legal unlock-and-destroy verb without initiating an unlock.
Idle callback delivery and a built-in fallback lock screen were not built in this pass.

## 0053. Five specified capabilities were never given a phase, and a bar is what found them

A real bar exposed capabilities specified without implementation phases.

1. Build battery, system clock and missing audio fields first; schedule brightness, workspaces
   and power separately rather than deciding compositor architecture incidentally.
2. Push time only when the epoch second changes. Minute-only UI still causes excess work;
   configurable cadence remains an upgrade.
3. Align the audio payload with the spec while retaining PID attribution. Master volume/mute
   are real; per-app fields were explicit placeholders pending subscriptions. Distinguish mixer
   Props from unrelated ALSA Props by channelVolumes.

Later brightness work uses udev plus a 30-second fallback, deterministic firmware/platform/raw
device preference, and unprivileged logind writes. No hardware means no snapshot, not zero.
Later power work makes fields independently optional across UPower and power-profiles-daemon;
UPower was live-verified, profile-daemon support was not.
Writable system.state and its producer remained undecided.

## 0054. The icon theme resolver lives in the renderer, and `image` is the node that draws a file

1. Resolve icon themes in the Renderer with freedesktop-icons; a synchronous Supervisor lookup
   would block the dispatch thread and require a missing request/response protocol.
2. Absolute icon names draw files directly; other names use theme resolution.
3. Add image for non-square path-based content; icon adds square sizing and name resolution.
4. Rasterize SVG with resvg and decode raster formats with femtovg's image dependency.
   Cache by resolved path and pixel size.
5. Defer app-ID/desktop-entry lookup; no separate Lua find_icon API.
6. Include file mtime and length in cache keys so overwritten tray spools refresh.
7. Queue texture deletion until before the next frame; recorded draws still reference IDs until flush.

Byte-bounded LRU and off-thread misses were not built; the initial cache was count-bounded FIFO.

## 0055. Wallpaper is an `image` on a Background panel, not a capability

1. Remove the wallpaper capability. Config owns the Background panel, monitor and image source.
2. Change wallpaper through named state, not IPC. Durable runtime selection was not implemented.
3. Image fit modes are cover, contain and stretch. Cover is default; no tile without a caller.
4. Keep ADR-0002's transitions deferred. Immediate texture replacement implements its
   first-frame/reload branch, not an animation system.
5. Expose config_dir from the actually loaded shell.lua location for bundled assets.

No picker or folder scan in this pass. A 3840×2160 RGBA texture costs roughly 32 MB; measurement
was left to the memory harness.

## 0056. `workspaces` speaks niri, and § 2.9 is wrong in three places

1. Implement niri first without a compositor trait. Missing niri leaves state nil; a second
   tested implementation must justify abstraction.
2. Use a separate event socket from keyboard to preserve controller lifetimes. A third consumer
   could justify a shared owner; two did not.
3. Publish ordered workspace entries with stable ID, display index and optional name.
   Focus takes the stable ID, not the index.
4. Focused-workspace is optional per output because global focus belongs to only one output.
5. Omit unavailable niri fullscreen state rather than fabricate false; class maps to app_id.

No window list beyond the focused client, or special-workspace model.
Amendment, ADR-0075: move compositor probing to a shared top-level module; keep niri types local.

## 0057. `json.decode` is one function on the engine's existing null mapping

1. Reuse the capability payload converter. JSON null becomes Lua nil, not a sentinel; null
   array elements leave holes that stop ipairs.
2. Decode failures return nil plus a message, including non-UTF-8 input and Lua conversion errors.
3. Success returns one value, failure two; a trailing nil on success changes Lua call arity.
4. No encoder until a caller needs it.

Rejected: jq pipelines, query libraries or a second Lua parser. Lua already queries tables, and a
second converter would disagree on null. Decoding null succeeds with nil and no error message.

## 0058. A crashed Renderer is detected and respawned, because a lock cannot be recovered otherwise

1. Watch the authoritative child's exit directly, not failed capability pushes.
2. Classify clean/nonzero/signal exits and include lock state in the diagnostic.
3. Bound restarts to prevent strobing failure loops: implementation uses three within 60 seconds.
4. Reacquire an active lock on replacement, with a new acquisition identity, only if the on-disk
   config still passes the single-authentication-field predicate. Log denied takeover.

Nested niri takeover was measured working; other compositors may refuse it. Do not pin an older
config silently, restart the whole stack and lose lock knowledge, or ask a dead client to recover
itself. Replacement loses named state. Supervisor death remained unhandled in this pass.

## 0059. The Renderer exits when the Supervisor is gone, and a service manager reruns the pair

1. Treat inbound disconnection separately from an empty queue and exit the Renderer with code 70.
2. Exit while locked without unlocking. Flush pending lock requests; avoid normal destructor
   teardown that would send an illegal destroy for an acquired lock.
3. A tripped Renderer restart brake exits the Supervisor with code 3; the service unit prevents
   restarting that code.
4. Let the service manager restart and collect the pair and its children, scoped to the graphical
   session. Compositor startup commands alone do not supervise them.

Rejected: reconnect to fresh Supervisor state or let the Renderer spawn its own authority.
A persistent lock marker was not built yet.

## 0060. A restarted Supervisor learns the session was locked from a file in the runtime directory

1. Keep the lock fact in $XDG_RUNTIME_DIR/oblisk-session-locked so it survives SIGKILL but not
   the login session. Do not serialize transient attempts or acquisition state.
2. Drive it from Renderer outcomes: Locked sets; Unlocked/Finished clear; Refused leaves it.
   Renderer loss must not clear it, because the compositor remains locked.
3. Feed a startup marker into the existing reacquisition path, still gated by a valid on-disk
   authentication field. Distinguish restart from crash replacement in diagnostics.

Nested niri verified recovery across Supervisor SIGKILL. File errors are logged; absence reads
unlocked. An externally unlocked session can leave a stale marker and cause one extra prompt;
the protocol cannot query lock state. Clean Supervisor shutdown must not clear the marker.

## 0061. Desktop entries are an enumerated capability, not a lookup call

Amends ADR-0054 decision 5.

1. Publish desktop entries as an applications snapshot, not a synchronous lookup.
2. Repeat entries in by_app_id rather than exposing zero-based indices to Lua. Exact matches
   precede case-folded and reverse-DNS fallback matches.
3. Keep parsed argv Supervisor-side; launch by entry ID. Managed process.run has the wrong
   lifetime because a generation swap would reap the GUI application.
4. Rescan at startup and explicit refresh, pushing only changes. No watcher generalization for
   infrequent package-install events.

Deferred: localization, OnlyShowIn/NotShowIn, terminal guessing, embedded field-code stripping and
incremental scans. Terminal entries refuse without $TERMINAL; scans run off-thread.

## 0062. Hover is a signal the engine writes, not a callback it calls

1. Hover is readable state, not only callbacks whose leave edge can be lost during reconciliation.
   An action callback may be added separately.
2. The Renderer owns named, read-only hover and hover_rect signals. Keep the last rect on leave
   because a closing popup still needs a valid anchor.
3. The hover property preserves its handle structurally rather than resolving it to a boolean.
4. Write only on boundary changes, not each motion event, to avoid unnecessary scene resolution.
5. Every node on the hit path is hovered, including composite ancestors.

No separate tooltip node, scrolling, animation, cursor changes or keyboard-focus equivalent in
this pass; a non-grabbing popup already supplies tooltip presentation.

## 0063. A display list is what makes a repaint skippable

1. Build and execute one flat display list. Compare it with the last painted list before touching GL.
   A parallel hash could drift from actual drawing.
2. Compare plain Rust values, not Lua table identity. Exact float equality is sufficient for
   repeated parsed inputs; NaN costs an extra paint rather than a missed one.
3. Store precomputed ancestor-intersection clips per draw. Exclude fully clipped subtrees.
4. Invalidate on new/undefined buffers and remember a list only after a successful swap.

Measured 25-second A/B: Renderer CPU 0.80% to 0.60%, niri 0.52% to 0.36%; wallpaper repaints
fell from 2.23/s to zero. Resolution and cloning still visit every surface on each dirty push.
Skipping invisible resolution remained separate because config maps could have side effects.

## 0064. A masked field draws from a count the tree never holds

1. Pass secure character count beside the scene into painting, never as a retained property.
   Count Unicode characters, not bytes.
2. Match the focused capability/action destination so another field cannot display its length.
   Unfocused fields show placeholders; focus changes clear the buffer.
3. A keystroke requests paint without scene resolution; display-list equality narrows GPU work.
4. Probe the installed Oblisk PAM service per authentication, falling back to login when absent.
   Naming a missing service would hit pam_deny on the observed system.

Leave the machine's PAM failure delay and lockout policy intact. No caret, placeholder styling
or general text editor in this pass.

## 0065. A font file is mapped once and shared, not copied per reader

1. Map font files once and share their Arc-backed bytes across shaping, cosmic-text and femtovg.
   Avoid Canvas's copying font API. Measured private dirty fell 49.7 to 26.4 MB, RSS 192.5 to
   166.3 MB; deleting emoji entirely saved less than another 4 MB. Mapping accepts the existing
   risk of a font file being modified in place.
2. Keep femtovg/OpenGL ES. The measured Mesa pages were shared clean; removing their mapping did
   not justify replacing the renderer. CPU wallpaper buffers would cost 18.4 MB private dirty
   when double-buffered at 1920×1200. Judge physical/private cost, not RSS alone.

No effort to shrink reserved virtual address space or compensate for the missing CJK font package.

## 0066. The icon path lookup is the paint loop, not the GPU

Profile before replacing rendering machinery. In 30 seconds, recording draws cost 165.1 ms versus
8.3 ms for flush; icon lookup alone took 1638 µs of each 1645 µs icon call.

Memoize (size, name) to path or absence for the process lifetime, matching the cached theme's
lifetime. Negative results matter because misses search the whole inheritance chain.
Parsed theme-index caching did not cache this lookup.

Measured afterward: icon resolution 79 µs, recording 39.1 ms, bar repaint 3.9 to 0.57 ms,
GL phase 0.64% to 0.19% of a core. Keep swap pacing, per-image revision stat and scene cloning;
none was the measured bottleneck.

## 0067. The Wayland client addresses surfaces by position, the retained scene by identity

Keep Wayland protocol instances in a position-addressed vector and retained nodes identity-keyed.
Their callers hold different information; rekeying protocol events would add scans or allocations.
Indices also allow methods to borrow EGL, painting state and surfaces together.

Rejected: named map-state transitions without a real invariant. Candidate null-buffer staging
does not share map-state lifetime. Keep ID-taking entry points where callers actually hold IDs.

## 0068. Paint properties are parsed once at apply time, and a bad one fails the pass

Parse paint properties once during scene apply and reject malformed values through rollback,
including on hidden nodes. Per-frame default substitution disagreed with geometry validation
and could repeatedly log huge values.

Painting keeps only scale/focus-dependent arithmetic over typed data. Configure-time surface
updates retain their separate last-good-value rule. Geometry already parsed at apply time;
there was no second parser migration to build.

## 0069. A scroll offset is engine state the layout pass clamps

1. Store and apply scroll offset in scene geometry so painting, hit-testing and hover agree.
   Measured cached applies: 200 rows 2.19 ms, 500 rows 6.14 ms; virtualization stays an upgrade.
2. The engine writes a named read-only scroll signal; Lua lacks the measured extents to clamp it.
3. Add a property to flowing containers, not a duplicate node kind.
4. Layout clamps to content minus viewport and writes back the value actually used.
5. A content-sized viewport has no scroll remainder and no-ops.
6. Prefer compositor pixel deltas; otherwise use value120 steps of three lines. Ignore deprecated discrete data.

Amendment, ADR-0077: apply the offset in the solved-geometry finish walk. Keep margins and spacing
in scroll bounds; taffy's CSS overflow bounds omit margins. No scrollbar until extent exposure is needed.

## 0070. A capability starts when the config first reads it

1. First namespace access starts a capability, covering reads and invocations without a second
   config-declared roster.
2. Startup is one-way for the Supervisor lifetime. Stopping would require bus-name, in-flight
   request and snapshot/revision lifecycle rules.
3. Every generation resends idempotent starts; existing snapshots replay, new state begins nil.
4. Construct inline in the Supervisor loop. Startup enumeration can delay queued frames;
   asynchronous construction is an upgrade if measured startup cost warrants it.
5. Secure-submit targets also start their backend; Polkit was then outside the snapshot roster.
6. Polkit registration/session-subject failures log and continue, including an already-owned agent.
7. Permit empty configs and zero surfaces; their presentation handshake completes without work.

## 0071. The GL context is built by the first surface that needs it

1. Make EGL optional and initialize on the first surface bind. Hold the Wayland connection so
   its lifetime guarantees the raw display pointer.
2. Candidates reach ready without loading GL; initialization moves after ActivateDraw.
   Measured ready was 104 ms; first binding grew from about 2 to 30 ms.
3. Accept initialization failure occurring later rather than eagerly allocating a context solely
   to prove it works. The entry recorded the resulting later-failure/rollback limitation.

Empty-config RSS fell 151 to 16 MB, PSS 37 to 13 MB. Verify with a real session and driver
mappings; unit tests cannot establish lazy GL loading.

## 0072. A tray item is addressed by the name it registered

1. Keep registered destination separate from resolved owner identity. Chromium tray properties
   accepted the well-known destination but rejected reads addressed to its unique owner.
   The owner still keys cleanup and spool files; owner changes between lookup/read remain a risk.
2. Icon foreground supplies SVG currentColor and participates in the texture cache key.
   Rewrite before usvg resolves it; leave files without currentColor unchanged.

No full CSS parser: textual color replacement can also match comments/attributes.
Keep theme vectors preferred over undersized pixmap fallback.

## 0073. The tray host asks the bus what is already there

At startup, enumerate both KDE and freedesktop StatusNotifierItem well-known names and adopt
them through the existing registration path. Keep live registration signals; duplicate adoption
overwrites the same identity.

This recovers apps that do not re-register after shell restart. Object-path-only registrations
remain undiscoverable without probing every bus connection; reject that expensive scan.
Existing liveness checks cover disconnects during adoption.

## 0074. The tray backend exposes what the spec defines

Framework coverage is judged against real application/protocol support, not only dev-config use.

1. Remove spools with items and sweep on startup. Concurrent Supervisors may temporarily erase
   each other's icons; that debugging-only risk was accepted.
2. Publish Passive status; hiding is Lua policy, unlike side-effecting activation semantics.
3. Carry base, attention and overlay icon variants separately; Lua chooses presentation.
4. Add SecondaryActivate and Scroll. ItemIsMenu gates only primary activation; scrolling needs
   its own parser rather than Activate's numeric coordinates.
5. Resolve item-local IconThemePath before theme names; reject path separators in icon names.

Deferred: unused legacy attention movies, X11 WindowId and category sorting.

## 0075. Compositor detection is session-level, and `workspaces`' seam is a file

1. Move session compositor detection out of keyboard; keep the keyboard-shaped CompositorLink
   trait with keyboard.
2. Use an explicit probe-precedence table and name unsupported sessions. XDG_CURRENT_DESKTOP
   is diagnostic context, not evidence that a compositor is running.
3. Reduce neutral workspace/window rows, not niri types. Keep protocol mapping local and share
   publication/deduplication behavior.
4. The extensibility boundary is a module, not a speculative trait. A second backend adds a
   sibling and exhaustive match arms, retaining neutral reducer tests and wire fixtures.

Whether a future abstraction needs one trait or two remains undecided.

## 0076. The capability roster is a type, and the module tree mirrors it

1. Replace the string roster with shared Capability and exhaustive matches. Typed snapshot
   names replace runtime membership assertions. Idle/Polkit were then explicit non-roster cases.
2. Capabilities owns controllers; main owns the loop. Race only receives, then process the winning
   signal, so cancellation cannot discard an event during an awaited state rebuild.
   Keep separate typed channels, not ADR-0037's rejected merged payload channel.
3. Organize by capability rather than D-Bus/hardware transport. Lock remains boot-created for
   restart recovery and is passed into dispatch.

Follow-up, 2026-09-01: derive channel senders, receivers, selection and construction from one
macro list, exhaustively checked against the roster. Lock is the stated channel exception.
Keep the signal payload enum hand-written and exhaustively dispatched.
Do not reorganize unrelated cohesive Renderer files.

## 0077. The layout math is taffy's, not this crate's

Supersedes ADR-0023's hand-written arrangement, one-pass and descendant-positioning choices,
not its unrelated deferred features.

1. Taffy owns sizing/positioning; scene code retains identity, leases, parsing, scroll bounds and
   elision. Prepare once, solve, then finish. Keep text measurement cached and pixel snapping in paint.
2. Fix stretched descendants after content-size resolution. Fill/percent in an indefinite flow
   container still resolves to zero.
3. Hidden nodes leave layout; getters run once in declaration order. Preserve Stretch overriding
   explicit size rather than introducing an unrelated config change.
4. Keep depth 64. Measured worst-case stack fell about 1400 to 1040 KiB, including computed signals.

Build a fresh solver tree per apply; persistent solver caching would need another reconciliation
lifetime and waits for measurement.

ADR-0143 removes the lease ownership in decision 1; transaction rollback stays.

## 0078. `exclusive` is three answers, not a boolean

1. Add exclusive Ignore beside existing booleans: reserve, respect others' reservations, or
   ignore them without reserving. This fixes backgrounds shrinking below bars.
2. Use Respect for unresolved signal placeholders; temporary reserve/ignore would move other UI.
3. Reject unknown strings instead of silently defaulting.

No numeric custom zone or wholesale enum migration without a caller. Versioning remained
pre-release; the entry did not bump the placeholder minor version.

## 0079. A rounded clip is an offscreen pass, not a rounded scissor

1. Rounded clipping renders children offscreen and composites through the rounded path, then
   paints the border. The recursive display-list group remains comparable.
2. Reject rounded-scissor intersection: femtovg's single rounded rect cannot represent the required
   intersection. The measured pill test leaked 8% along a straight edge.
3. Radius alone does not enable the extra pass. Box remains default; a childless rounded clip
   requires no offscreen group.

Hit-testing remains rectangular, so rounded corners can still receive input. Allocate/free the
target per repaint after consuming draws; a size-keyed pool waits for a high-frequency caller.

## 0080. The battery comes from UPower, not sysfs

Replace the stale sysfs/udev battery source and ambiguous charging boolean with UPower.

1. Read DisplayDevice state names, presence and estimates in one property snapshot. Presence
   requires battery type and IsPresent; unknown states remain Unknown and zero estimates become nil.
2. No sysfs fallback: it misses observed capacity changes and cannot supply equivalent estimates
   or pending-charge states. Missing UPower yields no data, not a fabricated answer.

Measured capacity fell 69 to 65 with zero power-supply uevents while UPower tracked it.
No speculative 0% glitch filter or charge-threshold field. Brightness's verified udev path stays.

## 0081. The stubs are checked against the config, not just parsed

Check real configs against Lua stubs, not just parsing and field-name inventories.
A probe found 21 useful missing Signal unions; add them while preserving structural restrictions.

Use complementary checks: sample every declared type through real scene apply to catch promises
the engine rejects; language-server checks catch omissions exercised by real configs.
Check lua-meta as its own workspace so library diagnostics cannot be suppressed; use explicit
LuaCATS prose markers on return annotations.

Reject generating types from the current parser layer: it would relocate hand-written claims,
not derive them. Language-server checking is optional when absent, but the skip is explicit.

## 0082. `oblisk.network` is subscribed to the association, not just to the scan

1. Connectivity comes from PrimaryConnection, not AP identity; AP lists cannot describe wired
   routes, radio power or DHCP progress.
2. Subscribe to association/device/manager changes, not only scan events. An observed 90-second
   association emitted no old scan wakeups.
3. Reduce all sources through one Changed event and full state rebuild.
4. SSID names the association; connected names the default-route result. Wired default wins.
5. Ethernet enabled reports activation state so disconnect has an observable read-back, not carrier.
6. Property-stream cache hydration supplies startup state; avoid a duplicate explicit read.
7. Watch strength on the associated AP only, retargeting/aborting with association changes.
   Measured 26 versus 76 events over 180 seconds did not justify debounce.
8. Retain AP proxies across rebuilds: 11.25 to 0.84 ms at ten APs. Sort the connected AP before
   the top-20 cut so a weak but active connection cannot disappear.

Multi-adapter selection and hotplug discovery remained unbuilt.

## 0083. `network:connect` reuses a saved profile, and the AP order is deterministic

1. Reuse saved SSID profiles with ActivateConnection; create only unknown profiles. The old
   unconditional creation produced duplicate UUIDs on every rejoin.
2. Update a retyped saved password without deleting/recreating other profile settings.
   Skip enterprise updates because GetSettings omits secrets; a proper secret agent is required.
3. Break equal-strength AP ties by SSID instead of inheriting HashMap order.
4. Reject signal-tier ordering after measurement: neighbours held strength between scans and
   the changing associated AP was already pinned first.

Association failure reporting remained missing until ADR-0084.

## 0084. A connect attempt reports its own outcome

1. Carry connecting_ssid and connect_error in snapshots as remembered attempt state, preserved
   across backend re-derivation.
2. Observe the activation object's StateChanged verdict and read current state after subscribing.
   A failure in that gap loses its detailed reason, not the completion.
3. Hand-write only the broken Active proxy: the dependency subscribed to lowercase state_changed,
   which NM never emitted. Keep the proxy crate elsewhere.
4. Accept replacement attempts; discard verdicts whose SSID is no longer current.
5. Saved networks activate without waiting for a new secret. Unknown-network prompting remains
   separate surface-policy work.
6. The example panel reports the attempt in its header without copying/enriching every AP row.

Live tests confirmed profile reuse and success/failure reporting. The 45-second timeout is a
backstop, not polling-based completion.

## 0085. The Wi-Fi password prompt, and what it cost to give a popup the keyboard

1. The Supervisor publishes password_ssid only when a new secured connection needs input.
   Saved and open networks proceed; hidden/unknown security defaults to prompting.
2. Cancellation is idempotent and centralized on panel close, without clearing unrelated errors.
3. Keyboard focus includes shown child popups, allowing one secure field anywhere in the scope.
4. Originally claim keyboard focus before mapping the popup; changing focus on the mapped parent
   broke its grab. ADR-0087 supersedes this bar-wide focus workaround.
5. Arm a newly visible sole secure field when focus already exists, but never steal an explicitly
   selected field.

ADR-0087 puts this prompt on its own focused surface; popup focus-scope behavior remains useful.
Passwords never reach Lua callbacks.

## 0086. `lua-meta` types nothing unless a signal is `userdata`

1. Lua-language-server 3.19.1 treated class unions as accepting arbitrary tables. Use built-in
   userdata via Bound for signal-valued property unions.
2. Define generic Signal methods as fields so callback payload types bind. Map chains beyond one
   hop and computed callbacks remain weakly typed.
3. Promote mismatch diagnostics from Hint to the checked severity in both repo and generated configs.
4. Find the editor-bundled language server when absent from PATH, and retain useful diagnostic
   output instead of expecting an unrequested JSON report.

Lua any, permissive classes and list callback inference remain limits; runtime parsers are the gate.

## 0087. The panel host is a layer surface, and a staged layer request needs its own commit

1. Make the example panel host a layer panel with a full-size outside-click catcher under the card.
   Respect the bar's exclusive area so bar clicks remain reachable.
2. Claim keyboard only while the password prompt needs it; restore the bar to None.
3. Replace popup SlideX with config clamping; the example retains its single-output assumption.
4. Explicitly commit changed layer-shell state when mapped and non-candidate. Paint deduplication
   may skip swaps, otherwise nonvisual focus/margin/size/exclusive changes stay pending.
5. Centralize panel toggle and prompt cancellation now that popup dismissal is no second writer.

Live niri tests verified nonvisual focus acquisition/release and card placement.
Supersedes ADR-0085's popup-driven bar-wide keyboard claim.

## 0088. Hiding a `panel` destroys it, because the layer-shell re-map is not honoured

Amends ADR-0038: protocol-correct layer remapping failed to show the surface on tested niri,
despite configure, acknowledgment and fresh buffers. Cache, timing and missing-state-commit
explanations were ruled out.

1. Hiding a panel destroys child popups, EGL resources and role objects; showing rebuilds them.
2. A never-mapped hidden startup panel remains created for presentation staging; distinguish it
   from a previously destroyed panel.
3. Revalidate size/anchor constraints on every show because signals can change while hidden.

Accept per-toggle allocation and configure latency; reusing an object that never reappears is worse.

## 0089. A `text` can wrap, and an unwrapped one now measures the line it draws

1. Shaping returns shared visual lines, not just count. Slice by min/max glyph cluster bounds;
   LayoutRun.text repeats the original paragraph and visual-order glyphs may be bidi.
2. Wrap is opt-in; None measures without a width so reserved height matches single-line painting.
3. Absent or zero max_lines means unlimited; negatives error.
4. Elide the last retained line using the remaining text, not only that line. Rejoined dropped
   lines may collapse whitespace inside an already-truncated remainder.
5. Break lines after sizing, then paint one line at a time; femtovg does not perform line breaking.

No hyphenation or separate character-wrap mode. The existing unscaled HiDPI font-size defect
was disclosed but left for hardware verification.

## 0090. A notification's actions are kept, and a config can invoke one

1. Store typed actions, separating default activation and inline reply from visible button rows.
2. Action icons are theme names; reject path separators to prevent arbitrary file display.
3. Accept only keys the sender declared.
4. Invocation closes unless resident; emit the corresponding close signal.
5. Cap at eight actions with 64-byte UTF-8-safe labels.

Live tests verified normal/resident invocation and rejection. Reply-placeholder metadata waited
for the then-unimplemented ordinary input path.

## 0091. The attached picture and the sending application's icon are two fields

1. Split attachment image_path from sender app_icon instead of one competing precedence chain.
2. Carry bare sender theme names to the Renderer; validate paths through trusted roots.
3. Any slash makes the value a path candidate. Testing only is_absolute would let relative
   traversal strings masquerade as theme names.
4. Rename the misleading old icon_path without an alias before any release.

Desktop-entry metadata was deferred until a consumer needed it.

## 0092. An ordinary `textfield` reads the keyboard too, because text-input-v3 types nothing

Supersedes ADR-0027 decision 3.

1. Ordinary and secure fields both read keyboard/xkb; text-input alone delivered nothing without an IME.
2. Composition, dead keys and compose sequences remain unsupported. Later text-input integration
   can augment raw keys when an input method exists.
3. Plain fields initially require a click, without the secure field's sole-target auto-arming.
4. Address plain focus by surface and rect, and explicitly exclude secure nodes from plain paint.
   A retained NodeId is an upgrade; matching rect alone could expose plaintext on a password field.
5. Change/submit callbacks receive full text. Submit empties but keeps focus; Escape originally
   clears without dropping focus.
6. Do not focus a field with no usable destination/callback.
7. A textfield press arms no ancestor button click, preventing reply entry from activating its card.

Plain draft state stays outside the retained tree; input repaints without forcing scene resolution.

## 0093. A notification carries when it arrived, because nothing else can work it out

1. Record arrival in epoch seconds, using the same helper and units as system.time.
2. Use wall time for human-readable age; clock adjustments can change it. No second monotonic field.
3. Replacement content gets a fresh timestamp even when its ID is reused.
4. Set it when Notify assembles content, not when a snapshot happens to push.

Config maps cannot safely record arrivals, and reload would misdate existing entries.
Do not publish a fixed expiry deadline when holds can move it.

## 0094. Expiry is held off by a deadline, not paused by a flag

1. Hold expiry for a self-releasing duration; zero releases early. Repeated activity can renew it
   without requiring a matching resume from a config process that may disappear.
2. Clamp holds to five minutes.
3. Hold globally, not by notification ID.
4. Preserve remaining countdown time; neither restart it nor expire immediately on release.
5. Keep one task per notification and add a watch-driven hold, not a second shared deadline registry.
6. Do not publish hold state to Lua; log transitions.
7. Use tokio time for deterministic paused-clock tests.

Typing could renew holds immediately; resting-pointer holds still needed a hover callback.

## 0095. `on_hover`, because a config could see a hover but not act on one

1. Fire hover callbacks only on boundary changes already detected by hover synchronization.
2. Require a named hover slot on the same node. Rect/position identity can transfer state to a
   replacement node; a silently inert callback is an error, not a fallback.
3. Pass only the boolean. The paired hover_rect already supplies geometry.
4. Log callback errors without aborting a working scene.

Live expiry-hold tests closed ADR-0094's resting-pointer case; a callback without a slot fails reload.

## 0096. A theme name in `image-path` is the application's icon, not a picture

1. A bare theme name in image-path feeds the sender app_icon fallback, not the attachment.
2. Keep image_path as an existing absolute picture path; mixing forms recreates the original ambiguity.
3. Positional app_icon takes precedence.
4. Distinguish paths by any slash, then apply trusted-path validation; relative traversal is not
   a theme name.

Live checks verified both separate picture/icon rendering and notify-send's hinted theme icon.

## 0097. The notification card, and the four things the config had to decide itself

Share one notification-card component between popup and history.

1. Initially group by app_name and order by newest content. Desktop-entry identity was still deferred.
2. One hover region owns the stack's expiry hold; per-card leave/enter ordering could release
   a hold just renewed by a sibling.
3. A Reply button requests keyboard focus before showing the field; mapping notifications must
   not steal focus unconditionally.
4. Card activation invokes the sender's default action, otherwise dismisses.

Suppress popup overlap while history is open. Keep expansion state in bounded shared tables,
not a new named signal per arriving app. Rich spans and animations were still unavailable.
Live tests covered grouping, wrapping, pictures, expiry holds and inline reply.

## 0098. A popup is retired, not hidden, and the config is what remembers

1. Config owns a seen-set because popup retirement is presentation, not notification removal.
2. Key by ID and timestamp so replacement content can pop up again.
3. Mark on history open and close, including notifications arriving while history is visible.
4. Replace the set with the current feed rather than merge forever.

Keep overlap suppression while history is open. The card's X still dismisses from history too;
changing that behavior was deliberately left for discussion.

## 0099. `ResolvedNode` carries its `NodeId`, and a plain field's focus is keyed on it

1. Carry retained NodeId into resolved nodes so plain focus survives movement and inserted siblings.
2. This is the engine's scene-wide identity, not the optional parent-scoped config ID hint.
3. Still require the node to be ordinary input; a node can gain secure_submit without changing identity.
4. Keep rect-based click arming: a cancelled click is a different failure from invisibly retained typing.

Live testing confirmed a reply remained visible and submitted after a new notification moved it.

## 0100. Expiry retires a notification from the popup; it no longer removes it

1. Expiry sets a flag in the existing queue rather than removing history or creating a second list.
2. Still notify the sender of closure at expiry; later history actions may reach a sender that
   no longer remembers the ID.
3. Transient entries are the exception and are removed on expiry.
4. Repeated or stale-incarnation expiry is a no-op.
5. Keep the 100-entry queue and 20-entry feed cap.
6. Replacements reset expired and receive a new timer.

Popup config filters expired entries; history appearance and X-button retirement policy remain config work.

## 0101. `desktop_entry` and `reply_placeholder` are carried from their hints

1. Carry desktop_entry for grouping/application lookup, capped at 128 bytes and rejecting slashes.
   Unknown IDs simply miss and use config fallback.
2. Carry the KDE reply placeholder, capped like a button label; empty means absent.

Both fields are added for existing consumers, not speculative metadata completeness.

## 0102. `textfield` gains `on_cancel`, and Escape gives the field up when it is declared

1. Add ordinary-field on_cancel; it alone does not make an unreadable field focusable.
2. Escape clears the draft, emits a changed empty value when needed, drops focus, then calls cancel.
   An already-empty field still cancels.
3. Without the callback, preserve clear-and-stay behavior.
4. Do not introduce a general key event or bare-surface Escape handling.

Secure-field behavior is unchanged. Live reply cancellation removed the row and released keyboard focus.

## 0103. `applications:open_url(url)`, so a link in a notification body can be opened

1. Add open_url to applications, using detached xdg-open rather than generation-owned process.run.
2. Allow only http, https and mailto. Reject local files and application-specific schemes from
   untrusted notification text unless a concrete future use justifies them.
3. Reject whitespace/control characters and URLs at or above 2048 bytes.
4. Refuse invalid input rather than guess by sanitizing it.

Config decides which affordance calls the action.

## 0104. `text.content` takes styled runs, drawn in the family's own bold and italic faces

1. Accept styled text runs beside strings; reject image spans rather than silently ignore them.
2. Represent runs as byte ranges over joined text so wrapping/elision can rebase styles.
   The ellipsis inherits the replaced character's style.
3. Send only measurement-affecting bold/italic ranges to shaping; color/underline share cached metrics.
4. Resolve actual primary-family variants once; fallbacks remain regular. Missing variants use
   regular on both measure and paint.
5. Track faces, not just files, so collection face indices and weights agree across renderers.
6. Paint styled segments with accumulated measured advances and explicit underlines.
7. Multiply run color by opacity like the node foreground.

Tests cover parser, wrap/elision ranges and shaping/paint width agreement. Live notification
rendering verified bold and link styling; hyperlink activation was still separate.

## 0105. The notification config pass: what the four Rust changes let the cards do

Config-only use of ADR-0100 through ADR-0104.

1. Preserve text styles and inline images; provide URL buttons while glyph hit-testing is absent.
2. Group by desktop ID with app-name fallback and installed metadata; omit transients from history.
3. Order critical, newest, then key for deterministic ties.
4. Derive urgency borders from the group's newest entry.
5. Render action icons where offered.
6. Wire DND to backend sound suppression and config popup filtering, with critical bypass.
7. Suppress popups while locked without marking them seen; expired pings remain retired on unlock.
8. Section and date history rather than showing only relative ages.

X still dismisses, launcher overlap and animation remain unchanged. Live cards verified styles,
metadata, urgency, actions and history buckets.

## 0106. A press on a link's own words opens it: `href` on a run, `on_link` on `text`

1. Carry href on runs and report it through text.on_link; URL-opening policy remains Lua-owned.
2. Share segment splitting with painting and rederive hit geometry through cached shaping.
   Existing measure/paint divergence tests bound the difference to 2%.
3. A hit link wins over ancestor buttons; ordinary words remain transparent to them.
4. Release must match the armed href as well as the paragraph rect.

Keep URL buttons for links elided out of the text. Live checks confirmed link activation without
card dismissal and ordinary body activation beside it.

## 0107. The pointer takes a shape over what it is on: `cursor` on every node, a default in Rust

1. Accept CSS cursor names on every node and reject unknown names.
2. Native defaults follow behavior: links/clickable buttons use pointer, fields text, otherwise arrow.
3. Walk innermost first; at each node explicit cursor overrides its default, not deeper children.
4. Use SCTK ThemedPointer for cursor-shape protocol with XCursor/shm fallback. Send on changes
   and reset on leave because shapes belong to enter serials.

The extra hit walk and link measurement cost were not measured in this pass.

## 0108. A reply's keyboard is on demand, and a plain field keeps its draft while it exists

1. Replies request OnDemand rather than Exclusive; network prompts retain Exclusive with an
   outside-click closer.
2. Keep a plain draft while its node lives, separate from whether it currently receives keys.
   Losing keyboard focus hides the caret but preserves text; node removal/cancel clears it.
3. Suppress ancestor clicks only when the press actually landed on a field, not whenever a draft exists.
4. Request keyboard only for a reply still represented in the feed, not a stale reply ID.
5. Disable card body activation while replying; explicit X still works.

Empty popup space still claimed input/focus; narrowing that region remained deferred.

## 0109. The reply field is always there, the keyboard is asked for on hover, and the input region is what is drawn

1. Always draw available reply fields; remove the extra Reply-button state. Stamp drafts by card
   so another card's Send cannot submit them.
2. Request OnDemand on hover or while a valid draft is pending; the actual click acquires keyboard
   on tested niri. Network prompts remain Exclusive.
3. Recurse through transparent containers when building input regions. Claim painted content and
   intentional invisible click handlers, not empty layout boxes.
4. Disable body activation while a draft is pending, not while an obsolete open flag is set.

Live tests verified empty space no longer takes input, one-click typing and draft preservation.
A focus property alone would not acquire the compositor keyboard and was rejected.

## 0110. A panel is as tall as its content, up to a cap: `max_width`/`max_height`, and the host card centres under its indicator

1. Add numeric max_width/max_height, 0–8192, for content-sized nodes. Capped content leaves a
   real scroll remainder; the decision treated fixed/Fill sizing separately.
2. Let the example host card size to content and cap each list instead of selecting fixed panel heights.
3. Center beneath the triggering indicator, then clamp inside screen edges in Lua.
4. Recompose network/Bluetooth panels from Lua controls. Empty Bluetooth names fall back to MAC,
   because empty strings are truthy.

Hidden-network entry, IP display, sectioning, visibility and codec controls were not all copied;
their omissions stayed explicit config scope, not proof of framework gaps.

## 0111. A flex item's cross-axis minimum is `auto`, because taffy 0.14 adds the container's margin to it

1. Use auto for flex-item cross-axis minimum, zero on the main axis and stacking axes.
   This avoids taffy 0.14 adding the container's margin to an explicit child minimum.
   A standalone reproduction and the anchored notification-card test justify the mapping, not a fork.
2. Add opt-in per-instance layout dumps so session geometry can be compared with test assumptions.

The observed card was measured at its 1521 px left margin rather than its 378 px content width,
underestimating wrapped height by 13.2 px.

## 0112. A launcher's four missing primitives: `autofocus`, `on_navigate`, `scroll:reveal`, and `oblisk set`

1. Autofocus arms an ordinary field on a live focused surface when nothing already owns typing.
   It opens empty; multiple candidates choose document order, unlike secure-target refusal.
2. Navigation callbacks receive up/down/page_up/page_down/tab/backtab without editing the draft.
   Repeats work; this is not a general key handler.
3. Scroll reveal is a one-shot request for minimum movement to show a child, then normal clamping.
   It must not hold selection against subsequent wheel input.
4. Carry application Comment for subtitles/search; localization, Keywords and GenericName stay out.
5. External set/toggle forwards named-state writes to the authoritative generation. Parse JSON
   or use a string; reject undeclared state and nonboolean toggles. No arbitrary function IPC.
6. The example launcher becomes a keyboard-owning layer panel. Calculator/currency copy actions
   wait for clipboard support; web opening already exists.

Same-day amendment:

7. Hover callbacks fire for Motion/Leave, not Enter or layout movement under a resting pointer.
8. Refresh hover signals silently after layout at the remembered pointer position.
9. Every autofocus arm emits an empty change callback, allowing selection/scroll reset even when empty.
10. Two-stage Escape is config policy: clear first, close when empty.

## 0113. What a code review is worth: four fixes out of two hundred findings, and the two that were the review's own doc drift

Verify review claims against behavior rather than counting findings.

1. Link the live pacman local database instead of copying roughly 1500 package directories per
   check. Require a real directory; accept the same concurrent-install window as checkupdates.
2. Let the consuming module configure sysinfo; unused temperatures stay dormant. Periodic update
   checks were initially left to explicit config policy.
3. Widen stubs to actual accepted scalar edges, border colors/signals, list arrays and optional offsets.
4. Correct integer output-scale documentation and remove the config's second division of geometry.

Also fix the starter's nil-before-hydration clock access.

Same-day amendment, retaining the original decision numbers:

11. Run update checks when due, including first startup; reload inside the interval must not
    restart an hour-long delay.
12. Remove direct install from the bar badge. Installation belongs beside a package list and
    deliberate confirmation control, not behind a nil-sensitive count guard.
13. Add manual check, live checking state, exit code, last 200 log lines and failure count.
    Keep privileged install command fixed; interpretation, thresholds and result dismissal are Lua policy.
14. Add scalar system-state writes through temp/rename and allow namespaced keys. Remembered
    checked_at seeds but never overrides fresher checks. Automatic persistence still needed a push hook.
    Later amendment (2026-09-06): the package list seeds beside checked_at under the same rule, and
    is ignored without it. Skipping the first check while showing an empty list read as "up to
    date" for the rest of the hour; the mirror never had the gap because its list lives in the
    persisted state object.
15. Build the update panel in Lua over those facts. No spinner or copy-log without animation/
    clipboard support; reuse the existing action-button pattern.

## 0114. `polkit` joins the roster, and the agent holds its reply until the prompt is answered

1. Put Polkit prompt state and cancel in the roster, retaining secure-target lazy startup.
2. Hold BeginAuthentication's reply until success or cancellation; returning early means failure.
3. Reject concurrent challenges rather than invent a queue before one is needed.
4. Wrong passwords keep the prompt open; PAM owns lockout policy.
5. Use Polkit's setuid helper for authentication/response. The unprivileged lock worker cannot
   call the root-only response method; it remains the lock path only.
6. A submit button sends the scope's armed native secret on release, without exposing it to Lua.
7. Destroyed surfaces clear focus and cannot auto-arm; the compositor owes no leave for them.
8. Clicking non-fields preserves secure focus; another field, leave or unmap still scrubs it.

Masked Escape-to-cancel, multibyte mask support and interactive prompt metadata were not
implemented in this pass.

## 0115. A capability push can run a handler: `on_change`, and the five things it unblocked

1. Capability on_change runs after each pushed value, before layout, with current and previous.
   It may act; derived maps remain pure and rollbackable.
2. Deliver every push; Lua defines thresholds and edge comparisons.
3. Clear handlers before re-evaluation to avoid accumulating registrations. During a topology
   handoff, old and candidate handlers may briefly both fire.
4. Budget each handler at 5 ms; log failure and continue without undoing the received value.
5. Config uses pushes for power notifications/actions, persisted check times and package announcements.
6. Share battery thresholds between pills and notifications.
7. Hold a spurious zero battery reading on mains after a nonzero value; pass genuine draining zero.
8. Drive OSD from state changes, including external commands, not only bar clicks. Lower-priority
   entries drop while a higher one is visible; equal/higher replace it.

Actionable update notifications, timer-based dedupe and OSDs without backend facts remained out of scope.

## 0116. Pointer drags and wheels on a button, and the microphone's volume

1. Buttons receive left-drag start/move/end with local unclamped pointer coordinates. Hold through
   release/leave; field presses do not drag. An inside release may still click after drag end.
2. Wheel callbacks receive vertical fractional notches, positive for increase. The innermost
   wheel handler or scroll container wins, with no chaining.
3. Drag/wheel handlers make invisible button boxes input-active.
4. Add default-source volume/mute and matching actions through the shared device write path.
5. Carry PipeWire device icon hints without resolving them in the Supervisor.
6. Retain volume's 0–1 clamp; no 150% headroom.
7. Implement quantized sliders and device/app controls in Lua, committing held drag values on release.

A brief old-snapshot snap-back remains possible. Microphone OSD and deeper app-icon lookup were
not built; live tests covered the pill and microphone reading.

## 0117. A workspace knows whether it is empty and what runs on it

1. Add populated and one representative app ID per workspace: focused window first, otherwise
   lowest window ID. Empty IDs become absent; keep reduction compositor-neutral.
2. Still no per-workspace window list; a switcher is a separate caller.
3. The example strip collapses on row hover and resolves installed application icons, otherwise
   showing workspace numbers.

A populated window without app ID remains populated. No width animation or opacity fade.

## 0118. `workspaces` speaks Hyprland, as a module behind the same publisher

1. Add a Hyprland module behind the existing publisher and exhaustive dispatch, still no trait.
   It uses documented IPC and synthetic fixtures, not live-verified captures.
2. Re-read workspace/monitor/client/active-window JSON on relevant event-socket lines using direct
   command sockets, not four subprocesses. Coalescing waits for measurement.
3. Regular workspace number is both ID and index. Nonpositive/special IDs were initially omitted.
4. Active/focused follow monitor state; use activewindow rather than stale client focus history.
   Representative app selection uses workspace focus-history order.
5. Share socket-path resolution with keyboard and fix both callers' missing leading dots.

Padding, specials, fullscreen and compositor metadata were deferred to the next payload decision.

## 0119. What one compositor has and the other does not is an absent key

1. Unsupported compositor features are absent keys, not a parallel supports table. Empty means
   supported with no current entries.
2. Specials are top-level, name-keyed, with optional shown-on output; they do not belong to one
   regular output list.
3. Publish compositor name so Lua can choose display policy, such as padding Hyprland slots.
4. Toggle specials by name on Hyprland; niri logs unsupported calls.
5. The example draws specials separately without a dynamic tooltip per entry.

No overview or urgency without a consumer. Keep synthetic display slots out of backend facts.
Hyprland remained documented-IPC-only, not live-tested.

## 0120. A watched folder is a capability, `oblisk.files`

1. Watch requested folders in the Supervisor, not blocking Lua reads or parsed ls output.
2. Key by the requested path with trailing slashes removed; readiness/errors belong to each folder.
3. List one level of nonhidden files, filtered by extensions and sorted case-insensitively.
4. Same-filter watches reuse and replay; changed filters replace. Unwatch aborts and removes the key.
5. Debounce settled writes for 200 ms using CLOSE_WRITE, not chunk-level MODIFY. Self deletion/
   movement reports a final result then stops; reappearance tracking is deferred.

No user-state overload or unnecessary stat fields. Application indexing keeps explicit refresh.

## 0121. A `panel` or `lock` may build its child per output

1. Panel/lock child functions receive the connector at per-instance apply, where output identity is known.
2. Invoke every pass and reconcile their results like list items; named state survives by its key.
3. Nil yields an empty instance.
4. Evaluation probes use the fake connector PROBE.

Reject functions on window/popup, which have no fixed output, and a global output signal with
ambiguous per-instance meaning. Config now implements per-output wallpaper and persistence.

## 0122. Images decode to their box, and off the frame through the thumbnail cache when asked

1. Downscale raster textures to cover their physical box, never upscale storage; include the box
   in every image cache key.
2. Opt-in async uses up to four workers and paints empty until completion. Upload on the GL thread
   and invalidate only lists naming the completed files. Inline remains default for complete first frames.
3. Async work uses/writes the freedesktop thumbnail cache, validating source mtime and URI and
   writing private temp files followed by rename.
4. Enable WebP decoding.

No fail-directory cache, shared thumbnail repository, byte budget or crossfade in this pass.
Do not cache transient inline spools as user thumbnails or add a redundant thumbnail switch.

## 0123. Idle textures have a byte budget, and the allocator's mmap threshold is pinned

1. Evict least-recently-used unshown textures above a 16 MB idle budget. Pin displayed images;
   an oversized working set stays over budget rather than thrashing. Icons are not pinned.
   Six wallpaper changes used 79 instead of 123 MB GPU memory without continued growth.
2. Pin glibc's mmap threshold at 1 MB so freed large decode buffers return to the kernel rather
   than raising the adaptive threshold and stranding future buffers on the heap.
   The measured heap stayed near 23 MB instead of growing to 64 MB.

Do not shrink displayed wallpaper textures, change the Supervisor allocator or hide whole-image
decode peaks. Streaming downscale remains an upgrade if peak memory matters.

## 0124. A hidden subtree is frozen, the loop wakes on an fd, and an idle turn does nothing

1. Freeze hidden subtrees without retiring their identity/geometry. Skip child resolution,
   list expansion and measurement; keep hover clearing correct.
2. Block on Wayland and eventfd instead of a 15 ms timer. Frames, decode results and socket-thread
   termination wake the loop, including Supervisor disconnection.
3. Run focus housekeeping only on turns with relevant work.
4. Use two Supervisor async workers instead of one per CPU; the blocking pool remains separate.

Measured debug idle cost fell 8% to 1.3% of a core and 64 to two wakeups/s; closed surfaces fell
from milliseconds to about 20 µs each. No per-surface dirtiness or streaming image decode without
further evidence.

## 0125. A panel shown in the turn that created it waits for its first configure

A kept but never-shown panel may still lack its first configure when a startup signal reveals it.
Choose AwaitingConfigure versus Mapped from the acknowledged configured size, not object existence.

The old path attached before acknowledgment and killed the Wayland connection. Fourteen clean
boots followed, versus three crashes in twelve beforehand. Leave the downstream EGL panic alone;
the root failure was using an already-dead connection.

## 0126. The release build is the optimisation, and `target-cpu=native` is not

Use the existing release profile before further optimization. On one 1920×1200 output, debug
versus release measured: first frame 799 versus 198 ms, boot CPU 0.91 versus 0.12 s,
picker CPU 0.73 versus 0.13 s, idle 1.6% versus 0.4%, Renderer RSS 82.7 versus 69.4 MB,
Supervisor RSS 38.2 versus 23.3 MB, binaries 238 versus 7 MB each.

Reject target-cpu=native: no measured gain beyond noise, less portable binaries.
malloc_trim returned none of the picker's retained 3.6 MB. No code change; the profile already
enabled LTO, one codegen unit, aborting panics, stripping and overflow checks.

## 0127. The update check hands its pages back, and the rest of the memory is where it should be

Release steady state measured 33.7 MiB Renderer plus 13.3 MiB Supervisor PSS on one output,
within the 50 MiB target; RSS overstated shared Mesa pages.

Trim glibc once after libalpm's blocking check releases its roughly 52 MB parse data.
Supervisor RSS three seconds after checking fell 84 to 32 MB; the legitimate 84 MB peak stays.

Reject forced Lua GC, which recovered only 93 KiB, and arena limiting, about 380 KiB PSS.
Do not shrink the useful thumbnail budget. Wallpaper accounted for 27.5 MB of 41 MB GPU memory;
other retained allocations required profiling rather than guesses.

## 0128. The camera scan runs when a camera opens, not when PipeWire renames one

Allocation profiling found only 3.2–4.7 MiB of live Rust allocations in the Renderer's 16 MB
heap. DHAT required relaxing the CPU cap under emulation and never reached a steady GL frame.

The real fix was Supervisor camera-scan cadence. An fd scan consumed 19.4 MiB of allocations
and 37,364 readlinks at boot; PipeWire name updates were needlessly rerunning it.
Scan device openers on startup/inotify only, then apply name enrichment on either source.

Idle never scanned continuously; this reduces redundant startup/event scans, not an idle leak.

## 0129. Measured against the mirror and against Noctalia, and what their renderer has that this one does not

Historical comparison on the same 1920×1200 machine, both shells running during a 35.5-second
idle window: Quickshell/reference config used 169.0 MB PSS plus 9.8 MB helpers, 214.2 MB GPU,
and 4.65% CPU plus 1.30% cava. Oblisk used 47.5 MB PSS, 51.5 MB GPU and 0.34% CPU.
Not feature-identical: the reference also ran a visualizer and animations.

The inspected native Noctalia renderer provided comparison ideas, not grounds for a rewrite.
Oblisk already shared one context and used a byte-budgeted image cache. Dedicated shaders and
in-process context recovery did not justify replacing femtovg/process recovery without evidence.

Correction from ADR-0130: CachedLayer serves blur/backdrop scratch buffers, not general subtree
caching. Blur has no caller here yet. Damage-region submission remains an unmeasured upgrade.

## 0130. Noctalia read line by line: their animation model, and the two pieces of it this tree already has

Adopt animation's elapsed-time and idle-frame-loop rules, not an implementation in this pass.

1. The inspected animator is a small scalar-setter collection, not a binding/property framework.
2. Derive progress from elapsed time since start, not accumulated callback deltas; sparse startup
   callbacks must not slow the animation.
3. Arm compositor callbacks only while active. Keep the callback chain alive without drawing
   when pixels are unchanged.
4. The inspected declarative plugin layer cannot request arbitrary animations; native widgets own
   them. Oblisk cannot use that shortcut because config authors its UI.
5. Retained node identity and leases already provide the lifetime basis; interpolation must survive
   reconciliation under that identity.

Reject arena limiting and background allocator machinery for gains not supported by our measurements.
Neither compared renderer implemented general damage tracking. Correct ADR-0129's CachedLayer
claim; binary sizes are not comparable without their shared dependencies.

ADR-0143 supersedes decision 5's lease assumption; retained node identity stays.

## 0131. What Noctalia has that is worth taking for memory, CPU and latency, measured

Measured release resolution: median 1.38 ms, p95 3.38 ms, max 5.48 ms over 62 samples.
At a 1.74 ms mean, 60 resolves/s would consume 10.4% of a core before paint.
Animation must interpolate retained state and repaint, not resolve Lua every frame.

Proposed work, not shipped by this entry:

1. Add an opt-in idle profiler with wake/work attribution and spin detection.
2. Use nonblocking EGL swap alongside compositor frame pacing when animation arrives.
3. Virtualize visible list rows plus overscan.
4. Investigate keeping alpha out of text raster keys.
5. Consider bounded shape-memo LRU if the working set outgrows the cap.

Reject unmeasured allocator changes, GL-state tricks absent from the reference, and a whole-run
CPU text cache over an existing GPU glyph atlas. ADR-0132 verifies and revises these proposals.

## 0132. Checking ADR-0131's five items against the tree, and building the two that survived

Verify ADR-0131 rather than treating its survey as implementation authority.

Item 4 is already satisfied: glyph and shaping keys exclude color/alpha.
Item 5 remains unjustified: roughly twenty live text nodes do not warrant per-hit LRU bookkeeping
to avoid an approximately hourly wholesale cache clear.

Item 3 needs viewport virtualization, not just delegate memoization. Fifty tiles measured
0.916 ms versus 0.783 ms with literal children; delegate work is about 19%, the remaining
resolution/layout/measurement about 81%.

Build items 1 and 2: an opt-in idle profiler and swap interval zero per bound surface.
Log a refused swap hint; retain fallback behavior. The profiler touches no clock when disabled.

Live idle windows showed about 17 resolves per ten seconds, explained by clock/CPU/RAM schedules,
not spinning, at roughly 0.24–0.25% CPU. Nonblocking swap alone had no measured idle speed gain.

## 0133. `oblisk.battery` reads UPower uncached, because its wake-up races zbus's cache

1. Disable caching on DisplayDevice reads while keeping one whole-object subscription. Reading
   before cache refresh compared equal, dropped the push and left state one event behind for minutes.
2. Five reads on infrequent changes cost less complexity than five property streams.
3. The power capability's cache-driven property streams remain unchanged (ordered correctly).

Live unplug/replug confirmed the fix. Hardware latency was not the cause.
A similar tray custom-signal/cache risk remained unconfirmed and deliberately unfixed.

## 0134. `oblisk.updates` is a schedule with a package manager behind a trait, and says which one

1. Put manager-specific name, check, install command, progress parsing and reboot detection behind
   a backend trait; the scheduler should not know pacman.
2. Detect executable availability in PATH once at capability start, not distribution branding.
3. Move pacman code and its libalpm allocator cleanup into that backend.
4. Publish optional package_manager so absence is a fact, not a misleading path error.
5. Push initial state even with no backend, when no later scheduler event will arrive.
6. Refuse check/install without a backend; configure quietly no-ops.
7. Show the example indicator whenever supported, and let its idle click check for updates.

One backend exists. Do not invent untestable apt/dnf implementations; the immediate gain is
ownership and explicit unsupported-host behavior.

## 0135. An empty `textfield` shows its placeholder even with the keyboard, because `autofocus` made the alternative unreachable

1. An empty ordinary field shows its placeholder even while focused, matching masked fields, so
   autofocus cannot make the prompt unreachable.
2. With no placeholder, retain the bare caret fallback. Nonempty draft/caret behavior
   stays unchanged.

Reject per-config overlay workarounds and placeholder-plus-caret in identical ink. A distinct
placeholder color is the upgrade path, not hiding search prompts again.

## 0136. Persistence is a JSON file the config names, and the framework names no path

1. Let Lua declare every store's absolute directory, filename and defaults, with no framework-owned
   settings/state split or default file.
2. Reads are signals; writes update/push immediately and save one second after the last edit.
3. A storage capability owns files by joined path across generation swaps.
4. Defaults fill missing keys without overwriting existing values; removed defaults do not delete data.
5. Support nested JSON values; nil deletes.
6. Accept any user-writable absolute path, not a home-directory or extension sandbox.
7. Remove system.state/write_state and their fixed path; system becomes the clock.

Reject machine-written TOML, whose comments would be lost, and a broad FileView clone without
a caller. Protocol/runtime files and interoperable thumbnail locations remain framework-owned.

## 0137. Privacy reports every capture; telling a video from a song stays in Lua

1. Add microphone and screencast user lists beside cameras, sharing one user type.
2. Use the existing PipeWire connection's stream classes, not a second connection.
3. Publish only Running nodes; allocated but inactive browser streams are not capture.
4. Exclude sink-monitor capture from microphones by stream.capture.sink, not app names.
5. Carry all lists through one mixer channel to avoid inconsistent ordering.
6. A missing camera watch must not terminate microphone/screencast reporting.
7. Publish MPRIS URL and desktop entry; config decides whether media is video.

Reject hardcoded video-app lists and unnecessary PipeWire link tracking.
Portal-owned streams may identify the portal, not the app. Direct compositor screencopy is invisible
to this backend; device mute and a running capture stream are distinct facts.

## 0138. `loginctl lock-session` locks the screen; `loginctl unlock-session` does not unlock it

1. Subscribe to this logind session's Lock signal and route it through the guarded lock path.
2. Log/refuse Unlock; authentication remains required.
3. Serialize SetLockedHint updates from confirmed Renderer outcomes, matching the runtime marker.
4. Subscribe at boot, not after config reads.
5. Missing logind degrades with a diagnostic; native shell locking remains available.

Reject making lock-session compliance optional Lua wiring.
Lock-before-suspend delay inhibition is separate work requiring its own fd/window/subscription.

## 0139. A held logind idle inhibitor stops idle events, because Oblisk is the idle daemon

Amends ADR-0032: a session running its own idle daemon must honor logind inhibitors itself.

1. Watch BlockInhibited and match idle as a complete colon-separated token.
2. Suppress threshold forwarding while idle is blocked.
3. Emit sorted Resumed events for previously announced idle thresholds when inhibition arrives.
4. Replay nothing on release; without an idle-state query, wait for the next idle period rather
   than restart thresholds from an invented time.
5. Watch independently of Wayland notify; logind failure leaves the gate open with a diagnostic.
6. Queue registrations arriving before notify becomes live, and clear queued/live entries together on reset.

Initially reject roster promotion until a UI wants foreign-holder state; ADR-0141 later supplies
that consumer. Keep inhibitor polling and unrequested idle-hint policy out.

## 0140. The config's idle module runs one threshold and a clock, and its settings are a modal

Config-side idle policy over ADR-0139.

1. Register one one-second threshold and use the existing clock for editable delays; there is no
   unregister API to safely replace separate threshold registrations.
2. Arm each stage after predecessors report done, timing from that moment. Unlocking unwinds
   later timers; a terminal stage cannot accidentally enable a successor. Validate shared order.
3. Rely on native inhibitor gating/resume instead of duplicating inhibitor guards in Lua.
4. Separate settings/facts from clock-driven actions and centralize inhibition writes.
5. Show AC/battery settings together in a modal rather than squeeze the matrix into a bar panel.
6. Draw equal-width stage chambers from their own armed delay, not a cumulative timeline.
7. Default idle actions off so first launch cannot unexpectedly blank the user's display.
8. Cycle the small timeout option list rather than build a new combo-box control.

Reject ignoring foreign inhibitors and leaking replacement registrations. Fullscreen inhibition
on niri remains unavailable where the backend has no fullscreen fact.

## 0141. `oblisk.idle` joins the roster, because there is idle state worth reading after all

Amends ADR-0032 and ADR-0139: the bar now needs foreign-inhibitor state.

1. Add IdleState with inhibited plus external who/why holders, excluding the shell's own hold.
2. Read ListInhibitors on BlockInhibited changes, not a timer, and publish holder changes even
   when the blocked boolean stays true.
3. Keep bespoke threshold/inhibit methods alongside capability read/change methods; each path
   must request lazy startup.
4. Remove the off-roster dispatch/start exceptions; retain hand-written stub signatures for callbacks.
5. Replay current state on lazy start so a quiet machine does not leave the member nil.

Reject a boolean-only answer, double-reporting local reasons, or a Renderer-local signal for
Supervisor-owned state.

## 0142. The icon spool moves out of `/dev/shm`, which is world-writable

Amends ADR-0031/0033: a UID-named directory under world-writable /dev/shm does not establish
ownership. Precreated symlinks could redirect startup sweeping or PNG writes.

1. Move spools to the user's private runtime directory under oblisk/{subdir}.
2. Fall back only to /run/user/$UID, never /dev/shm. Missing runtime storage degrades to no icon.
3. Keep removal's prefix check based on the same directory helper.

Reject extra shared-directory hardening and mtime sweeping when a private directory solves the
ownership problem directly. This is a cross-user boundary, not a same-UID threat.

## 0143. Removed scene nodes need no lease without a holder

The retirement bag had no production holder, and all three successful transaction paths immediately
drained it. Remove the bag, per-node release protocol and child-first destruction requirement;
ordinary ownership drops unmatched nodes while the independent rollback snapshot preserves the
working scene through admission and budget checks. Supersedes the lease clauses of ADR-0023,
ADR-0045, ADR-0077 and ADR-0130; keep node identity and design deferred retention only when an
actual animation or resource owner needs it.

## 0144. A `text` node names its own font family, because per-glyph fallback cannot choose between two families that both have the glyph

`fonts { ... }` is one ordered chain and the codepoint picks the face, which is the right model
until two installed families carry the same codepoint and draw it differently. A Nerd-Font-patched
body family is exactly that case: `CaskaydiaCove Nerd Font Propo` and `JetBrainsMono Nerd Font Mono`
both cover the private-use icon block, the body family wins fallback every time because it leads the
chain, and its `Propo` icons sit proportionally spaced and fill most of the em where the `Mono` ones
fit one cell. At the same `theme.icon.*` size those are visibly different icons and no chain ordering
reaches the second one.

Add `font = "<family>"` to `text`. The family leads, and the declared chain stays behind it, so CJK
and emoji still resolve under a node that named a display face. `fonts { ... }` keeps its job as the
default and as everyone's fallback tail.

A family name rather than a fixed set of roles. An earlier draft of this decision added a second
declared chain and a `font = "Body" | "Icon"` enum, mirroring the reference QML shell's
`Theme.fontFamily` / `Theme.iconFontFamily` pair. That is the same mechanism with the general case
nailed shut: a config wanting a heading face or a monospaced readout would need a third chain and a
third enum variant, each one more IDL. Naming the family costs no more and the icon case falls out
of it -- the config keeps the pair as `theme.icon_font`, which is where it belonged.

Resolution is lazy and happens once, on the shaping worker, through the same `fc-match` path
`fonts { ... }` already uses, into the same `fontdb::Database`. That is what keeps this from being
the second independent font discovery ADR-0043 decision 2 closed: there is still one resolver, and
paint consumes exactly the face list it produces. The worker bumps a generation counter when the
loaded set changes and `wayland::surface` re-registers the new faces with femtovg before drawing the
list that names them -- one atomic load per frame, and an actual sync only on the few frames where a
family first appears. Both outcomes are memoized, so a family nothing on the system answers costs one
`fc-match` rather than one per measurement.

The family is part of the measurement cache key, because a box measured in one family is not usable
by another.

The name is not validated at parse time. Parsing sees the property, not the loaded font set, and the
set is not fixed at parse time. An unresolvable family draws in the declared chain and says so once
on stderr -- the bargain `fonts { ... }` already makes for an entry nothing answers, and the engine
cannot tell a typo from an uninstalled font anyway. An empty string is refused, since that would read
as "no family named" with nothing to point at.

A named family is never coverage for the declared chain, though the declared family is coverage for
every named one. Plain text must not drift into whichever family some unrelated node happened to
name; a node that named a display font and then drew prose in it should still get glyphs that font
lacks.

## 0145. `animate` is per-property tweening on the retained node, ticked by compositor frame callbacks, never by Lua

QML's `Behavior on width { NumberAnimation { duration; easing.type } }` is what every shell config
reaches for: the reference config has 34 `Behavior on` blocks, on `color`, `opacity`, `width`,
`x`, `border.color` and a few layout sizes, with `InOutQuad` and `OutCubic` at 100-250 ms. That is
implicit animation, and it maps onto this tree's existing structure without a new object model:

1. A node names what eases: `animate = { width = 147, background = { duration = 147, easing =
   "OutCubic" } }`. The table is an ordinary property, so it may be a signal (a direction-dependent
   easing derives the whole table, per § 5.1's nested-signal rule). Numbers, `"NN%"` strings and
   colours only; a percent eases only against another percent, so `"Fill"`, a percent meeting a
   number and an edge table snap, and a property outside the list is refused so a typo fails the
   pass. (Amended the same day: the first cut snapped every percent, which left every meter's
   fill, the mirror's `FillBar`, unanimated.)
2. The tween lives on the `ResolvedNode`, under the identity reconciliation already keeps
   (ADR-0099, ADR-0130 decision 5). `properties` holds the displayed value; each `Tween` holds the
   target. When a pass resolves a target that differs from the retained one, the node starts a
   tween from the value on screen, which is what the retained map holds after the last pass or
   tick, whether that was at rest or mid-flight. An unchanged target keeps the running tween, so
   the whole-scene re-resolve every signal write causes (ADR-0044 decision 2) does not restart
   motion. A first value is taken as it is, as QML does.
3. Between passes, `Scene::tick` advances every tween and lays the instance out again from the
   retained property maps: same parsers, same solver, same frozen-when-hidden rule, but no signal
   read, no item function, no id allocated. This is ADR-0131's "interpolate retained state and
   repaint": a resolve costs 1.4 ms release median before layout, a retained relayout only the
   solver and the memoized measurements.
4. The clock is the compositor's. `paint_surface` requests a `wl_surface.frame` callback before
   the commit only while that surface's tree is mid-tween; the callback sets one flag the poll
   loop takes on its next turn. Nothing is armed when nothing moves, so ADR-0124's timeout-free
   poll stays that way and ADR-0130 decision 3 holds. Progress is elapsed time since the pass
   that started the tween, never accumulated frame deltas (ADR-0130 decision 2). A mid-tween
   surface commits even an unchanged display list, because a frame request is only answered after
   a commit.
5. Overshooting easings (`OutBack`) are clamped into the property's legal range, so a `width`
   easing to `0` never hands the parser a negative. The default easing is `InOutQuad`, the
   reference config's most-used, not QML's `Linear`.

Rejected: an animated signal (`animated(signal, spec)`) whose value tweens. Reading it means a
Lua resolve of the whole scene per frame, the thing ADR-0131 measured out. Rejected: a per-frame
Lua callback (Noctalia's animator) for the same reason and because config authors this UI, not
native widgets (ADR-0130 decision 4). Rejected: a generic timer as the tween clock; frame callbacks
are pacing the compositor already provides.

Not built, each waiting on a consumer: exit animation (`visible = false` removes the node the
same pass; a fade-out needs the node to outlive its `visible` or a config timer, the "exit-resource
lifetime" the roadmap named), looping or indeterminate motion (a running-state model, not a
target), edge-table and per-edge tweens (writing a table back per frame), transforms (no `x`,
`y`, `scale` exist to tween; ADR-0149 added them). One flag ticks the whole scene, so two outputs at different refresh
rates tick every animated surface at the union rate. A tween starts from whichever retained node
ADR-0045 paired the fresh one with, so id-less animated siblings ripple when one is removed; give
them ids or a `list` key. A hidden subtree's tweens are frozen with it and do not count as
animating; the thaw's retarget settles them.

## 0146. Tweens are typed by value shape, enter from `from`, and exit under `delay(signal, ms)`, because the reference config's remaining motion was blocked by names, tables and the lack of a clock

ADR-0145 shipped with a list of property names a tween could carry, snapped every edge table, and
took every first value as it was. Porting the rest of the reference config's 34 `Behavior on`
blocks hit each of those: the notification card's `x` and the panel's `y` are `margin` edges here,
the OSD and panel fade in from nothing, and the close side needs the surface to stay mapped while
the exit runs. Reading Qt Quick's own design against ours (Quickshell adds only an `EasingCurve`
helper and a render-loop hook; the machinery is Qt's) settled what to copy and what to keep away
from.

1. **Type by value, not by name.** Qt registers interpolators per `QVariant` type and a `Behavior`
   can sit on any property. `animate` now names any property the node's own table accepts
   (`lua::nodes::accepts`, so a typo still fails the pass), and `Animatable::from_value` decides
   from the value: number, `"NN%"`, `#` colour, or a table of numeric edges. Two shapes that differ
   snap. The name lists are gone.
2. **Edge tables tween per edge.** `Animatable::Edges([f32; 4])`, absent edges read as `0` the way
   `parse_edge_insets` does, written back as a four-key table per frame. That is the slide-in and
   the drop the reference animates as `x`/`y`, without a transform.
3. **`from` is the entry.** A spec may carry `from = <value>`; a property nothing displayed yet
   starts there. Absent keeps ADR-0145's rule that a first value is taken as it is, which is also
   what QML does with an initial binding, and what the reference has to gate by hand: three
   `enabled: root.settled` guards in `PanelHost.qml` exist only because a `Behavior` fires on
   construction writes too.
4. **`delay(signal, ms)` is the clock, pull-based like every other signal.** A read notes the
   source's new value and its due time, answers the held value, and arms the poll loop's one
   timeout (`DelayDeadline`); a read after the due time adopts it, and a source that reverts sooner
   cancels. The loop stays timeout-free while nothing is pending (ADR-0124), the same way a frame
   callback is requested only while a tween runs. The timeout is the remaining hold rounded up
   to a millisecond: truncating the last fraction gave a zero timeout that came straight back to
   a turn still a few hundred microseconds early, some five hundred times per close on the first
   live run. With it, `visible = linger(open, ms)` keeps a
   surface mapped through its exit tween, and hidden subtrees already keep their content
   (ADR-0124), so nothing is copied. The reference's `PanelHost.qml` builds the same close-hold
   from a `Timer` and six `retained*` properties.

Kept away from, deliberately: a `Behavior` overwrites the item's real property, so every binding
downstream of `width` re-evaluates per frame in the config's language; here the pass's target
stays the truth and only the retained node holds the displayed value, and no Lua runs per frame
(ADR-0131). Qt keeps animating unmapped windows (the reference OSD fades to zero where nobody sees
it); a frozen subtree here does not tick. A per-property `Behavior` object is six lines each; one
table per node is the surface.

Not built, still: a removed `list` item has no node left to ease, so dismissal snaps; `scale` and
`rotate` need a paint-only transform property; sequences and loops (the battery plug flash) need a
running-state model.

## 0147. `geometry(name)` publishes a node's laid-out rect to Lua, written quietly by the pass, because a reveal that slides a card by its own height needs the height

`PanelHost.qml` slides its card from `y = -height`, and `height` is the item's own laid-out size.
Porting that under ADR-0146 used a theme constant sized to the tallest card, and it looked wrong:
a 250 px card travelling 760 px in 147 ms is off screen for the first 60 ms of the open and gone
within two frames of the close. Lua had no way to read what the pass had measured.

1. `geometry(name)` is a signal kind of its own, like `hover`/`scroll`: name-keyed, engine-written,
   `:set()` refused. A node declaring `geometry = geometry(name)` is measured; the pass and a
   tween tick write its absolute `{ x, y, width, height }` after the solve, the space `on_click`
   and `hover_rect` already report in.
2. A tick's write is quiet: a tick that moved the card must not run Lua on every frame
   (ADR-0131). A pass's write that changed a rect earns exactly one follow-up pass (amended the
   same day): the reference morphs a switched panel's `height`, which here means the card's
   height is bound to the section's measurement, and without the follow-up a section that grew
   left the card one pass behind it, its content spilling until some unrelated pass. One and not
   two, so a binding fed by its own measurement stops after a pass instead of spinning the loop.
3. Not a `hover_rect` extension: that rect is written only while the pointer is on the node and
   naming a hover slot makes the node an input region. Measuring must not change hit-testing.

Not built: `width`/`height` bound to a geometry signal of an ancestor is a binding loop the way
QML's is, and nothing prevents it beyond the one-pass lag.

## 0148. `oblisk toggle <name> <value>` sets a state or restores its declared initial, so one keybind opens and closes a modal named by a string

The three modals became one `state("modal", "")` holding the name of the one showing, the
reference's `activeModal`, so two can never stack. That left a keybind with no toggle: `oblisk
toggle` flipped booleans only, and `oblisk set modal '"launcher"'` needs a second binding to close.
`toggle <name> <value>` stores the value unless the state already holds it, and then restores the
initial the config declared, which the state registry already keeps for reload. It is a `set` with
one comparison, the scalar one `literal_was_edited` makes, so `oblisk toggle modal launcher` reads
like the reference's IPC and works for any scalar state. Not built: a toggle between two values
that are both not the initial; that is two bindings, or a boolean.

## 0149. `scale`, `rotate`, `translate` and `origin` are one paint-only affine on every node, because the solver must never see a transform

QML gives every `Item` `scale`, `rotation` and a `transform` list, and the reference config uses
`scale` on launcher rows, wallpaper tiles and the modal card. This tree had nothing of the kind:
a hover zoom had to be a `width` tween, which moves siblings.

1. Four properties on every node, CSS `transform`'s shape rather than QML's three mechanisms:
   `scale` (number or `{ x, y }`), `rotate` (degrees), `translate` (`{ x, y }` px) and `origin`
   (fractions of the box, centre by default). They compose into one matrix about the origin.
2. Paint-only. `LayoutStyle` parses them into `ResolvedNode::transform`; the solver, `geometry`
   and siblings see the untransformed box. `layout::paint` emits the node and its subtree as one
   `Draw::Transformed` group and sets the matrix on the canvas, so text, images and rounded clips
   inside need no knowledge of it. femtovg's scissor follows the matrix, which is what the node's
   own box wants.
3. Hit-testing maps the pointer through the inverse at each transformed node, so a scaled tile is
   clicked where it is painted; input regions take the painted bounds. A zero scale paints
   nothing and takes nothing.
4. They tween through the existing shapes: a number, or an `{ x, y }` table as a second key set of
   the table tween, with an absent axis reading the property's default (`1` for `scale`).

Not done: an ancestor's clip travels with the group, so a scaled child overflowing its parent is
cut by the parent's box scaled with it; nested transforms are not composed into input regions;
skew and 3D. Each is a few lines when a consumer appears.

## 0150. A dropped child with an `animate.exit` block stays as a leaving node until its tweens finish, because a node the tree no longer holds has nothing left to ease

`animate` (ADR-0145) eases a property between two passes, and `from` (ADR-0146) covers the pass a
node first appears in. Removal had no answer: the pass that stops returning a child destroys it,
and the frame after a notification is dismissed simply has one fewer card. `util.linger` only
covers a whole surface whose `visible` source dropped; it cannot hold one row of a list.

1. `animate.exit = { duration, easing, <property> = <target>, ... }`, one spec shared by every
   target in the block, which is QML's `ViewTransition` on `remove`: one transition, several
   properties, run after the model row is gone. It parses on every live pass, so a bad block is
   refused while the node is still there to name in the error.
2. A child reconciliation does not claim stays in its parent as a **leaving node**: kept after the
   live children, out of the solver, holding the rect it was laid out at. Each pass advances its
   tweens, reparses its paint and the pixel `width`/`height` they name, and drops it once nothing
   is in flight. `depart` starts each target from the value the node displays, or from the
   property's identity when it never set one (`1` for `opacity` and `scale`, `0` for the rest), so
   `exit = { opacity = 0 }` fades from opaque without the config having to say `opacity = 1`.
3. Painted and nothing more. `in_flow()` is `visible && !leaving`, and it is what flow
   measurement, hit-testing, input regions, `geometry` writes and autofocus ask, so a card on its
   way out never swallows the click meant for the one that moved up into its place. `contains_node`
   answers the same way: a leaver is back among its parent's children so that it paints, and a held
   draft asking whether the field it was typed into is still in the tree (ADR-0108) would otherwise
   find one the tree has already dropped, keep the keyboard on it, and run its `on_submit` for a
   card that is gone.
4. A leaving node is never paired again. A re-added `id` is a new node beside the one still
   fading, the way QML builds a fresh delegate, rather than a live node yanked back out of its
   exit. A hidden child, or one with no exit block, is gone the pass it is dropped.

Not built: `visible = false` runs no exit (`util.linger` is the surface-level answer, and a hidden
node's subtree is frozen rather than advanced); an exit block below a node that is itself dropped
never runs, because only the child reconciliation stopped at is asked to depart, so a card wrapped
in a row that leaves goes with the row (declare the exit on whatever the tree actually drops);
siblings snap into the gap instead of easing into it, which is a move transition and a second
mechanism; and a leaving node does not reflow, so only
what paint reads moves it (`translate` slides a card out, `margin` eases a number nothing draws),
a `width` in the block resizes the box its subtree is clipped to, and a leaving `text` keeps the
string it was fitted to while its colour and the rest of its paint still move -- the string is what
no pass measures again, which is narrower than freezing the whole of its paint. An absolute-positioned solver pass over the leaver is the upgrade when a
config needs the reflow.

## 0151. The easing set is QML's whole `Easing.Type` list plus CSS's cubic Bezier and steps, because eight curves is a menu and the ninth request is always the one missing

`animate` shipped with eight easings (ADR-0145), the ones the reference config wrote. That is a
menu, and a menu of motion is the wrong shape: a config that wants a drawer to settle with a bit
of weight has to pick the nearest of eight, and the engine has no answer for "not one of these".

1. All thirty-one of QML's names, spelled without the prefix, so `easing.type: Easing.OutBounce`
   still ports by dropping four characters: the four polynomial families (`Quad`, `Cubic`, `Quart`,
   `Quint`), `Sine`, `Expo`, `Circ`, `Back`, `Elastic` and `Bounce`, each with `In`, `Out` and
   `InOut`, beside `Linear`. `InOutQuad` stays the default. The whole list and not the used
   subset, because the list is the port surface: a config author moving a `NumberAnimation` across
   reads the name they already wrote, and a partial list turns that into a runtime error at the
   one moment they have the least context to fix it. Fifteen of these are a name, a table row and
   a one-line arm each, all covered by the one reflection test below; the cost of holding them is
   not the cost of designing them.
2. Most of each family's `In` arm is written out once: `Out` is that arm reflected through the
   centre, and `InOut` is the two halves. `Back` and `Elastic` are the exceptions and are written
   out, because Penner widens their constant for the `InOut` case alone (`s * 1.525`, a period of
   `0.3 * 1.5`) and a reflection of the `In` arm is a visibly different curve. The test pins all
   thirty-one against values computed from Qt's own `QEasingCurve` at a quarter, a half and three
   quarters, not against this module's other arms: checking a reflection against a reflection is a
   tautology, and it passed while both exceptions were wrong.
3. A table where a name goes is CSS's two curves QML has no name for. Four numbers are
   `cubic-bezier(x1, y1, x2, y2)`, solved for `y` at the parameter whose `x` is the progress, which
   is every smooth curve the named list does not hold. `{ steps = n }` is `steps(n, jump-end)`, for
   an indicator that should tick rather than glide. Both are one `easing` value, so nothing else in
   a spec changes shape.
4. Only a Bezier's control `x` are bounded, to `[0, 1]`, the same bound and the same reason CSS
   has: outside it the curve doubles back and one progress has more than one answer. The `y` are
   free, which is what lets a Bezier overshoot the way `OutBack` does.

`dev-config` is unchanged by this: the reference config reaches for seven of the original eight
and `OutBack` once, so every curve it wants already existed. This is framework generality, and the
tests are its only consumer until a config asks for one.

Not built: a named easing cannot be given `Elastic`'s period or `Back`'s overshoot as parameters,
the way QML's `easing.amplitude`/`easing.overshoot` can. A Bezier covers the smooth cases and the
Penner constants are what everyone recognises; a spring (velocity, not a curve of progress) is its
own mechanism and its own decision.

## 0152. `keyframes` walks a property through a list of values and `loops` repeats the walk, gated by nothing but whether the entry is there, because a shell has no way to call `restart()`

Five animations in the reference config are a `SequentialAnimation` or a looped `NumberAnimation`:
two spinners, the power menu's breathing countdown, the battery's plug flash and the lock screen's
shake. `animate` (ADR-0145) could express none of them. It eases one property from what it shows to
what a pass resolved, once.

1. An entry may name `keyframes`, a list of at least two values. The first is where the property
   starts; each later one is a segment eased into over the entry's `duration` and `easing`, or over
   its own when it is written `{ value, duration, easing }`. A segment of no duration is a jump
   rather than a stop, which is QML's `PropertyAction`; a segment between two equal values is a
   hold, which is its `PauseAnimation`. Both fall out of the one shape instead of being two more.
2. `loops` is a count, or `"Infinite"`, and one when absent.
3. A sequence drives its property. It reads nothing the pass resolved for it and eases toward
   nothing, which is what `SequentialAnimation on <property>` does in QML: it takes the property
   over for as long as it runs.
4. There is no `running` flag. The entry's own presence is the gate, and `animate` is already a
   bindable property, so `animate = counting:map(function(on) return on and { ... } or {} end)`
   starts and stops it. That is one mechanism instead of two, and it also answers what the
   reference has to write by hand: a property with no entry falls back to the value the pass
   resolves, which is the mirror's `onRunningChanged: opacity = 1.0`.
5. Phase comes from whole nanoseconds against the frame list, not from elapsed seconds in an
   `f32`. An endless sequence runs as long as the process does, and a 24-bit mantissa is out of
   millisecond resolution after two hours and out of a whole 100 ms cycle after a fortnight, at
   which point a spinner freezes and jumps rather than turning.
6. The run is stateless in the value and stateful only in one bit. Which frame is showing is
   derived from elapsed time against the frame list, so it survives reconciliation with nothing to
   carry, and the same list going round again is the same run. The bit is `Tween::resting`: a
   counted sequence that has played out stays in the list, holding its last frame, so a pass that
   re-resolves for some unrelated signal can tell a finished run from one never started. Without
   it the tween would be dropped on the frame it finished and started over on the next pass.

Not built: a trigger. Three of the five consumers are fired imperatively (`clickFlash.restart()`,
a shake on a failed unlock), and this engine has no imperative call into a node -- a config
describes what is, not what to do. A one-shot therefore runs once per entry, and re-firing it means
the entry going away and coming back across two passes. A signal that pulses would be the smallest
thing that closes this, and it belongs with `delay` rather than here. Also not built: easing
between two *whole sequences*, and a sequence on a property another sequence already drives.

## 0153. `pulse(signal, ms)` is the trigger and `delay` is a spec's lead-in, because the three animations left in the reference config are fired by a call this engine will never have

ADR-0152 shipped the shape of a one-shot and no way to fire one twice. Five reference animations
are sequences; two run forever and need no trigger, and the other three -- the battery's plug
flash, its click flash and the lock screen's shake -- are `restart()` called from a signal handler.
A config here describes what is, not what to do, so there is nothing to call. The gap is not a
missing verb; it is that no signal in the vocabulary says *a change just happened*.

1. **`pulse(signal, ms)` is `delay(signal, ms)` read from the other side.** `delay` answers the
   old value until a change has settled for `ms`; `pulse` answers `true` for `ms` after a change
   and `false` the rest of the time. Both are pull-based, both compare against a value the cell
   remembers, and both arm the poll loop's one timeout, which is now named for the wake rather
   than for `delay` (`WakeDeadline`, `next_wake_deadline`, `take_due_wake`). A change inside an
   open window restarts it rather than extending it, which is what `restart()` does to a running
   `SequentialAnimation`.
2. **A pulse gates an entry; it does not start an animation.** `animate = pulse(clicks,
   ms):map(function(on) return on and { opacity = { ... } } or {} end)` puts a sequence entry in
   the table while the window is open and takes it away after, and ADR-0152 decision 4 already
   made the entry's presence the gate. Firing a one-shot twice is therefore the entry leaving and
   coming back, which is exactly what it always was; the pulse is only what makes those two passes
   happen. Nothing was added to `animate` for this.
3. **The window is the config's business, not the engine's.** A pulse shorter than what it drives
   cuts the sequence off; the engine does not read the animation to size the window, because a
   pulse also gates things that are not animations. `[1, 60000]` ms, the same bound `delay` has:
   the bound is on the whole milliseconds the caller gets, so a window that rounds to none is
   refused rather than accepted and never opened.
4. **A pulse fires on any change, and one edge is a `computed` away.** The reference's
   `onIsPluggedInChanged: if (isPluggedIn)` becomes `computed({ pulse(plugged, ms), plugged },
   function(fired, on) return fired and on end)`. An `edge = "rising"` option would be a second
   mechanism for something the first one already composes into.
5. **`delay` on a spec is the lead-in a sequence cannot express.** A pause *between* two frames is
   a segment of equal values (ADR-0152 decision 1), but a pause *before* the first is not
   expressible, because a sequence starts on frame one by definition. `delay = ms` on any entry
   holds the property still and then runs, which is CSS's `transition-delay` and the
   `PauseAnimation` a QML `SequentialAnimation` needs a wrapper for. It is one saturating
   subtraction in `Tween::progressed`, and both `at` and `done` read it, so the delay is added to
   the tween's life rather than taken out of it. On a sequence it offsets the whole run once,
   loops and all, not each cycle: the phase is measured from the moment the first frame is left.
6. **A delayed tween still asks for frames while it waits.** It repaints its unchanged value up to
   sixty times a second for the length of the hold. Skipping those would mean the frame-callback
   loop knowing which tweens are merely waiting, and one flag already ticks the whole scene
   (ADR-0145): a delay costs no more than the tween beside it that is actually moving.

`dev-config` gains the plug flash, which is the whole mechanism in one place: `pulse` on the
charging state, gated to the rising edge by `computed`, driving a `loops = 2` sequence of
`PropertyAction`/`PauseAnimation` pairs written as zero-duration and equal-value segments. The
click flash and the shake are the same shape over a `state` counter a handler writes and are left
to whoever ports those modules.

`delay` on a spec has no consumer at all, in `dev-config` or in the reference, and this is not
ADR-0151's position restated: those 31 easings finish a closed list QML defines and the reference
already reaches into, while nothing outside this repo asks for a lead-in. It is here for decision
5's reason alone -- ADR-0152 made a pause between two frames writable and a pause before the first
frame unwritable, and that asymmetry is in the vocabulary rather than in any config.

Not built: a pulse that fires on a *predicate* rather than on any change (`computed` covers it), a
pulse of zero width for something that only wants the dirty pass, and a `delay` that differs per
keyframe -- the frames carry their own `duration`, and a zero-value segment in front of the list
is already a per-sequence lead-in a config can write by hand.

## 0154. A `spring` is a third kind of motion, not a thirty-second easing, because the one thing an easing cannot do is keep its speed when the target moves

The reference config contains no `SpringAnimation` and no `SmoothedAnimation`, and the roadmap
row for animation says to decide against a real consumer before adding one. So this is not
completeness, and the case for it is not that QML has springs. It is that `animate` has a
behavioural hole no curve can fill: a tween whose target changes mid-flight starts a fresh curve
from a standstill at whatever value is on screen (ADR-0145's `retarget`), so anything driven by a
signal that moves while the motion runs -- a hover, a drag, a measured geometry -- visibly stops
and restarts. A spring is the mechanism that does not, because its state is a velocity rather
than a position along a curve.

1. **In units of the displacement, not of the property.** The spring works on `s`, the fraction of
   the original displacement still to cross: `1` when the run begins, `0` on the target. The value
   is `to + s * (from - to)`, so one scalar drives a number, a percent, a colour and an edge table
   alike, it feeds the existing `lerp(from, to, t)` with `t = 1 - s` and needs no new interpolation
   path, its overshoot is `t > 1` and is clamped by the property's own range exactly as `OutBack`'s
   already was, and -- the part that actually mattered -- the rest threshold is dimensionless. A
   spring in property units would have needed an epsilon that knows a pixel from an opacity from an
   8-bit colour channel; a thousandth of the displacement needs nothing.
2. **Solved in closed form, not integrated per frame.** `Tween::at` has to stay a pure function of
   elapsed time: a pass and a tick both call it, and ADR-0152 made the running value carry no state
   across reconciliation. Stepping a velocity forward per frame would be a second source of truth,
   would drift with the refresh rate, and would put two outputs at different rates out of step. The
   three regimes -- underdamped, critically damped, overdamped -- are three closed solutions of
   `s'' + damping * s' + stiffness * s = 0`, chosen by the sign of `stiffness - (damping/2)^2`
   against a threshold relative to `stiffness`, because that quantity is in units of stiffness and
   a fixed threshold would call a soft spring critical and a stiff one never.
3. **`stiffness` and `damping`, both required, no `mass`.** Mass divides out of both, so naming it
   would be a third number that only rescales the other two. No defaults either: a spring whose
   constants are implicit cannot be read or tuned. Not QML's `spring`/`damping` scalars -- there is
   no mirror obligation here, because the reference never uses the type.
4. **A spring has no `duration`, and the parser stops requiring one only for it.** `duration`,
   `easing`, `loops` and `keyframes` beside a spring are all refused rather than ignored, which is
   the same rule ADR-0152 applied to `from` beside a sequence: two ways of saying the timing is a
   config bug, not a precedence question. The rule runs the other way too, so a `loops` with no
   `keyframes` to count is refused rather than read by nobody.
5. **Which fields are live is now a type.** `AnimationSpec` carried `duration`, `easing` and
   `sequence` with doc comments saying when each was dead; a third motion made that a three-way
   puzzle, so it is a `Motion` enum -- `Eased { duration, easing }`, `Sequence`, `Spring` -- which
   is what ADR-0152's review asked for and was deferred until this landed. `delay` stays outside
   it, on the spec, because it applies to all three.
6. **`done` comes from a bound on the envelope, not from watching the value.** Each regime bounds
   its solution above by `amplitude * exp(-rate * t)` and inverts that, so the settle time is
   computed once when the spec is parsed and is never early -- late only keeps a tween sitting on
   its target for an extra frame, while early would drop it mid-flight. Capped at sixty seconds so
   that constants approaching no damping cannot ask for frames forever, and `at` is pinned to
   exactly `1` past the settle so the property lands on the value a pass resolved.
7. **The hand-over is a projection.** Both runs read `value = to + s * (from - to)`, so matching
   the value's rate across a retarget gives the new run's starting velocity as the old run's rate
   projected onto the new displacement -- exact for a single number, and for a colour or an edge
   table the closest one scalar comes when the components are not moving in step. It is bounded,
   because a target landing almost where the value already is makes the projection enormous and
   would fling the next run off the screen.

8. **A pass that changes nothing carries the spring, and only the spring.** A spring's `velocity`
   is the rate the last retarget handed it, never a number a config wrote, so re-parsing the entry
   always yields one at rest -- and every unrelated signal in the surface causes a pass. A running
   spring whose two constants still match therefore keeps the spring it is running rather than the
   freshly parsed one. What it does not keep is the entry around it: `delay` is re-read like any
   other field, so editing it still lands on a spring already moving. Editing a constant is a
   config change and takes the new spring at its parsed rest; neither case restarts the run, whose
   target never moved.

Nothing in `dev-config` uses this and the tests are its consumer, the same standing `delay` has
under ADR-0153. The roadmap's animation row says to decide against a real consumer before adding a
spring, that rule was not met here, and it stays in the row unchanged rather than being softened to
fit what was built -- softening it was the first draft of this ADR and was the wrong instinct,
because a gate edited by the change it gates is not a gate. The trade is recorded instead: roughly
two hundred lines solving a second-order equation in three regimes, with a hand-over whose ceiling
is written on `Spring::handed` and no config to catch a regression in it.

Not built: `mass` as a third constant, a spring on a sequence's individual segments, a per-channel
velocity for a colour whose components move apart (the ceiling is written on `Spring::handed`),
and the `SmoothedAnimation` shape, which is a velocity limit rather than a spring and would be a
fourth motion rather than a knob on this one.

Amendment: the spring stays. Asked to rule on the paragraph above, the owner kept it, so the
sentence about taking it back out is no longer a standing intent -- it records only that the code
is separable, which is worth knowing and is not a plan. The gate it was measured against is
untouched and still governs the next addition to that row.

## 0155. The engine's test suite guards no `dev-config` component, because nineteen files nothing ships are sample usage and not product

Six tests in `renderer/src/lua/mod.rs` loaded `dev-config/oblisk` to assert what
`components/panel_card.lua`, `panel_header.lua`, `toggle.lua` and `panel_toggle_card.lua` build
and how their `on_click` filters a button. The subject of every one of them was Lua that lives in
the sample config. ADR-0154's test sweep moved four *engine* tests off that same load and left
these behind, on the reading that fixing them meant building a Lua test runner first.

1. **`share/starter` ships one file, and it is `shell.lua`.** The nineteen files under
   `dev-config/oblisk/components/` are not installed, not referenced by the starter, and not part
   of anything a user of this engine receives. They are how one config is written. A test suite
   that fails when a sample is restyled is measuring the sample.
2. **Every engine contract those six touched is already pinned by a fixture.** The one that is
   not pure Lua is `on_click`'s second argument, and `wayland/input.rs` holds it three times over:
   `on_click_takes_the_button_name_as_a_second_argument_beside_the_rect`,
   `on_clicks_argument_is_the_buttons_rect_as_four_named_fields`, and the `BTN_RIGHT`/`BTN_MIDDLE`
   name mapping. The six called the Lua handler directly from Rust, so they never reached the
   engine's dispatch at all; what they tested was the component's own `if button == "left"`.
3. **Deleted, not moved.** A Lua spec runner cannot be a standalone one: these components need
   `panel`, `text`, `state` and the node builders, which exist only inside the engine's VM, so the
   only host is a new engine subcommand next to `oblisk check`. That is new machinery whose whole
   beneficiary is a sample. `just check` already parses every Lua file and type-checks
   `dev-config` against `lua-meta`, which is what catches a component breaking structurally; the
   rest is caught by running the shell, which is what a sample config is for.
4. **`require_resolves_the_nested_modules_the_shipped_dev_config_actually_splits_out` stays.** Its
   subject really is the shipped tree: whether `?` substitution resolves a dotted `config.theme`
   across directories, which a flat fixture cannot pose. That is the shape ADR-0154's sweep asked
   for -- keep the load when the config is the thing under test.

What this gives up is a guard on four components a live shell exercises daily, and the day any of
them moves into `share/starter` it becomes product and the runner in decision 3 stops being
machinery for a sample. Until then the boundary is that the engine tests the engine.


## 0156. A swap handshake hands back every frame it is not the reader of, because the Candidate asks for its capabilities before it signals ready

On 2026-09-07 a live session locked with no PAM worker behind the lock screen: it rendered, took
keystrokes, and had nothing to authenticate against, which under `ext-session-lock-v1` is a
lockout rather than a failed unlock. Recovery went through ADR-0060's takeover marker.

The Renderer evaluates `shell.lua` before it sends `ReadySignal` (`wayland/mod.rs`'s § 14.2 order:
evaluate, bind, clear, signal), and ADR-0070 decision 1 makes reading `oblisk.<capability>` queue a
`StartCapability`. So a Candidate's starts always reach the Supervisor *ahead of* the `ReadySignal`
the swap handshake is waiting for -- and `SocketCandidateLink::recv_matching` logged and threw away
everything that was not the frame it wanted. Every swap, not only a rushed one.

It stayed invisible because `Capabilities::start` is idempotent and the generation before had
usually started the same names already, so the dropped frame asked for something that existed. The
first read of a name is the one that matters, and at boot that name is `lock`.

1. **Non-handshake frames are deferred, not dropped.** `recv_matching` now sets each one aside in
   arrival order for the caller. The bug was never specific to `StartCapability`: `Command`,
   `SetState`, `LockReport`, `RequestReload` and `ReevaluateReport` went the same way, and
   `main.rs`'s own comment on the stale-`LockReport` arm spells out what losing one of those costs.
   Fixing the shared receiver fixes all of them at once.
2. **`ReadySignal` and `PresentationEvidence` are still dropped.** This link is the only reader
   either one has, so one arriving out of turn is stale or a wire desync, not work owed to anybody.
   Deferring them would hand the main loop a frame whose only handler logs it as exactly that.
3. **They go back through the main loop's `match`, via a queue drained ahead of the socket.** Not
   re-queued onto the inbound channel: it is bounded at `MAX_INBOUND_FRAMES` for flood control, so
   a replay would either await inside a path nothing is draining, or `try_send` and drop again
   under the load where dropping hurts most. A `VecDeque` popped by `next_inbound` before it reads
   the socket keeps the order exact and needs no second handler. The pop is synchronous, so the arm
   stays cancel-safe.
4. **The replay happens whether or not the swap succeeded.** A handshake that times out consumed
   the frames all the same, and the Candidate that sent them may be about to become authoritative.

Not built: an acknowledgement for `StartCapability`. Nothing re-sends one lost to a connection that
dies mid-write, and the renderer-side `started` set means a generation asks exactly once. That is a
narrower hole than this one and wants a real second occurrence before it grows a protocol.

Measured live on 2026-09-07, same machine, same swap (one surface added to `dev-config`), the two
binaries A/B'd: **26 frames dropped before, 0 after.** Twenty-one were `StartCapability` -- every
capability the config uses, `lock` among them -- and five were `Command` envelopes with real work
in them: `storage.open` of the state file, `files.watch` on the wallpaper directory,
`sysinfo.configure`, `idle.register`, and `updates.configure` carrying the whole package list. So
the failure was never the rare race the roadmap recorded. It was total capability loss on every
topology reload, hidden because the outgoing generation's controllers were already built and the
incoming one inherited them.

The roadmap row that recorded this said the drop was silent. It was not -- `recv_matching` printed
a line naming the frame and both generations on every one. What was missing was anyone reading the
log, which is the argument for the frame surviving rather than for a louder message.


## 0157. The layout pass owns the evaluation memo, because a config's shared computed was answering once per property rather than once per pass

ADR-0044 decision 3's ceiling was closed halfway. `EvaluationMemo` collapsed repeats within one
evaluation, and `node::resolve_properties` calls `Signal::get_value` once per property, so one
evaluation was one property of one node. A computed reached by twelve properties across four nodes
ran its body twelve times a pass. `signal.rs` already named widening it "the next rung" and
deferred it for want of a measurement against a real config.

Here is the measurement. Every signal getter in a live `dev-config` shell was timed on
`CLOCK_THREAD_CPUTIME_ID` under 40 spinners on 20 cores, the contention a `cargo build` on this
machine produces:

| | worst getter CPU | getters over 500us in ~20s |
| :--- | ---: | ---: |
| Per-property memo | 1.35 ms | 87 |
| Per-pass memo | 0.57 ms | ~3 |

Against the 5ms cap that is 3.7x headroom becoming 8.8x. The prompt was a real failure: a live
`row.padding` on `components/expanding_pill.lua`'s cell -- a four-link chain through `linger`,
`delay` and `hover` doing no real work -- exceeded 5ms once during a boot under a parallel build,
and the scene kept its prior frame. `row.padding` measured 1.20ms cold, second worst of everything
sampled.

1. **`LayoutPassBudget::enter` opens the memo and its `Drop` closes it.** The scope was already
   expressed as an RAII holder with an `owner` flag, so every `EvaluationMemo::enter` inside a pass
   simply becomes a non-owner and the table outlives it. Four lines, two of them the set and the
   remove. The pass takes the table unconditionally rather than claiming it behind an `owner` flag
   of its own: no `Computed` is running when a pass starts, so there is nothing to displace, and
   both of this budget's fields already assume one live holder -- nesting two would have the inner
   `Drop` clear the outer's deadline too. A flag on one field and not the other would read as
   nesting-safety that is not there.
2. **Outside a pass nothing changes.** Startup evaluation and a `notify_change` handler still let
   the outermost `Computed` own the table, which is what keeps a handler that `:set()`s between its
   own `:get()`s observing its own writes.
3. **The cost is that two mid-pass writers now land a pass later, for every reader rather than
   some.** `layout::scene` writes a `Scroll` cell for the clamp and publishes a `geometry(name)`
   rect, both while resolution is still walking the tree. A derived readout of either used to give
   the pre-write answer above the writer and the post-write answer below it -- a split that
   depended on where in the tree the reader sat and was documented nowhere. Now every reader gets
   the value the pass started with. `LiveSignalHandle::set_quiet`'s own contract already said a
   derived readout sees the clamp next pass, so this makes that sentence true instead of
   approximately true, and `Scene::settle_geometry` already schedules the follow-up pass a moved
   rect needs.

Not built: caching across passes. Nothing here observes a `state` or `Live` cell changing between
two passes, so the Watcher stays the thing that decides when a value is stale, and an invalidation
graph is a different design rather than a wider scope.


## 0158. The Renderer says when it forgot its idle thresholds, because the Supervisor was clearing them after the replacements arrived

`Loader::evaluate_named` drops every local threshold callback before each evaluation, and the
config re-registers what the new tree wants. The Supervisor cleared its own fan-out separately,
from `answer_unchanged_report`. Both halves were right. Their order was not.

One reload, in the order the frames move:

1. Supervisor sends `Reevaluate(sequence)`.
2. Renderer forgets its callbacks, runs the config, and `register_threshold` puts an
   `idle`/`register` command on the socket.
3. Renderer replies `ReevaluateReport::Unchanged`.
4. Supervisor calls `reset_registrations`, deleting the entry step 2 just made.

After any in-place reload the config held callbacks nothing could reach.
`cleanup_generation_thresholds` had emptied the generation's fan-out list, so
`spawn_idle_event_forwarder` expanded each `idled`/`resumed` into no events. Confirmed live by
printing from the config's own callbacks: across 22 minutes of ordinary use, with no inhibitor
held, `on_idle` and `on_resume` ran zero times.

The cost is not a missing feature. `modules/global/idle.lua` zeroes `idle.since` and clears the
armed stamp in `on_resume`, so a lost resume leaves the stage counting from the first time the seat
went idle, ignoring every keystroke since. A five-minute lock fires five minutes after that first
idle whatever the user does. `power-off-monitors` changes `wl_output`, the Renderer answers with
`RequestReload`, and that reload breaks the next one. Which is why the reported symptom began after
a blank.

1. **`IdleRegistry::forget_thresholds` sends `idle`/`forget_thresholds`.** The clear and the command
   are one act, riding this generation's one ordered socket, so the new tree's registrations land
   behind it. Silent when nothing was registered, so a config with no threshold does not start
   `idle` by way of the reload path.

   One exception to that order: `replay_pending_registrations` takes the queued registrations into a
   local vector before installing them. A forget arriving in that window clears the queue and the
   fan-out, and the replay reinstalls what it already took. Reachable only before notify goes live.

2. **Both threshold arms of `dispatch` run inline.** `tokio::spawn` on each would let two tasks
   apply a forget and a register in either order, the failure this pair exists to stop. Neither ever
   awaited: the fan-out is a `std::sync::Mutex` and the Wayland calls are synchronous. The
   `Inert`/`Live` holder becomes a `std::sync::RwLock` and both methods say `fn`. Inhibit stays
   async and spawned, because it makes a real D-Bus call.
3. **`answer_unchanged_report` resets nothing.** It had also been zeroing the generation's inhibit
   counts and closing the logind fd, while the VM that reload keeps alive still held its own record
   of that hold. The shell dropped a `microphone` inhibitor on the first reload and, believing it
   still held one, never took it again. An in-place reload keeps the generation, and only its
   thresholds were explicitly forgotten.
4. **`reset_registrations` moves to the reap, where a generation really is gone.** It had no other
   caller once decision 3 landed, and the swap path had never called it: a superseded generation
   kept its fan-out entry for the life of the Supervisor, and every idle transition after a swap
   logged a push to a generation with no connection. That was the roadmap's *Idle registrations
   outlive their generation* row.
5. **`forget_thresholds` is excluded from the generated `invoke` union.** It is an `IdleAction`
   because it crosses the socket as an ordinary command, but a config calling it would silently
   unregister its own thresholds. `stubs::internal_actions` names it, and a test holds the union to
   the three a config may call.

A doc comment on the new variant would have been the third bug here. schemars emits a flat `enum`
for a plain unit enum and a `oneOf` once any variant is described, `stubs::action_names` reads only
the flat form, and one `///` emptied `IdleCapability`'s whole `invoke` union without failing
anything but the golden test.

Not built: an acknowledgement for a registration. A lost `register` is still lost, which is the
roadmap's *Capability start acknowledgement* row for a different frame.


## 0159. Re-arming the idle listeners on release was tried and reverted, on evidence that turned out to be something else

`IdleGate::observe` returns before recording, so a seat that goes idle during a logind block is not
in the `idled` set. There is nothing for `set_blocked(false)` to replay and no later event to
expect, because a notification that has sent `idled` never sends it twice. That hole is real, and
ADR-0139 decision 4's `ponytail:` calls it "still wrong, but safe".

`notify::rearm_listeners` was written against it: destroy each live `ext_idle_notification_v1` on
release and create a fresh one. It was reverted the same hour. Both the reason for writing it and
the reason for reverting it were wrong.

1. **The symptom that prompted it was not this hole.** A countdown that would not start after a
   logind hold was dropped was read as the replay gap. ADR-0160 measured the same machine directly:
   a browser held a Wayland surface idle inhibitor, so the compositor was withholding `idled` from
   every gated listener, block or no block.
2. **The symptom that prompted the revert was not the rearm.** Idle went silent after it ran and a
   restart was needed, which read as a destroyed listener the process could not rebuild. Three
   Supervisors were running against one config directory by then, each truncating the same log the
   diagnosis was read from, and the Wayland inhibitor was up throughout.
3. **Reverted and left reverted**, not because it is known bad but because nothing here was
   measured. A release at one log line and the lock stage firing two lines later looked like
   evidence that a compositor resolves a fresh notification against last input rather than creation
   time. It is not: adjacent log lines carry no timestamps, and the stage's own deadline could have
   arrived anyway. That question is open.
4. **The hole goes to the roadmap** as what it is, a code-level gap with no demonstrated live cost,
   since every symptom attributed to it has been explained.

The method failure is worth more than the fix. Three diagnoses were stated as settled on
correlation, each after a change that appeared to fix a symptom that moved on its own. What ended it
was instrumenting the layer that tells "not being told" from "told and dropped": the Wayland event
handler, the first layer and the last one reached.


## 0160. `oblisk.idle` reports that the compositor is withholding idle notifications, because nothing else can see a surface inhibitor

ADR-0141 put foreign logind inhibitors in `IdleState` so the shell would stop claiming nothing held
the session awake. It saw half the holders. `zwp_idle_inhibitor_v1` is a surface-scoped Wayland
inhibitor, what a browser takes for a video call, and while one is up the compositor withholds
`idled` from every `get_idle_notification` listener. logind's `BlockInhibited` never mentions it, so
`inhibited` read false, the widget said "nothing is holding this awake", and the countdown sat at
zero with no explanation anywhere.

Measured during a Google Meet call in Zen: zero `idled` events reached the Supervisor's Wayland
handler over four minutes of an untouched seat, with the logind gate open and the threshold
registered.

Quickshell solves the same protocol problem one layer up. `IdleMonitor` carries a
`respectInhibitors` property, and `idle_notify/proto.cpp` picks `get_idle_notification` or
`get_input_idle_notification` from it, leaving any comparison to the config. It exposes the pair.
This derives one answer from it.

1. **Bind `ext_idle_notifier_v1` at `1..=2` and pair every listener.** Version 2 adds
   `get_input_idle_notification`, which the compositor may not withhold. A duration has two
   listeners, told apart by `ListenerId`. Input events never fan out to a config. They exist so
   silence on the gated listener reads as evidence rather than as a seat in use.
2. **The claim is "the compositor is withholding notifications", not "an application holds a
   surface inhibitor".** The observation does not establish the cause: niri folds its freedesktop
   screensaver inhibition into the same flag, sway adds configured focus and fullscreen policy, and
   the protocol lets an ordinary notification weigh inputs the twin does not. The published holder
   has an empty `who`, since no protocol names one, and a `why` stating the observation.
3. **Only the shortest fired threshold votes.** Any-of is the obvious rule and it is wrong.
   Releasing an inhibitor restarts the gated timers, so a config idle at 1s and 300s gets its 1s
   listener back a second later and its 300s one five minutes later, and any-of reads that gap as a
   still-held inhibitor for the whole five minutes. The first version shipped that with a test
   asserting it as correct.
4. **A `Resumed` clears both halves of its pair.** Clearing only the reporting half made the answer
   depend on read order: a gated `Resumed` alone leaves the input half idle, which reads as a held
   inhibitor, and the input `Resumed` behind it is no evidence and preserves it. Every wake could
   latch a false holder for as long as the seat stayed busy.
5. **No evidence is not evidence of nothing held.** The divergence exists only while the seat is
   idle, so `wayland_inhibited` answers `Option<bool>` and `PublishedIdle` keeps its last value
   through an active seat. The published answer has no staleness bound: a seat in continuous use
   holds it indefinitely.
6. **`PublishedIdle` merges the two sources**, and holds its lock across the send. Different tasks
   watch logind and the compositor, and releasing between settle and send let the other writer
   overtake, so the older payload arrived last with `last_sent` already past it.
7. **A version 1 compositor degrades to the logind-only answer** rather than failing to bind.
8. **A generation appears once per duration in the fan-out.** That list is destinations, and the
   Renderer already runs every callback it holds at a duration for each event, so a second entry ran
   every callback again: two `register_threshold(300, ...)` calls fired four times. Each half had a
   passing test. Only the pair was wrong.

Draining the raw channel before answering narrows the window where one half of a pair is read
without the other. It does not close it, so the published answer can still carry an ordering
artifact and not only a stale reading.

Not built: naming the holder, or asking whether one is held while the seat is in use. Neither exists
in any protocol. A dedicated zero-timeout detector would answer live rather than one threshold late,
and `timeout: 0` is explicitly valid. It is deferred because the always-idle behaviour it needs is
compositor-specific, matching Hyprland while Smithay reinserts a timer, and the wakeup cost was
never measured.


## 0161. The PAM worker is reached through `/proc/self/exe`, because reading that link strands a locked session

`spawn_worker_and_exchange` re-execs this binary to run PAM off tokio (ADR-0028). It resolved the
path with `current_exe()`, which *reads* the magic link into a pathname. Once the binary on disk is
replaced, the kernel appends " (deleted)" to that pathname, nothing exists at it, and the spawn
fails with `ENOENT`. The lock screen then shows "could not start authentication" and the session
cannot be unlocked at all.

Seen twice on 2026-09-07, the second time with the session locked and the user on a TTY. Confirmed
after the fact:

```
/proc/3342256/exe -> /mnt/Work/0Coding/1Rust/oblisk-shell/target/debug/oblisk (deleted)
```

A `cargo build` did it here. `pacman -Syu` over a locked session does the same thing to an installed
`oblisk`, which is the ordinary case rather than an exotic one.

1. **Execute the link, do not read it.** `SELF_EXE` is the literal `/proc/self/exe`. The kernel
   follows it to the inode this process already pins, which Linux supports after unlinking, so the
   worker starts from the same code the running Supervisor is. One line.
2. **`renderer_binary_path` is not affected and is unchanged.** It calls `with_file_name`, which
   replaces the whole " (deleted)" filename with `oblisk-renderer` and yields a real sibling path.
3. **Rejected: an `O_PATH` fd pinned at startup, exec'd with `execveat`.** It works, and it is what
   this needs only if procfs itself becomes unreachable. Against an ordinary upgrade it buys the
   same survival for more machinery at the exec boundary.
4. **Rejected: a long-lived worker started at boot.** It adds supervision, restart, and per-request
   state, and a crash reintroduces the exec problem it was meant to avoid. A transaction per attempt
   is right regardless.
5. **Rejected: caching or re-resolving an install path.** It selects new worker code for an old
   Supervisor's protocol, fails during the replacement gap, and re-resolution is a TOCTOU: a
   writable install directory would let replacement code receive the password.

Pinning the executable does not pin the whole PAM stack. A fresh exec loads shared libraries through
the ELF interpreter, so libpam and its modules come from whatever is installed now. The Supervisor
holding the unlock decision is already the old code; upgrading its file never patched it. Security
fixes apply at a controlled restart after unlocking, and this only guarantees there is a way to
unlock.

The failure path is fixed alongside it, because a worker can still fail to start for other reasons.
`apply` already held the lock and cleared `authenticating`, so a retry was allowed; it also counted
every failure as an attempt, and now counts only `AuthFailed` and `MaxTries`. A config drawing a
limit from `attempts` would otherwise have shut the user out for something they cannot answer.
`StartFailed` names the repair rather than only the error, and `dev-config`'s lock status wraps: at
380px it had clipped to "could not start authentication: pam worker f".

Not fixed: an upgrade that removes the loader or a library the old executable needs still stops the
worker, and no exec strategy survives that.

## 0162. The network panel's missing facts are capability gaps, not config workarounds

`NetworkPanel.qml` draws six things `oblisk.network` cannot answer. The config currently fakes two
of them and drops four. Recording the list so the fakes are removed when the capability grows,
rather than hardened into config idiom.

`NetworkState` carries `available_networks`, `connect_error`, `connected`, `connecting_ssid`,
`ethernet_enabled`, `networking_enabled`, `password_ssid`, `scanning`, `ssid`, `strength` and
`wifi_enabled`; `AccessPointInfo` carries `active`, `band`, `secure`, `ssid` and `strength`;
`invoke` accepts `set_networking_enabled`, `set_wifi_enabled`, `set_ethernet_enabled`, `scan`,
`connect`, `cancel_connect` and `forget`.

1. **`AccessPointInfo.saved`.** The mirror shows forget only on a known network
   (`network.known`). Without it every row offers to forget a network NetworkManager has no profile
   for, which is a no-op the user cannot predict.
2. **A `disconnect` command.** The mirror separates leaving a network from deleting its profile.
   The config offers only `forget`, so the sole way to drop a link also destroys the credentials.
3. **`NetworkState.link_type`.** The mirror reads `linkType`; the config infers wired from the
   `ssid == "Ethernet"` sentinel, which is a display string doing a type's job and breaks against a
   real SSID of that name.
4. **`NetworkState.ip_address`.** The mirror's wi-fi tile shows the address. The config shows
   strength again, which the glyph beside it already says.
5. **Ethernet `speed`.** The mirror's wired tile shows the negotiated rate; the config shows no
   detail.
6. **`ethernet_interface` and a `ready` flag.** The mirror disables the wired tile with no
   interface and says "Unavailable"; the config cannot tell absent hardware from a disabled radio
   and says "off" for both.

1 and 2 are the pair worth doing first: together they are the difference between a panel that can
manage saved networks and one that can only join. 4, 5 and 6 are readouts, and 3 removes a
workaround rather than adding a feature.

Rejected: deriving any of these in config from what already arrives. `saved` is not implied by
`active` or `strength`, and the wired sentinel is the existing attempt at deriving 3 — it is the
bug, not the pattern to extend. NetworkManager holds every one of these facts already (ADR-0037's
reasoning for `password_ssid`: what NetworkManager knows does not belong in config).

Rejected: one `network_details` blob added at once. Each field has an independent consumer, and the
panel drift they cause is separately visible, so they can land one at a time.

## 0163. `PolkitState` cannot describe polkitd's prompt, so the dialog hardcodes it

`PolkitDialog.qml` draws three things `oblisk.polkit` cannot answer, all of them properties of the
authentication request rather than of our dialog. `PolkitState` carries `action_id`, `active`,
`authenticating`, `error`, `icon_name` and `message`; `invoke` accepts `authenticate` and `cancel`.

1. **`input_prompt`.** The mirror draws polkitd's own prompt string and hides the line when it is
   empty (`inputPrompt`, `visible: text !== ""`). The config prints a fixed "Password:", which is
   what pam_unix asks for and nothing else. A fingerprint or one-time-code module asks a different
   question and would be labelled wrongly.
2. **`response_visible`.** polkitd says whether the answer should echo. The mirror switches
   `echoMode` on it; the config always masks, so a prompt whose answer is not secret is still typed
   blind.
3. **Whether the field holds text.** The mirror disables Authenticate until the field is non-empty
   (`passwordField.text.length > 0`). `textfield` keeps its content in a native buffer no callback
   can read (ADR-0092's reason the mask stays server-side), so the button is always live and an
   empty submit costs a PAM round trip.

1 is the one worth doing: it is a string already in hand at the agent boundary, and without it the
dialog can only ever serve password authentication. 2 rides along with it from the same message. 3
is not a payload field but a `textfield` question, and answering it means giving the config a way
to observe a buffer that is deliberately opaque; if it is ever wanted, an `empty` boolean signal is
the smallest thing that does not leak the text.

Rejected: reusing `message` as the prompt. It is the sentence explaining why authorization is
needed, drawn above; polkitd sends both, and collapsing them loses the one the field is labelled
with.

## 0164. `PlayerState` describes the track but not what the player will accept

`MediaPanel.qml` greys each transport control from a capability flag and offers a stop button.
`oblisk.mpris` answers neither, so `modules/bar/panels/media_panel.lua` draws every control live and
omits stop. Recording the list rather than faking the flags, which would mean guessing from
`play_state` what only the player knows.

`PlayerState` carries `album_art_path`, `artist`, `desktop_entry`, `id`, `identity`, `length`,
`play_state`, `position`, `position_updated_at`, `title` and `url`; `invoke` accepts
`control(id, command)` for `play`, `pause`, `play_pause`, `next` and `previous`, plus `seek` and
`seek_relative`.

1. **`can_go_next`, `can_go_previous`, `can_seek`, `can_control`.** MPRIS publishes all four. The
   mirror disables the matching control; the config cannot, so a radio stream shows a next button
   that does nothing. `can_seek` is the one that misleads most, because the seek bar is draggable
   and the drag is silently discarded.
2. **`stop`.** `controller.rs`'s `VALID_COMMANDS` has five entries and stop is not among them,
   though its own test uses `"stop"` as the invalid case. MPRIS `Stop` differs from `Pause`: it
   releases the track rather than holding a position.
3. **`album`.** The mirror's second line falls back title → artist → album → identity. Without
   album, a classical track whose artist tag is empty drops straight to the player's name.
4. **A monotonic clock, or a pushed position while playing.** `position` is valid only at
   `position_updated_at`, which is `CLOCK_MONOTONIC`, and no Lua global reads that clock. The panel
   anchors each push against `os.time()` in an `on_change` handler and adds elapsed seconds, so a
   clock adjustment during playback skews the bar until the next push. Either a monotonic reading
   beside `oblisk.system.time` or a position pushed on a cadence while `Playing` removes the
   workaround; the clock is the smaller and serves anything else timing a duration.

1 is the one worth doing first: it is four booleans already on the bus, and without them three of
the six controls are decorative on some players. 4 is next because it is a general capability, not
an mpris one.

Rejected: inferring 1 from `play_state`. Whether a player can seek is independent of whether it is
playing; a paused local file seeks and a playing stream does not.

Rejected: computing elapsed time from `os.clock()`. It returns CPU seconds for this process, which
stops advancing whenever the shell is idle -- precisely when a track is playing and nothing is being
drawn.

## 0165. `Position` is read twice around a state change, and never fabricated

Two changes to `capabilities/mpris/player.rs`, both from watching a browser drive the media panel.

**A failed read keeps the last reading, with its timestamp.** `resync` did
`player.position().await.unwrap_or(0)`, turning "the player did not answer" into "the track is at
the start" -- the thing ADR-0036 forbids, and which the `PlaybackStatus` and `Metadata` arms beside
it already avoid by keeping their previous values. The timestamp travels with the value now: a
stale position under a fresh `position_updated_at` tells a client extrapolating from the pair that
the track jumped backwards, which is worse than either half alone. `-1` when nothing has ever been
read, matching `length`.

**A `PlaybackStatus` change schedules a second read 100ms later.** Several players update `Position`
at an indeterminate time *after* they publish the new state, so the read taken while handling that
signal returns whatever they held mid-transition -- for Firefox, sometimes zero. Quickshell hit the
same players and answers it the same way (`MprisPlayer::onPlaybackStatusUpdated` requests the
property, then requests it again on a 100ms `singleShot`, commented for YouTube). One late re-read
in the forwarder's `select!` is the same remedy without a polling loop.

Rejected: polling `Position` while playing. It is a D-Bus round trip per tick for a number clients
can extrapolate, and ADR-0036's whole point is that the pair of value and timestamp is enough. The
recheck fires on a transition, not on a cadence.

Rejected: filtering the zero in config. `dev-config` did carry that workaround while this was being
diagnosed, and it needed a track identity to tell a bogus zero from a track legitimately starting at
zero -- reconstructing in Lua what the Supervisor already knows. What the player said belongs where
the player is read.

## 0166. What the player says about its own position is not evidence

Extends ADR-0165, which was written before the player was measured. Driving Firefox directly over
D-Bus: seek to 340s, then read `Position` back as 340s, `0`, `0`, and finally the true 363s, with
`PlaybackStatus` `Playing` throughout. `Rate` reads `0`. Some tracks publish no `mpris:length` at
all. The zero is transient and recovers after seconds, not after the 100ms ADR-0165 waits.

**A `Position` of zero on a track we were already inside is discarded** (`resolve_position`). It is
the player not having recomputed, and publishing it restarts every progress bar at the beginning
while the video plays on. A zero on a *new* track is published, because that is where one begins.

**A failed read keeps the previous reading only for the same track.** ADR-0165 kept it
unconditionally, so a 30-second track could inherit the previous track's 5:40 and draw past its own
end.

**`seek_relative` sends the offset to the player** (MPRIS `Seek`) instead of reading a position and
converting to `SetPosition`. That read is the one path `resolve_position` does not cover, so "forward
five seconds" from 5:40 became `SetPosition(5s)` -- a jump to the start. Where an absolute target
must still be converted, for a player with no usable trackid, an unavailable position now refuses
the command rather than standing in as zero, and the subtraction is checked.

**The 100ms recheck is disarmed only by its own timer.** Clearing it on any event let a `Metadata`
change 20ms later cancel the correction, which is the case it exists for.

Rejected: filtering the zero in config. It needs a track identity to tell a bogus zero from a track
legitimately starting at zero, which reconstructs in Lua what the Supervisor already knows. What the
player said belongs where the player is read.

Rejected: polling `Position`. A round trip per tick for a number clients extrapolate. The recheck
fires on a transition, not a cadence.

## 0167. A signal read inside a `:map` is not a dependency

`modules/bar/panels/media_panel.lua` picked its player with `chosen:get()` inside every map, so the
switch-player button changed what those maps would answer without changing anything they declared.
Nothing re-evaluated; the panel kept drawing the previous player until something unrelated moved.

One `computed({ oblisk.mpris, chosen }, ...)` now resolves the player, and every reader takes that
signal. `false` is the no-player value, because a `computed` yielding `nil` has no value to hold.

The rule generalises: `:get()` inside a `:map` or `computed` callback reads a value the graph does
not know was read. It is correct only for something that cannot change while the map is alive.
Callbacks -- `on_click`, `on_commit`, `on_change` -- may read freely; they are not re-evaluated.

`just types` cannot see this, and neither can `luac -p` or `oblisk check`: the code is valid and the
scene resolves. It shows up only as a control that does nothing.

## 0168. Chromium's tray object lives on one of its several connections

ADR-0072 decision 1 keeps a tray item's registered name as the message destination, explaining it as
"Chromium dispatches property reads on the message's destination field, not the owner". That
explanation is wrong. The decision is right and stays.

Measured against Slack 699047, varying destination and object path independently:

| destination | object path | `Get Id` |
| --- | --- | --- |
| `org.freedesktop.StatusNotifierItem-699047-1` | `/StatusNotifierItem` | fails |
| `org.freedesktop.StatusNotifierItem-699047-1` | `/StatusNotifierItem/1` | answers |
| `:1.2656` (its owner) | `/StatusNotifierItem` | fails |
| `:1.2656` (its owner) | `/StatusNotifierItem/1` | answers |

The destination does not matter; the path does. What does matter is *which connection*: Slack holds
two, `:1.2655` and `:1.2656`, and only the one owning the well-known name exports the object. The
other answers "Object does not exist" at every path.

So addressing the registered name is still the right call, for a different reason than recorded: the
bus routes it to whichever connection owns it, and we never have to be right about which of a
process's connections that is. ADR-0072's accepted risk -- a well-known name moving owners between
lookup and read -- is smaller than the risk it avoids.

This also explains the September observation behind ADR-0072, where reads addressed to the owner
`:1.659` failed. That was Chromium's other connection, not evidence about destination fields.

Rejected: collapsing `ResolvedRegistration` to a single name (~35 lines). The split is load-bearing.

Rejected: keeping `DEFAULT_ITEM_OBJECT_PATH` as the well-known branch's path. Slack's object is at
`/StatusNotifierItem/1`, so that default was never going to answer; the registration-string split
above supplies the real path, and the default now applies only to a `service` that names none.

## 0169. A lockout leaves evidence, or it is not a diagnosis

On 2026-09-08 a lock screen refused a correct password with `pam worker failed: i/o error: early
eof`. The worker had exited without writing its outcome frame, and that was the entire record:
nothing on its inherited stderr, no coredump, nothing in the journal. The account was untouched --
`faillock` empty, the previous acquisition a `Success` minutes earlier -- and the session came back
only because a later attempt happened to work. The cause is still unknown.

1. A failed exchange reports how the worker died. `reap_process_group` already collects the exit
   status and `exchange_over` was dropping it; a signal number or an exit code now rides on the
   error. It cannot say why, but it separates "killed" from "returned non-zero", which is the fork
   the next occurrence turns on.

2. Layout errors carry the walk that reached the node: `column[0] > row[1] > text[1] > ...`,
   accumulated as the error unwinds. The surface name alone (ADR-0024) named a lock screen holding
   a dozen `text` nodes and distinguished none of them.

Correction: a `:map` returning `nil` does *not* reach the engine as `Integer(0)`.
`resolve_properties` skips a nil-resolving signal and the property is simply absent, which is
ADR-0044 decision 1 working as written; a probe config confirms it renders empty and raises
nothing. The `Integer(0)` values seen are real zeroes -- a `delay`'s pre-change identity is `0`,
which is the `util.linger` bug -- so a signal that has not produced a value yet is the shape to
suspect, not a nil return. One comment asserting the wrong mechanism was removed.

Not done: one bad property still discards the whole re-resolve, which is what froze the lock screen
mid-authentication. Partial application is a larger decision than this incident settles.

## 0170. A memo key is an identity, not an address

The lock screen's keyboard label and its wallpaper path both resolved to `Integer(0)` on
2026-09-08, from two Lua functions that cannot return an integer: one answers a string, `"--"` or
`"!"`, the other a path or a default. ADR-0169's walk located the nodes; it could not explain them,
and the guess recorded there -- a signal that had not produced a value yet -- was wrong.

`EvaluationMemo` (ADR-0157) keyed each computed on `Rc::as_ptr(deps)`, reasoning that `computed()`
and `Signal::mapped` each allocate a fresh `Rc<Vec<Signal>>`, so the address separates every
distinct computed. It does, until one is dropped: the allocator hands the next same-sized
allocation the address just freed, the memo holds a raw pointer and so keeps nothing alive, and the
dead computed's value is served to the live one that landed on its grave. The memo's scope is a
whole layout pass, and a pass builds and discards computeds continuously -- every `:map` inside a
`list`'s `itemfn`, every one in a surface that rebuilds its tree, which is what a lock surface does
per output on every resolve. Eight lines of Lua reproduce it: read a computed returning `0`, drop
it, collect, build another returning a string, and the string comes back as `Integer(0)`.

The key is now a counter handed out at construction and copied by `Signal::clone`, because a clone
is the same computed and must share the entry. A counter cannot be recycled.

This was never confined to the lock screen. Any pass that frees a computed could serve any other
computed a stale value of any type, silently and only sometimes; a wrong colour or a wrong number
would have drawn without complaint. It surfaced here only because the two victims were typed
properties that refused an integer, and because ADR-0169 had just taught the error to name them.

## 0171. Adoption tries the paths items actually use

ADR-0073 adopts items already on the bus by walking well-known
`org.{kde,freedesktop}.StatusNotifierItem-PID-N` names, and it has no
`RegisterStatusNotifierItem` argument to read, so it guessed ADR-0031's default object path and
stopped there. That is the wrong path for every Chromium application: Slack exports at
`/StatusNotifierItem/1`. Until ADR-0168's liveness probe the guess produced a blank item rather
than nothing, so the gap surfaced as a duplicate-key freeze in the bar instead of as the missing
icon it always was -- and once the probe started refusing an object that answers nothing, Slack and
vesktop simply vanished on every restart of the shell.

Adoption now tries `/StatusNotifierItem`, `/StatusNotifierItem/1`, and
`/org/chromium/StatusNotifierItem/1` in order, keeping the first that answers `Status`, and reports
every refusal together when none does. Three round trips at startup for an item at the last of them,
against an icon that was otherwise lost until the application itself restarted.

The ayatana shape (`/org/ayatana/NotificationItem/<id>`) is deliberately not in the list: its last
segment is an application-chosen id no fixed list can hold, and those clients re-register on
`StatusNotifierHostRegistered` -- yerd, watched through a restart on 2026-09-08, came back on its
own within the second. An item exporting at any other path still needs the connection introspection
ADR-0073 declined.

## 0172. An item is a connection and a path, the way KDE says it

Reading how the established hosts do this (Qt's client, KDE's watcher, Plasma's system tray,
Quickshell, Noctalia v5) settled a question ADR-0031 left open. Every one of them parses the
`service` argument identically to us -- leading `/` means the sender plus that path, otherwise a bus
name with `/StatusNotifierItem` as the default -- so the parsing was never the difference. The
difference is what happens next.

KDE's watcher composes `QString notifierItemId = service + path;` and publishes *that*, and Plasma's
own host then rejects any id without a `/` as invalid. The resolution happens once, at registration,
where the sender is still known, and no host downstream ever guesses.

Ours resolved the same pair, keyed the registry on it, and then handed config the unique name alone.
Two consequences, one latent and one that already bit:

1. `RegisteredStatusNotifierItems` answered `":1.42"`, which Plasma's host would call an invalid
   notifier id. It now answers `":1.42/StatusNotifierItem"`, and so does
   `StatusNotifierItemRegistered`.

2. `TrayItem.id` was the connection alone, so one connection exporting two items gave both the same
   id -- and a `list` handed two rows with one key refuses the whole tray, which is the freeze of
   2026-09-08. Chromium numbers its items `/StatusNotifierItem/1`, `/StatusNotifierItem/2` precisely
   because one process can hold several. The id is now the sanitized name with the path appended,
   `"1.42/StatusNotifierItem"`. Icon spooling folds the separators into a flat filename stem, and
   doubles `_` first so no two ids can fold onto one file.

Not taken from KDE: its watcher accepts every registration and lets `NameOwnerChanged` clean up,
where ADR-0168 refuses an object that answers nothing. Refusing is still right -- the alternative is
a blank icon in the strip for an item that is really somewhere else -- and this ADR makes the probe
optional rather than load-bearing, since a duplicate id can no longer reach config.

Still open: `StatusNotifierItemUnregistered` is declared and never emitted. Nothing consumes our
watcher's signals but us, and the registry drops the entry on `NameOwnerChanged` regardless.

## 0173. The notifications panel is the shell's status sheet, not a feed

`NotificationHistoryPanel.qml` opens with three things above its masthead: `WeatherWidget`, then
`SystemInfoWidget`, and only then the bell, the summary line and the list. Ours had the masthead and
the list. Two thirds of the panel were missing, and the readout that should have been at the top of
it was instead a two-glyph pill inside the settings toplevel -- a window the mirror does not have.

Restored:

1. `SystemInfoWidget` is a factory (`modules/bar/indicators/system_info.lua`), instantiated by
   `modules/bar/panels/notification_history.lua` where the mirror instantiates it. Each instance
   names its own `expanded` state, which is what QML gets for free from instantiation. The settings
   toplevel does not take a second instance: the same numbers in two places is what this ADR is
   removing, not something to reproduce. That window stays, thin, because it is the config's only
   `window {}` and so the only exercise of § 6's toplevel.
2. `Components/InfoBadge.qml` became `components/info_badge.lua` and carries the header's urgent
   count. `bluetooth_panel.lua` had already written the same capsule as a local `battery_badge`;
   that was the second call site the extraction rule wants, and the mirror had made it a shared
   component for six.

### What is absent, and why absent beats faked

The mirror's `SystemInfoService` shells out for GPU load, per-disk usage, uptime and boot time.
§ 2.12 is `cpu_percent`, `ram_percent`, `swap_percent`, `temp_cores` and `temp_gpu` -- "CPU, memory
and temperatures", exactly as the capability table names it, and nothing here proposes to grow it.
So the GPU usage tile, the disk rows and the uptime footer have no data and are not drawn. Their
space is spent on what § 2.12 does push: swap as the memory tile's second line, where the mirror
prints `used / total`, and the GPU's temperature where the GPU tile stood -- already conditional in
the mirror (`visible: gpuTemp > 0`), and `temp_gpu` is `-1` with no sensor, so one test covers both.
The collapsed summary is `CPU`/`RAM`/`SWAP` where the mirror's is `CPU`/`RAM`/`GPU`/`DISK`.

`temp_cores` is one entry per hwmon sensor rather than per core, and the mirror's single `cpuTemp`
is a package figure. The tile shows the hottest sensor: a mean over a list that may include a
chipset probe reads cooler than any core actually is.

### Polling has no off switch here

The mirror ref-counts `SystemInfoService.refCount` from the widget's `active`, so the pollers only
run while the panel is open. § 2.12's `configure` sets an interval and nothing else -- zero stops a
poller for every reader, not for one widget -- so the choice is polling always or polling never.
Two `/proc` reads every couple of seconds is the cheaper mistake, and `temp_interval` now rides with
RAM at 5s because a tile's second line is not a number anyone watches move.

### An unrelated swap this uncovered

`config/icons.lua` had `cpu` and `ram` the wrong way round: F035B is the square processor with pins
and F061A the DIMM stick, and `SystemInfoWidget.qml` uses them that way. The old system readout
labelled a memory module "CPU". Nothing else referenced either name.

### Still a deviation

`DateTimeDisplay.qml` opens the notifications panel from the whole clock and hangs `MinimalCalendar`
off its hover tooltip. Here the bell opens the panel and the clock opens the calendar as a panel of
its own, because a hover-revealed calendar cannot be exercised in this session at all. The panel's
contents now match; its opener does not.

## 0174. The clock is one control, and the calendar is a tooltip

`DateTimeDisplay.qml` is a `Rectangle`, a `Row` holding the bell and the clock, and one `MouseArea`
filling the whole thing whose click opens the notifications panel. The calendar is not a panel at
all: `MinimalCalendar` hangs off the same item's hover tooltip, under the weather description.

Ours had split the control down the middle -- bell to history, date to a calendar panel of its own.
So the bar's one always-visible readout opened a month grid half the time, and the panel that
carries the system readout, the greeting and the feed was reachable only by hitting a glyph two
characters wide.

Now: one `button` over the pill, opening `notification_history`, with `MinimalCalendar` moved into
`date_time.lua`'s tooltip and dropped from `panel_host.lua`'s list. The pill also takes the mirror's
third state, `border.color: panelOpen ? activeColor : ...`, so it rings while its own panel is up.

### Tooltips stand down while a panel is open

The mirror gates this item's tooltip on `mouseArea.containsMouse && !panelOpen`. That gate is now in
`components/tooltip.lua`, so it covers all seven slots that use it, and on any panel rather than the
hovering indicator's own: the panel card hangs directly under the bar, so a tooltip opening into
that space is a second sheet over the one just asked for, whichever indicator opened it.

`MinimalCalendar` is sized by its month, not padded to a fixed six weeks: `rowCount:
Math.ceil((firstDayOffset + daysInMonth) / 7)` is four to six, and a fixed six drew a row of seven
blank cells under September 2026. A `popup` surface is sized explicitly (§ 6), so the height is a
`Bound` -- which is why § 6 takes `integer|Bound` there -- and the tooltip's own height follows it.
Its vertical padding is `spacing.md` rather than the shared `xs`, per-tip because the one- and
two-line tips have their `xs` already counted into a fixed height and widening it for all of them
would squeeze their text.

The panel card took the same number on every edge. `NotificationHistoryPanel.qml` is
`readonly property int padding: Theme.spacingMd` with `anchors.margins: root.padding`, sizing itself
as `contentColumn.implicitHeight + padding * 2`; `panel_card`'s default is `sm` top and bottom
against `md` left and right, so every panel's first line -- the greeting here, a section heading
elsewhere -- sat on the card's top edge. `panel_host.lua` now names the padding once and derives
`CARD_CHROME` from it, since the card's animated height is a number rather than its content.

### Twelve-hour, decided rather than derived

`TimeService.qml` asks `Qt.locale().timeFormat(Locale.ShortFormat)` whether an `AP` marker is
present and picks `HH:mm` or `hh:mm AP` from the answer. A config has `os.date` and no locale to
ask, so the format is chosen here: `%I:%M %p` on the bar and in the tooltip's seconds line.

### A greeting the mirror does not have

The notifications panel opens with the account's full name and `Tuesday 08th of September 2026
03:11 PM` above the system readout. `NotificationHistoryPanel.qml` has neither. It is 420px of
sheet hanging off the bar and reads as a sidebar; a sidebar that never says whose session it is,
and abbreviates the date to `Tue 08 Sep` because the bar pill is narrow, was the gap. The bar keeps
the abbreviation; the panel has room to spell it out.

Reading the name meant `modules/global/lock.lua`'s identity block -- `getent passwd` for GECOS,
`uname -n` for the host -- moving to `lib/identity.lua`. Two readers is the extraction rule, and it
matters more than usual here: the guard that stops a reload spawning more processes is a `state`,
so the two subprocesses must run once for the session rather than once per module that asks.


## 0175. A process the user would notice stopping belongs to the session, not the generation

`process.run` gives a config one lifetime: the child belongs to the generation that spawned it, and
`reap_generations_processes` kills its group on every swap. That is right for a helper that answers
a question and exits, and wrong for anything the user would notice stopping. A screen recorder is
the case that forced this, and the mirror shows what the wrong lifetime costs.

`Services/SystemInfo/ScreenRecordingService.qml` is 223 lines, and about 180 of them are one
workaround. Because Quickshell replaces its singletons on reload, the recorder has to be orphaned
rather than held: a 900-character `sh` script backgrounds `gpu-screen-recorder`, reads
`/proc/$pid/exe` to check the right binary came up, reads field 22 of `/proc/$pid/stat` for the
kernel start time, and writes pid, start time, path and launch epoch to a lock file. Every later
signal re-runs that probe first, because a pid alone can name a process that has already been
recycled. A two-second `Timer` polls the same probe to notice a crash. `PersistentProperties` and
the lock file's launch epoch between them reconstruct elapsed time, disagreeing about paused
seconds depending on which one survived.

None of that is about recording. It is the cost of the owner dying while the owned keeps running.

Copying it was the obvious move -- it is the mirror's own design, it works on this machine, and
`setsid` is enough to escape our `killpg` where Quickshell needed nothing. What decided against it
is that the Supervisor does not restart on a config edit. It already holds a `Child` for every
`process.run`. The workaround exists to answer "is that still my process?", and the Supervisor never
has to ask.

1. **`oblisk.processes` is a roster capability, and `session_process { name, stop_signal }` is its
   declaration.** Exactly the `oblisk.storage`/`persistent_table` pair in shape: the config names the
   thing, the Supervisor owns what sits behind it, and state comes back keyed by that name. It is
   that pair's opposite in what it holds -- `storage` keeps a file the config could have read
   itself, this keeps a handle the config *cannot* hold.

   Not an option on `process.run`. A `detached = true` flag spawns something whose exit reports to
   callbacks in a VM that no longer exists, and hands back a handle nothing can re-find. Detachment
   without an owner is the workaround with a nicer spelling.

2. **One task per running program owns its `Child` and is the only place its pid is signalled.**
   `supervise` selects between `child.wait()` -- cancel-safe, so it re-arms after each request --
   and a request channel. Every signal is therefore sent by the task that has not yet reaped the
   process, so the kernel still reserves that pid and it cannot have been recycled underneath.
   Keeping a pid in the controller's map and signalling from there would have reopened the exact
   window the start-time check exists to cover.

3. **The stop signal is declared, and shutdown uses it.** `SIGTERM` is the default and wrong for the
   first program that will use this: `gpu-screen-recorder` finalises its container on `SIGINT`, and
   a reap that skips that step leaves an unplayable file. The grace is five seconds rather than
   § 10's 100 ms for the same reason -- a program is declared this way because it is doing something
   long, and closing it out takes longer than closing a helper that had nothing to finish.

4. **stdio is inherited, not piped.** A session process outlives the generation that started it, so
   there is no callback left for its output to reach. Piping it would mean either dropping the lines
   or inventing an owner for them across generations; the shell's own log is the honest destination,
   and a config that wants a program's output wants `process.run`.

5. **`start_error` is state, not just a log line.** A config waits on `running`. A command that is
   not on `PATH` never sets it, and without a readable reason that is indistinguishable from a slow
   start -- a spinner that never resolves, with the explanation only in the Supervisor's stderr.

6. **`start` on an undeclared name is refused rather than creating one.** Declaring is what makes a
   name exist, so a typo reads `nil` instead of looking like a program that never manages to start.

The wire name is `processes`, one letter from the existing off-roster `process`, and the two route
through different arms of `main.rs`. A test pins both: `from_name("process")` is still `None`, and
`from_name("processes")` resolves.

What this deletes from the config that has yet to be written: the launch script, the lock file, the
`/proc` probe, the liveness poll, the restore-on-restart path, and the split elapsed-time
accounting. What replaces them is `rec.running` and `rec.started_at`.


## 0176. The recorder is argv, a file name and pause arithmetic; everything else was the lock file

`ScreenRecordingService.qml` is 223 lines. `lib/screen_recording.lua` mirrors it in about 250, and
the two files have almost nothing in common, because ADR-0175 deleted the mirror's subject. Gone:
the 900-character launch script, the `/proc/$pid/exe` check, the kernel start time, the lock file,
the re-probe before every signal, the two-second liveness poll, and the restore-on-restart path.
What is left is the part that was always the config's -- which argv to build, what to call the file,
and how to count a pause.

The panel and indicator are `ScreenRecorderPanel.qml` and `ScreenRecorder.qml` as they stand.

### Where this deliberately leaves the mirror

1. **`-w <WxH+X+Y>`, not `-w region -region <WxH+X+Y>`.** The installed gpu-screen-recorder prints
   *"option -region is deprecated, use -w with region directly instead"* and then fails:
   `gsr_encoder_receive_packets: failed to write frame index 1 to muxer, Invalid argument (-22)`,
   with nothing written. The same geometry through `-w` records cleanly. Found by running the
   mirror's own form on this machine, not by reading.

2. **The exit status picks the notification.** `_clearRecording(true)` announces "Recording saved"
   however the recorder ended. Observed live: a bad `-a` argument killed it at once and the popup
   offered to play a file that did not exist. `gpu-screen-recorder` answers `SIGINT` by writing the
   container index and exiting 0, so zero means there is something to offer and anything else gets
   a failure notice naming the code.

3. **Three mouse buttons on the indicator.** `components/icon_button.lua` guards left-click only, on
   the argument that a stray right-click should not act. This is the mirror's design and it is the
   right one here: the two captures differ only in extent, so one click each beats a panel round
   trip, and the panel still names everything for anyone who does not remember which button is
   which. It takes `on_button` rather than `on_activate`, which is the escape hatch that already
   existed for exactly this.

4. **Four buttons in two slots.** `OButton` binds `bgColor` and `variant` live; `action_button`
   picks its grounds from a static `tone`. Making `tone` a signal means mapping rest, hover, border
   and ink through it, for one caller. An invisible node takes no size and no spacing gap, so a pair
   per state costs the same row and every button keeps one label, one tone and one job.

5. **The settings section stays open across a close.** `onIsOpenChanged` collapses it; ours matches
   `modules/bar/indicators/system_info.lua`, the config's other expandable section. There is also no
   close edge to hang the reset on that would not make `lib/ui_state.lua` require a panel back.

### Components that grew, and why each was the mirror's own parameter

`panel_header` gains `accent`. `PanelHeader.qml` has `property color accent` and our boolean
`active` was a simplification of it -- fine while every subject was on or off, wrong for a recorder
where a live capture is `critical` and a ready one `activeColor`, and neither is "off".
`info_badge`'s ink now follows a live ground for the same reason: `badgeColor: paused ? warning :
critical` swaps peach for red mid-capture. `action_button` gains `danger`, `height` and a `glyph`
slot; `panel_toggle_card`'s icon and height become optional, because a frame-rate tile has no glyph
and the mirror's `modelData.icon ?? ""` draws an empty line where a missing one is the same intent.

### Every glyph on the bar was a third too large

Reported as "the stop icon doesn't look right compared to the QML version". It was not the stop
icon. `components/icon_button.lua` defaulted its glyph to `theme.icon.lg`, under a comment claiming
that matched `iconSizeFor("md")`. It does not: `IconButton.qml` defaults `size: "md"` and no bar
indicator overrides it, so the mirror draws `iconSizeMd`, `s(18, 14)`, while `icon.lg` is
`s(24, 18)` -- the mirror's `iconSizeLg`.

So every circle on the bar had been drawing its glyph a third oversize since the component was
extracted. A wifi arc or a bell hides that; a filled square does not, which is why the recorder is
where it surfaced. One number, and the comment that had been asserting it was right.

### The hole, and the one thing not built

Pause arithmetic is `state`, so it survives an in-place reload and resets on a generation swap,
after which paused seconds count as recorded ones. The mirror has the same hole across a Quickshell
restart. A debounced disk write per pause is not worth closing it.

`IPC.qml`'s `rec toggle` has no equivalent. `oblisk set` writes a value, and a surface reading that
value re-renders; starting a recording is a call, not a value, and the only place a config can run
code at a moment is a capability's `on_change`. Built on `oblisk.system` that is a keybind answering
up to a second late, which is worse than not having one. It is a roadmap row instead.

Verified live rather than by inspection, because the pointer cannot be moved from here: a capture
started, paused, resumed and stopped from a temporary CLI door in this file, since removed. The
elapsed badge read `1:47` seven seconds apart while paused and `1:51` four seconds after resuming;
the stopped file is 9.3 MB and `ffprobe` reads `duration=112.905512`, so `SIGINT` closed the
container and gpu-screen-recorder's own duration agrees with the arithmetic here to within two
seconds.


## 0177. The bar draws glyphs in the body font; only panels use the icon font

Two bars side by side, ours above and the Quickshell config below, and the report was "all icons in
our bar look off". Not one icon -- all of them.

It was not the codepoints. Extracting every private-use character from `Modules/Bar/Indicators/*.qml`
and diffing against `config/icons.lua` found 25 of 26 already identical, down to the Font Awesome
range the mirror mixes in for battery levels and update states. `wallpaper` was the only wrong one.

It was not the size either, though that had to be fixed first to see past it (ADR-0176).

`Config/Theme.qml` declares two faces:

    readonly property string fontFamily:     "CaskaydiaCove Nerd Font Propo"
    readonly property string iconFontFamily: "JetBrainsMono Nerd Font Mono"

and the split between them is **bar versus panel**, not glyph versus text. `IconButton.qml`,
`NetworkIndicator.qml`, `DateTimeDisplay.qml` and `BatteryIndicator.qml`'s `OText`s all draw their
glyphs in `fontFamily`; `PanelRow`, `PanelHeader`, `PanelToggleCard`, `OSDCard`, `AppLauncher` and
`LockContent` use `iconFontFamily`. Our `components/icon_button.lua` passed `theme.icon_font` to
every circle on the bar, so each one drew the right Material codepoint in JetBrainsMono's lighter,
narrower cut instead of CaskaydiaCove's. Correct glyph, wrong hand.

`components/glyph.lua` keeps `icon_font` and needs no change: its callers are exactly the panel
components that use `iconFontFamily` in the mirror. ADR-0144 said "a glyph is drawn in the icon
font"; the rule is narrower than that, and the bar is the other half of it.

### The battery pill, which the comparison also settled

Three deviations, all of them ours and all documented at the time:

1. **Green for a healthy battery.** `batteryColor` is `critical : warning : Theme.activeColor`.
   Green was a fourth state the mirror does not have, and side by side it was the loudest difference
   between the two bars.
2. **A fill tinted to 38%, and one readout colour.** The tint was justified by green being too loud
   opaque; accent at full opacity is the same weight as every other lit control here, so both go
   back. The readout returns to `textContrast(percentage > 0.6 ? batteryColor : bgColor)` -- 60% is
   where the text's centre crosses from the fill onto the pill, so that is which ground it
   contrasts against -- and both lines are `bold: true`.
3. **The charge glyphs were swapped.** `isPendingCharge` is tested *first* and gets the bolt;
   everything else on mains gets the plug. So a battery that is actually charging draws the plug,
   and only one parked at a charge limit draws the bolt. Not the obvious order, and right on a
   machine with a limit set, where plugged-and-moving is ordinary and plugged-and-parked is the
   state worth its own glyph. Ours had them the other way, which is why this laptop showed a plug
   where the mirror showed a bolt.

Measured rather than eyeballed throughout: the power glyph is 13x15px here against the mirror's
14x16, which is what said the size was already right and sent the search to the face.

## 0178. A tween that only changes what a node paints does not lay the tree out again

ADR-0145 made a compositor frame callback the tween clock and `Scene::tick` the frame. That tick
answers one question -- what size is everything now -- by cloning the retained tree, building a
fresh taffy tree, re-parsing every node's `LayoutStyle` and `PaintStyle`, solving, and measuring.
Most of what a config animates cannot change the answer. `opacity`, `background`, `border_color`,
`foreground` and `radius` are read by neither `taffy_style` nor `measure_for`, which takes a text's
content, size, family and wrapping but not its colour. The shipped `dev-config` names `background`
in eight `animate` blocks and `border_color` in four, against five for `width`.

So a tick whose every running tween names one of those properties advances the tree where it
stands: `node::advance` writes the displayed values into the retained map, and the node's `opacity`
and parsed paint are re-derived from them. No clone, no solver, no measurement, no geometry
publish. Anything else -- a `width`, a `padding`, a leaving node whose exit ends by dropping it --
falls back to the relayout unchanged.

`transform` is excluded although the solver ignores it too. ADR-0149 maps the pointer back through
a node's inverse transform, so moving one changes what the pointer hits, and the input regions have
to be rebuilt with it. It stays on the layout path until something rebuilds those regions without a
full pass.

**The clone was rollback, and the fast path still needs one.** `node::advance` writes into the
retained map, and a value it writes can be refused: a spring overshoots its target, and
`parse_opacity` rejects anything outside `[0, 1]` rather than clamping (ADR-0068). Working on a
clone made a refusal free, because the half-advanced tree was a copy. In place it is not: a refused
value left in the retained map would be re-read by the next pass and fail that too, one bad frame
becoming a scene that stops updating. The fast path therefore keeps the values it is about to move
and puts them back on refusal, bounded by that node's tweens rather than by its subtree's
properties.

**`Scene::tick` returns the instances it advanced, not whether any did.** The poll loop repainted
every mapped surface on any turn that resolved, so each unticked surface built a display list --
copying a text draw's content and style runs, an icon's name -- only to have it rejected as equal
to the one it last painted. A tick knows which trees it touched. Recorded before the advance, not
after: the frame that ends a tween is the one that shows its target, and by then the tree is no
longer `animating`.

**A mid-tween surface whose list is unchanged commits without drawing.** The commit is what makes
the frame request effective, so it cannot be skipped, but re-drawing identical pixels can be. A
hold, a lead-in `delay`, or a step easing sitting on one value now costs a commit instead of
make-current, clear, every draw call and a swap.

Measured on the real `dev-config` under a continuous keyframe tween, debug build, 60Hz, machine
idle. Tick turns fell from p50 4.98ms / p90 6.27ms / max 30.41ms to p50 1.42ms / p90 2.15ms / max
3.83ms, and tick turns over the 16.6ms frame budget from 2 in 2959 to 0 in 2670. Every dropped
frame in the run after the change was a full capability pass; none came from a tick.

Rejected: replacing the whole-scene rollback clone at the head of `Scene::apply_admitting` with
borrowed preparation. Timed directly at p50 0.38ms against a p50 10.79ms pass -- about 4% -- so the
ownership and staging it would take buys nothing worth the risk.

Rejected: per-surface dirtiness in place of ADR-0044 decision 2's single flag. It needs a set of
sources read by each surface, and a config's getters are arbitrary Lua that may read the clock or a
mutable upvalue, so a declared dependency vector cannot be assumed to capture everything. That
needs a reactivity contract or a conservative fallback, not an accounting change.

Rejected: taking the tween clock from the frame callback's `time` argument instead of
`Instant::now()`. It is milliseconds against an undefined epoch, needing wrap handling and a rule
for multiple surfaces, and the sampled step between presented frames already measured p10 16.34ms /
p90 16.83ms. Its target is the residual 2.9% of outlier steps, which is not where the time is.

**Amends ADR-0152.** A sequence's `resting` flag is now re-derived from the clock a pass ran on
rather than carried from the tick that set it. `delay` sits on the spec beside the motion, not in
it, so a played-out run handed a fresh `delay` still matched as the same list and carried
`resting = true` across; `advance` skips a resting tween and `animating` does not count one, so
nothing asked for the frame that would have started it and the run sat on its old last frame. Under
one monotonic clock a counted sequence that is done stays done, so re-deriving costs nothing and
only differs where carrying was wrong.

**What this does not fix.** The first frame that reveals a large surface still costs far more than
a steady one -- 204ms of repaint on the update panel's first open, 97ms then 50ms for the
notification area, against 5.84ms for that panel's subsequent tween ticks. That cost survives the
surface being destroyed and recreated, so it is a process-wide cache filling rather than anything
the tick does, and it is not attributed yet: the repaint phase spans every mapped surface, EGL
binding, the swap, and `ImageCache::upload_landed`, which charges every landed background decode to
whichever surface paints first.

## 0179. A dev build optimizes its dependencies, because the frame is mostly their code

ADR-0178 left the first frame that reveals a large surface unattributed: 204ms of repaint on the
update panel's first open, against 5.84ms for its later ticks. Splitting `layout::paint::execute` by
draw kind named it. Text drawing was 272.8ms of a 301.5ms frame; `ImageCache::upload_landed` was
0.0ms on every frame, and `draw_clipped` 37.1ms on the one frame with scratch targets and about
0.3ms elsewhere. So it is femtovg rasterizing each glyph into its atlas the first time that glyph
and size are drawn, which is why the cost survives the panel's surface being destroyed and
recreated -- `TextPainter` and its warm atlas outlive the surface -- and why the second open is
smooth. The `draw_clipped` pool its own `ponytail:` comment proposes is not worth doing: it has no
cold/warm distinction and is not where the time is.

The same split ruled out the two other candidates. Layout measurement does not warm this: it goes
to the cosmic-text worker, while paint hands the string to femtovg separately, so a cheap resolve
phase says nothing about paint-side glyph cost.

None of that work is workspace code, and all of it is what `opt-level = 0` punishes hardest.
Building dependencies at `opt-level = 3` while this workspace's crates stay unoptimized takes the
same first paint from 272.8ms to 9.3ms, and a 5162x2160 wallpaper decode from 9636ms to 162.7ms --
within about 1.5x of a release build for both. Dependencies rebuild only when one changes: the
one-time cost was 3m52s, and an incremental workspace rebuild stayed at 0.7s.

This is worth a decision entry rather than a config tweak because a dev build was dropping frames a
release build never would, which made every animation judgement taken against it a guess. The
measurements in ADR-0178 were taken before this and are all `dev` figures; they compare against
each other, not against a release shell.

Rejected: warming the atlas by drawing the chrome's text before it is shown. It relocates the cost
to startup rather than removing it, needs a list of what to warm that nothing keeps in step with
the config, and does nothing for text a config produces at runtime.

Rejected: `opt-level` on the workspace crates too. The debug experience is the point of a dev
build, and the measurements say the workspace's own code was never the expense.

Still open: the wallpaper. 162.7ms in dev and 114ms in release, on the render thread, at every
startup and every wallpaper change, because `Load::Inline` is the default that ADR-0122 decision 2
chose for complete first frames and a wallpaper-sized box is past the largest thumbnail size
(`image::thumbnails::size_for`). The decode is 40.7ms and the resize of 11.1 megapixels down to
3.4 is 54.5ms, so there is no scale-on-decode shortcut: covering a 1920x1200 box from 5162x2160
needs 2868x1200, and the next DCT step down undershoots it. Moving it off the frame, not making it
faster, is the fix, and which way costs a blank first frame.
