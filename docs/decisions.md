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
