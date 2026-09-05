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

## ADR-0116: Pointer drags and wheels on a button, and the microphone's volume

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

## ADR-0117: A workspace knows whether it is empty and what runs on it

1. Add populated and one representative app ID per workspace: focused window first, otherwise
   lowest window ID. Empty IDs become absent; keep reduction compositor-neutral.
2. Still no per-workspace window list; a switcher is a separate caller.
3. The example strip collapses on row hover and resolves installed application icons, otherwise
   showing workspace numbers.

A populated window without app ID remains populated. No width animation or opacity fade.

## ADR-0118: `workspaces` speaks Hyprland, as a module behind the same publisher

1. Add a Hyprland module behind the existing publisher and exhaustive dispatch, still no trait.
   It uses documented IPC and synthetic fixtures, not live-verified captures.
2. Re-read workspace/monitor/client/active-window JSON on relevant event-socket lines using direct
   command sockets, not four subprocesses. Coalescing waits for measurement.
3. Regular workspace number is both ID and index. Nonpositive/special IDs were initially omitted.
4. Active/focused follow monitor state; use activewindow rather than stale client focus history.
   Representative app selection uses workspace focus-history order.
5. Share socket-path resolution with keyboard and fix both callers' missing leading dots.

Padding, specials, fullscreen and compositor metadata were deferred to the next payload decision.

## ADR-0119: What one compositor has and the other does not is an absent key

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

## ADR-0133: `oblisk.battery` reads UPower uncached, because its wake-up races zbus's cache

Battery wakeups and zbus's property cache consumed the same change independently. Reading before
cache refresh compared equal, dropped the push and left state one event behind for minutes.

Disable caching on DisplayDevice reads while keeping one whole-object subscription.
Five reads on infrequent changes cost less complexity than five property streams.
The power capability's cache-driven property streams were already ordered correctly.

Live unplug/replug confirmed the fix. Hardware latency was not the cause.
A similar tray custom-signal/cache risk remained unconfirmed and deliberately unfixed.

## ADR-0134: `oblisk.updates` is a schedule with a package manager behind a trait, and says which one

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

## ADR-0135: an empty `textfield` shows its placeholder even with the keyboard, because `autofocus` made the alternative unreachable

An empty ordinary field shows its placeholder even while focused, so autofocus cannot make the
prompt permanently unreachable. With no placeholder, retain the bare caret fallback; nonempty
draft/caret behavior stays unchanged.

Reject per-config overlay workarounds and placeholder-plus-caret in identical ink, which looks
like typed text. Accept the weaker focus cue among multiple empty replies for now.
A distinct placeholder color is the upgrade, not hiding search prompts again.

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
