# Decisions

Historical decisions, not current API documentation. Entries may describe proposals, deferred work
or behavior superseded later. Current contracts live in [API](lua-api.md) and
[services](services.md); open work lives in [roadmap](roadmap.md).

Entry and internal decision numbers are permanent because code cites both. Keep the choice,
constraints, rejected alternatives and amendments when shortening an entry. Add new decisions
sequentially; do not rewrite an old decision to match later implementation.

Early phase numbers and spec section references belong to the historical documents.

## 0001. Reload strategy splits on topology change

Config edits use two reload paths. A value change, anything not touching a top-level `surface`
node's set, layer, anchor, or monitor, resets and reruns the Lua VM in place. It creates no new
process or generation ID bump. A topology change uses the full generation swap: candidate spawn,
presentation evidence, promote, and reap.

Rejected: one generation-swap path for every edit. Each save would pay Wayland rebinding and a
multi-output presentation-feedback round trip, causing the stutter the swap protocol avoids.

Rejected: one long-lived Wayland connection reconciled against a rebuilt scene tree, matching
Quickshell's per-reload `QQmlEngine` and stable-id-matched `Reloadable` protocol, which lets
components reuse their native handle across the rebuild. QML's tracing GC
can abandon and reclaim the old object graph. Rust cannot safely reuse native surface or texture
handles while reconciling two trees without unsafe code, `Rc<RefCell<>>` panics, or manual arena and
generation tagging. A process boundary makes killing the process one kernel-guaranteed free. The
in-place path is safe because it drops the whole `mlua::Lua` VM atomically via RAII and never
reconciles live native objects. Supervisor state held for a generation, such as idle-threshold
registrations, still needs separate cleanup (ADR-0006).

Superseded in part by ADR-0044: the VM is not reset.

## 0002. Wallpaper surface skips transitions on reload and queues overlapping sets

The wallpaper surface (ADR-0007) paints its current texture directly on candidate first-frame and
in-place reload, without a shader transition. Transitions respond only to a live `wallpaper:set()`
call. A second call queues its target until the current transition finishes. It does not interrupt
the shader or overwrite its texture. The transition's second texture buffer exists only for its
duration.

This follows Quickshell `AnimatedWallpaper.qml`: `Component.onCompleted` sets the image source
without a shader, the transition `Loader` exists only while a transition runs, and
`changeWallpaper()` queues through `pendingUrl`.

Amendment (ADR-0055): wallpaper is now an `image` node on a config-declared `Background` panel and
paints a new texture directly without transition. That is this ADR's first-frame and reload branch.
The transition branch is unbuilt and remains specified: the engine has no animation model, shader,
or second buffer for the queue to arbitrate. ADR-0055 decision 1 retracts `wallpaper:set()` as a
capability call; references above mean config writing to the `state()` signal bound to
`image.source`.

## 0003. Authority transfers per output, not per process

`services.md` § 14's all-display presentation barrier is replaced by
per-(`generation`, output) authority. Each output moves to the candidate when its own presentation
evidence arrives, without waiting for siblings. The Supervisor reaps a generation at zero owned
outputs, immediately or minutes later if one output sleeps.

A sleeping DPMS-off or slow output would otherwise stall every promotion, or force a deadline that
either leaks the old generation or blind-promotes an unproven candidate, reintroducing the black
frame the generation swap prevents. This matches `surface` with `monitor = "All"`, which already
binds one `wl_surface` per output.

Rejected: whole-process authority with a deadline that force-promotes every output. A dead
candidate, detected by SIGCHLD before any output promotes, aborts the whole candidate. A slow output
on a live candidate waits indefinitely if needed; its null-buffer is already staged.

## 0004. Revision is tracked per capability, fed by inbound state pushes

The Renderer's IPC receive loop keeps one revision per capability and increments it for every inbound
`StateSnapshot` (`shared::StateSnapshot { revision, payload }`), whether Lua reads that signal or
not. A write envelope reads that counter for its existing capability-scoped `expected_revision`
field. There is one counter per capability, not per signal, and no new wire format.

Writes take bare values (`audio:set_volume(vol)`), not `Signal` handles, so `expected_revision` cannot
be read from the write arguments as `generation_id` is carried by the mlua wrapper.

Rejected: stamping a per-capability "last observed revision" table on `Signal:get()`. A zero-argument
write such as `audio:toggle_mute()` may have no `:get()` in its callback, making freshness depend on
an unrelated read elsewhere in the frame.

Quickshell and Noctalia have no comparable optimistic write concurrency because each has one active
writer per capability. Obelisk needs it while generations N and N+1 both retain write paths during a
swap.

## 0005. Secure textfield submit targets a capability action, never Lua

Typed characters in a `mask_character`-bearing `textfield` never enter Lua. `textfield` gets
`secure_submit = { capability, action }`. With both properties, `on_submit` has no argument and the
Renderer's IPC layer attaches the native input buffer to that capability/action envelope server-side.
With `mask_character` but no `secure_submit`, Lua cannot read the value. The buffer is zeroed after
the IPC send, not only by `Drop`, because panic or early-return timing cannot protect a plaintext
password. This follows Noctalia.

The reference lock screen needs `on_submit = function(text) ... end`, and Wi-Fi password entry needs
the same secret-input primitive. Quickshell's PAM binding `PamContext::respond(const QString&)` passes
the password to QML as a plain string. Noctalia avoids a scriptable callback by using a fixed native
lock widget. Making the primitive non-scriptable does not fit Obelisk, where Lua must author the
Wi-Fi password field.

Amendment (ADR-0009): the original decision protected only the Lua boundary and never told the
compositor or IME the field is sensitive. A `mask_character`-bearing `textfield` also sets
`purpose = password` and the sensitive-data hint on its `zwp_text_input_v3` object, using
`wp-text-input-v3`'s `content_type` purpose/hint fields. A well-behaved IME then skips logging,
autocorrect, and clipboard-history capture, in addition to the Lua boundary.

## 0006. In-place reload explicitly resets Supervisor-held registrations

Before an in-place reload, the Renderer sends one IPC message: capability `"renderer"`, action
`"reset_registrations"`. The Supervisor drops every registration for that `generation_id`; the
fresh top-level run rebuilds them. Both sides use unconditional clear then rebuild, with no
dedup-by-key.

ADR-0001 resets the Lua VM and reruns `shell.lua`, reissuing every `idle:register_threshold(...)`.
Those registrations live in the Supervisor, so a VM reset cannot remove them. A second register
looks new and leaks duplicate `ext_idle_notification_v1` listeners on every value-change reload.

Quickshell's `IdleMonitor` extends `PostReloadHook`; a fresh object and its destructor tear down the
listener. Noctalia's `IdleManager::reload(config)` calls `clearBehaviors()` before rebuilding. Those
solutions work because registration and reload share a process; neither crosses this boundary.

## 0007. Wallpaper gets its own Background-layer surface

The two static surfaces for `main_bar` (`Top`) and `overlay_canvas` (`Overlay`) gain a third,
`wallpaper_layer`, on the `Background` layer, non-exclusive, one per monitor. It is separate from
the UI surfaces. See the Wallpaper surface term in `CONTEXT.md` and ADR-0002.

Rejected: painting wallpaper in `overlay_canvas`, which spans the whole screen. `Overlay` is above
every application window in wlr-layer-shell stacking by protocol definition, not by z-order the
shell controls. Wallpaper there covers the desktop, and an input region cannot fix paint order.

Amendment (ADR-0038): the paint-order argument and `Background` surface remain. `shell.lua`, not a
Rust-owned role, declares it. Read "a third static surface" as "a third surface in the default
config".

Amendment (ADR-0055): no `wallpaper` capability or wallpaper-specific Rust code exists. A
`Background` panel containing an `image` node is the whole feature; the layer choice survives.

## 0008. Depend on smithay-client-toolkit instead of hand-dispatching Wayland protocols

The renderer directly depends on `smithay-client-toolkit` (SCTK) and uses `wlr_layer`,
`presentation_time`, `session_lock`, `foreign_toplevel_list`, `output`, and `seat` instead of
hand-rolling registry binding, layer-shell creation, and presentation-feedback dispatch.

SCTK's `Smithay/client-toolkit` source shows `src/shell/wlr_layer` wrapping
`zwlr_layer_shell_v1`/`zwlr_layer_surface_v1`; `src/presentation_time.rs` wrapping
`wp_presentation`/`wp_presentation_feedback` with typed `feedback()`; `src/session_lock` wrapping
`ext_session_lock_v1`; `src/foreign_toplevel_list.rs` and `src/output.rs` covering active-window
and output tracking; and `src/seat` bundling keyboard/pointer handling with `xkbcommon`. It covers
nearly every Renderer Wayland protocol and is maintained with `wayland-client`.

Raw `wayland-protocols` remains for `wp-text-input-v3`, the `textfield` IME binding, because SCTK
does not wrap it. This follows the project rule to use an installed dependency rather than rebuild
on the same lower-level crate.

## 0009. Text-input service owns text-input-v3, textfield nodes never touch the protocol

The renderer has one `TextInputService` bound to the seat. It owns `Dispatch<ZwpTextInputV3, D>` and
the raw event sequence; scene nodes never see the protocol.

`textfield` (`lua-api.md` § 5.2) maps to `wp-text-input-v3`. No maintained crate wraps it beyond
`wayland-protocols` raw generated bindings gated behind `unstable`. Following Noctalia's
`src/wayland/text_input_service.h` and `src/ui/text_input_client.h`, each node implements the
`TextInputClient` shape: it reports surrounding text, cursor, purpose, and sensitive/hidden state,
and receives a batched diff (`commitText`, `preeditText`, delete-before/after lengths), not raw
keystrokes. Only a diff handles IME composition such as CJK input. `on_change` fires from the diff,
effectively on the protocol's `done` event.

This composes with SCTK (ADR-0008) on one event loop. SCTK's `calloop` feature drives one
`wayland_client::EventQueue`; the top-level state implements `Dispatch<T, D>` for delegated SCTK
types through `delegate_*!` macros. Hand-written `Dispatch<ZwpTextInputV3, D>` for
`TextInputService` uses the same struct and queue, and
`zwp_text_input_manager_v3::get_text_input(seat)` uses SCTK's `wl_seat`.

`lua-api.md` § 5.2 is corrected accordingly.

## 0010. Supervisor owns idle-notify and lock authority, with its own Wayland connection

The Supervisor has its own Wayland connection for `ext_idle_notifier_v1` and `ext_session_lock_v1`,
separate from the Renderer.

The session-lock half is superseded by ADR-0042; the idle-notify half stands. `ext-session-lock-v1`
keeps a session locked when its client dies, and hides non-lock surfaces, so an `overlay_canvas` lock
UI would be invisible while locked. Lock surfaces cannot cross processes. ADR-0042 therefore has the
Renderer hold and paint the lock, while the Supervisor remains the durable authority.

`ext_session_lock_v1` and `ext_session_lock_surface_v1` belong to the connection that created them.
The Supervisor uses SCTK's `session_lock` module for lock handling and hand-dispatches
`ext_idle_notifier_v1` through raw `wayland-protocols`, which has no SCTK wrapper. On lock it commits
a minimal fallback surface: a solid-color `wl_shm` buffer with a pre-rasterized "locked" indicator,
without a GLES3/EGL context. The process holding "is the screen locked" must be durable enough that
a Renderer crash never drops coverage, even for one frame. The Renderer paints the full Lua-styled UI
while alive; the Supervisor fallback is authoritative and repaints immediately if that surface drops.

The Supervisor owns anything a Renderer crash or reload cannot interrupt, such as authority handles
and data-stream backends, never presentation.

Rejected: Renderer-owned session-lock authority with a rare crash window. A GPU-driver or Lua VM
crash that fails open across a security boundary is a bug.

## 0011. Renderer loads GL function pointers via glow, not the `gl` crate

The renderer directly depends on `glow` for GLES3 function-pointer loading, not `gl`. It targets
`EGL_OPENGL_ES3_BIT`/GLES3 contexts. The `gl` crate (`brendanzab/gl-rs`, published as `gl`) hardcodes
`Registry::new(Api::Gl, (4, 5), Profile::Core, ...)` in `build.rs` and has no feature flag for
`Api::Gles2`/`Api::Gles3`, so it cannot generate GLES3 bindings.

`femtovg` already pulls `glow` transitively and builds its OpenGL backend from a `glow::Context`.
`glow` loads pointers through any `get_proc_address`-shaped closure and supports desktop GL, GLES,
and WebGL. The renderer therefore depends on it directly and loads it through
`khronos_egl::Instance::get_proc_address` after context creation. `gl` is removed from
`renderer/Cargo.toml`.

## 0012. FemtoVG's glyph atlas is used as-is, not bridged from cosmic-text's shaper

cosmic-text's async shaping wrapper and FemtoVG's glyph atlas remain separate. FemtoVG 0.26.0's
`GlyphAtlas::render_atlas` in `femtovg/src/text.rs` is `pub(crate)` and accepts only its own
`PositionedGlyph`s, produced by `rustybuzz`/`swash` through `Font`/`FontFaceRef` and
`Canvas::add_font_mem`, `fill_text`, and `measure_text`. There is no public entry point for
cosmic-text glyphs. Its private fixed `TEXTURE_SIZE = 512` grows by adding 512x512 pages, not one
configurable 2048x2048 texture.

Decision: keep cosmic-text's `ShapingHandle` in `renderer/src/text/shaping.rs` and FemtoVG's atlas
in `renderer/src/text/atlas.rs` separate for this milestone. `ShapingHandle` is an off-thread
measurement/shaping primitive for a future layout engine. FemtoVG's `Canvas`/`TextPainter` owns GPU
rasterization, its internal shaper, and atlas. `ShapingHandle::default_font_bytes()` lets both use
the same font, found once off-thread through cosmic-text's `fontdb`, without sharing shaping.

Rejected: bridge shaped runs through `draw_glyph_commands`/`GlyphDrawCommands`. That would duplicate
FemtoVG's UV and kerning bookkeeping.

Rejected for now: a hand-rolled 2048x2048 atlas using `create_image_empty`/`update_image` and
cosmic-text/swash bitmaps in an image-pattern fill. No scene-graph `Text` node exists to drive
eviction; revisit when a `Text`/`Icon` node needs cross-surface sharing or working-set eviction.

Amended by 0211: femtovg 0.27's public `fill_glyph_run` is that entry point, and paint now draws
cosmic-text's glyphs through it.

## 0013. Polkit agent registration uses zbus_polkit for the Authority proxy, hand-writes the AuthenticationAgent side

`supervisor/src/dbus/polkit.rs` uses `zbus_polkit` for the polkit `Authority` proxy and hand-writes
the `AuthenticationAgent` interface polkitd calls.

The wire method is
`org.freedesktop.PolicyKit1.Authority.RegisterAuthenticationAgent`,
`(subject: (sa{sv}), locale: s, object_path: s) -> ()`, at
`/org/freedesktop/PolicyKit1/Authority` on `org.freedesktop.PolicyKit1`, verified against
`src/polkit/polkitauthority.c` and `data/org.freedesktop.PolicyKit1.Authority.xml`.
`RegisterAuthenticationAgentWithOptions` exists but is unused. `Subject` supports `unix-session`
(`session-id: s`), `unix-process` (`pid: u`, `start-time: t`), and `system-bus-name` (`name: s`).
The callback `org.freedesktop.PolicyKit1.AuthenticationAgent` requires
`BeginAuthentication(action_id: s, message: s, icon_name: s, details: a{ss}, cookie: s,
identities: a(sa{sv})) -> ()` and `CancelAuthentication(cookie: s) -> ()`.

`zbus_polkit` (MIT, `dbus2` org) supplies an `Authority` `#[zbus::proxy]` and matching `Subject`/
`Identity`; its register/unregister signatures match the wire signature. It is pinned at
`zbus_polkit = "5.0.0"` with `zbus = "5.19.0"`, `default-features = false`, and `features =
["tokio"]`. No maintained MIT/Apache crate covers `AuthenticationAgent`, so it uses
`#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]`.

`current_session_subject()` uses `$XDG_SESSION_ID` and `unix-session`, not this process's pid and
`unix-process`, because `pam_systemd` sets it for every real session. Resolving a pid through logind
is the upgrade path if a logind client is later needed. zbus 5.x changed `#[dbus_interface]` to
`#[zbus::interface]`; the test module uses the latter.

Rejected: `zbus-polkit-agent`. Its GPL-3.0-or-later license conflicts with the workspace's
MIT/Apache-2.0 dependencies for a handful of lines.

## 0014. SecureBuffer uses the zeroize crate, not secrecy

`shared::SecureBuffer` in `shared/src/secure_buffer.rs` wraps `Vec<u8>` with
`#[derive(Zeroize, ZeroizeOnDrop)]`, not `secrecy`.

ADR-0005 requires explicit zeroing after the one sanctioned read, serializing into an IPC envelope,
with `Drop` as the panic/early-return backup. `secrecy`'s `SecretBox<T>`/`SecretString` zeroize on
`Drop` but expose no live-value zeroing method; `zeroize` provides `.zeroize()` and `ZeroizeOnDrop`.
`push_str` appends one edit diff's UTF-8 bytes, not per-keystroke, matching ADR-0009, and
`expose_secret()` is the one sanctioned read. The caller invokes `.zeroize()` after crossing IPC.

The type lives in `shared` because the secret originates in the Renderer (`textfield` keystrokes)
and is read in the Supervisor (PAM); neither constructs or consumes one yet (ADR-0015).

`Vec<u8>::zeroize()` scrubs initialized elements and spare capacity. `.clear()` would leave plaintext
on the heap. Growth via `Vec::extend_from_slice` could copy old bytes to a new allocation, leaving
the old block unsanitized and unreachable by later `.zeroize()` or `Drop`. `push_str` therefore grows
itself: allocate, copy, zeroize the old Vec, then drop it.

## 0015. Polkit's PAM conversation and textfield/IPC wiring are deferred, not built

The end-to-end path is deferred: a privileged action triggers a request, the Supervisor sends
challenge metadata over IPC to a Lua dialog, `secure_submit` accepts the result, and a PAM
conversation answers polkitd. No PAM crate exists in the dependency tree; real `pam_conv` prompting
and `AuthenticationAgentResponse2` are separate from the D-Bus handshake. The `textfield` node and
Unix socket server also do not yet exist, so ADR-0005's envelope attachment has no endpoints.

`supervisor/src/dbus/polkit.rs` has a real dispatchable `AuthenticationAgent::begin_authentication`,
wired in `supervisor::main` to the live session bus. It forwards `BeginAuthenticationCall` over an
`mpsc` channel; `main()` drains it with `eprintln!` until the event loop can push it to the Renderer.
`cancel_authentication` only dispatches. `shared::SecureBuffer` (ADR-0014) is built and tested but
has no `textfield` writer or PAM reader.

Upgrade path: (a) the Unix socket server and event loop replace `eprintln!`; (b) the scene graph's
`textfield` and `secure_submit` (ADR-0005) fill a `SecureBuffer` and return it; (c) a PAM crate,
likely `pam-client` or hand-rolled FFI against `libpam` (unresearched), implements the conversation
and calls `zbus_polkit`'s `Authority::authentication_agent_response2`.

Not built: PAM conversation driving and `textfield`/IPC envelope attachment. This does not contradict
ADR-0005 or ADR-0009, which specify the target shape.

## 0016. Per-app audio stream PID uses `application.process.id`, not `sec.pid`

`sec.pid` (`PW_KEY_SEC_PID`, wire name `pipewire.sec.pid`) belongs to the `Client`, not the stream
`Node`; through `pipewire-pulse` it reports the shim's pid for most PulseAudio-API apps. `node.client-id`
is not a PipeWire property; the real node-to-client link is `client.id`. `application.process.id`
(`PW_KEY_APP_PROCESS_ID`) is set on the stream node by native and PulseAudio-compat clients and
matched the real owning process in every checked case against `pw-dump` and `/proc/{pid}/comm`.

For a `pipewire-pulse` stream it is absent from the first `global` event, then arrives as a `PROPS`
change on the node's `info` event. `supervisor/src/audio/mixer.rs` `on_global` filters only
`media.class`, binds the node, and parses `media.class` plus `application.process.id` in the bound
node's `info` callback, which runs on bind and later property pushes. No `Client` bind or `client.id`
lookup is needed.

## 0017. `audio.apps` push to Lua is deferred, not built

Phase 6 has a dispatchable PipeWire registry listener for per-app streams, but no Lua push: the IPC
socket and Lua VM do not exist, the same gap as ADR-0015.

`supervisor/src/audio/mixer.rs::run` sends each `Vec<AppStream>` snapshot over an unbounded `mpsc`;
`supervisor::main` drains it via `tokio::select!` and logs it with `eprintln!`.

Not built: `audio:set_app_volume`, `audio:set_app_muted`, master `audio.volume`/`audio.muted`,
default sink/source routing, or BlueZ codec control. `AppStream` has no `volume`/`muted` because
nothing populates them.

Since built: the socket, Lua VM, and audio push as a real signal in ADR-0022. Phase 11's minimal
end-to-end slice uses the mixer's data as its first payload. ADR-0037 generalizes it to per-capability
push.

## 0018. Process-group spawn/reap primitives land without `process.run`, a registry, or the swap orchestrator

Phase 7 ships only the two low-level primitives requested. `process.run`'s Lua binding, a process
registry, stdout/stderr streaming, and the generation swap orchestrator are deferred because
their interfaces are not yet defined, as in ADR-0015 and ADR-0017.

1. **`spawn_group_leader(cmd, args) -> io::Result<Child>`.** Spawns an independent process-group
   leader with Tokio's safe `process_group(0)` builder, not the spec's `unsafe { .pre_exec(setpgid)
   }`.
   `0` uses the child's pid as PGID, confirmed in vendored tokio 1.53.1, so no `unsafe` is needed.
2. **`reap_process_group(child, grace) -> io::Result<ReapOutcome>`.** Sends `SIGTERM` to the group
   with `nix::sys::signal::killpg`, waits caller-supplied `grace: Duration` (`DEFAULT_REAP_GRACE`
   names the eventual caller's value, not the spec's hardcoded 100ms), then sends `SIGKILL` to the
   whole process group if needed.
   `ReapOutcome::ExitedCleanly`/`Escalated` exposes the decision.

Both are `#[allow(dead_code)]` in `main.rs`, not runtime-wired. Real OS tests cover a pgid differing
from the test process, clean `SIGTERM` reap, escalation of a `SIGTERM`-ignoring child, and a
backgrounded grandchild in the same group.

Since built: `process.run`'s Lua binding and a process registry in ADR-0026. ADR-0025 first calls
`reap_process_group` from the swap orchestrator.

Amended: the post-`SIGKILL` wait has its own 2s ceiling instead of `grace`. SIGKILL cannot be
ignored, yet at load 200 a killed process outran 100ms and was reported unreapable.

## 0019. Generation swap control-socket transport and Lua AST evaluation are deferred, not built

Phase 8 ships generation swap ordering and gating only, not the surrounding `services.md` § 14
system. These items are deferred for the same reason as ADR-0015/0017/0018: no consumer or
transport exists yet to build against. They are the Unix control-socket wire, Lua AST evaluation,
Renderer null-buffer commit and `wp_presentation_feedback`, NetworkManager/BlueZ state hydration,
true per-(`generation`, output) evidence fan-out (ADR-0003), `services.md` § 14.3 swap messages
(input deselection on N and the promotion signal to N+1), `reload.rs` wiring into `main.rs`, and the
`inotify` config-watch trigger.

1. **Real process lifecycle.** `run_swap` calls `process::spawn_group_leader` during Overlapping
   Spawn and `process::reap_process_group` after evidence verification and on every Candidate
   failure.
2. **`CandidateLink`, the IPC-boundary trait.** Its four `services.md` § 14.2-14.3 operations are
   `push_state_snapshot` (state hydration), `recv_ready_signal` (null-buffer staging),
   `send_activate_draw` (activate draw), and `recv_presentation_evidence` (evidence verification).
   It reuses `shared::StateSnapshot` for hydration, but not `shared::CommandEnvelope` for
   `ActivateDraw`: `lua-api.md` § 7 defines that envelope as a generation-guarded
   Renderer-to-Supervisor Lua write, while activation is a Supervisor-issued nonce, so it carries a
   plain `u64`.
3. **Failure semantics.** Any failure before presentation evidence is verified, including a
   `CandidateLink` error or ready/evidence deadline, aborts the Candidate and leaves generation N
   untouched and authoritative. N is reaped only after evidence verification.

Fake `CandidateLink` tests cover immediate and delayed success, hung calls, link errors, and real
short-lived child processes for both generations. Ten tests cover full success, N remaining alive
mid-verification, a hang at each four handshake steps with the correct `Stage`, and `/proc`
confirmation that an aborted Candidate's process group was reaped.

`reload.rs` only orchestrates generation swaps, not ADR-0001's in-place reload. Since built: the
production `SocketCandidateLink` and control-socket transport in ADR-0025; real `shell.lua`
evaluation from ADR-0023; and a production caller through ADR-0024's Watcher.

## 0020. Control-socket transport ships without dispatch, swap wiring, or `process.run` streaming

Phase 9 builds Unix control-socket framing and connection identity, but defers everything after a
frame arrives: the `lua-api.md` § 3.2 command table of ~30 writes, still forwarded to an
aggregated `eprintln!` channel; ADR-0019's production `CandidateLink`; `process.run` line streaming,
which is a separate transport concern; handshake deadlines, so an idle client blocks only its own
connection task, not the accept loop; real generation IDs (`renderer/src/socket.rs` reads
`OBELISK_GENERATION_ID`, defaulting to `0`, since nothing yet spawns a Renderer with a real one);
reconnection, backoff, and auth, unnecessary for the local `AF_UNIX` socket restricted by filesystem
permissions.

1. **`shared::framing`.** A transport-agnostic 4-byte big-endian length prefix precedes JSON. It is
   generic over `AsyncRead`/`AsyncWrite`, so `UnixStream` and `tokio::io::duplex` share the tested
   path. `MAX_FRAME_LEN` rejects lengths over 16 MiB before allocation, closing the DoS exposed by a
   `u32` prefix on the socket ADR-0005 uses for secure submissions. `shared::ConnectionHandshake`
   (`{ generation_id: u32 }`) is sent first on every connection.
2. **`supervisor/src/socket.rs`.** Binds `$XDG_RUNTIME_DIR/obelisk-shell.sock`, never `/tmp`
   (world-writable). It removes a stale socket left by an unclean shutdown before binding, otherwise
   `bind` fails with `AddrInUse` on every restart after a crash. It accepts unbounded concurrent
   connections for N and Candidate N+1, and registers each `generation_id` in
   `GenerationRegistry` for `GenerationRegistry::send_to`.
3. **`renderer/src/socket.rs`.** Connects as client on its own OS thread with a dedicated
   current-thread Tokio runtime because the main thread is occupied by `wayland::run()`'s blocking
   dispatch loop. It holds the connection after handshake; reading a payload is later work.

Tests use real I/O: framing over `tokio::io::duplex`; a `UnixListener` under
`tempfile::tempdir()` with two distinct generation IDs; and a real Renderer client whose handshake
decodes correctly.

Since built: command dispatch in ADR-0037, with per-module routing replacing the aggregated log;
production `CandidateLink` and swap wiring in ADR-0025; and `process.run` line streaming in
ADR-0026.
## 0021. Lua loader ships without retained-scene reconciliation or signal memoization

Phase 10 builds an `mlua` VM and loader for `shell.lua` node trees and surface topology. It defers
retained-scene reconciliation (`deserialize_lua_table` makes one shallow `VirtualNode`, never
recursing into `children` or matching by identity), socket write dispatch, `textfield`'s
`secure_submit`/`SecureBuffer`, computed-signal memoization/invalidation, property validation,
`obelisk.*` signals, `list` repeaters, `button` input, and a production `Loader` call in `main.rs`.

1. **The type-marshalling boundary** (`marshal.rs`). `check_number`/`check_integer`/`check_string`
   enforce finite `f64` (rejecting NaN/Inf), `i64`/`u64` in `[-2^53+1, 2^53-1]`, and a 64KB
   `String` cap beyond `mlua`'s automatic mapping.
2. **`Signal`/`computed`** (`signal.rs`). `Direct` is pushed; `Computed` holds a Lua closure and
   dependencies and re-runs on `get()`. `computed(dependencies, fn)` passes current values,
   positionally. The 5ms cap uses `Lua::set_hook`, not `Lua::set_interrupt`, because `mlua`'s
   `luau` feature is unavailable in the `lua54` build. An every-1000-instruction hook errors after
   the budget. Since `Lua::set_hook`/`remove_hook` have one unstacked slot per thread and
   `call_with_cpu_cap` is reentrant, `Lua::app_data` holds deadlines: install on 0->1, remove on
   1->0, check the innermost.
3. **Node constructors and `VirtualNode`** (`nodes.rs`). `rect`/`row`/`column`/`text`/`icon`/
   `button`/`list`/`textfield`/`surface` tag props with `kind` and return the table. The loader
   pulls out `kind` and copies other keys into `properties` as raw `mlua::Value`.
4. **`Loader`.** `Loader::evaluate(source)` requires a top-level `surface` or non-empty array and
   returns `LoadOutput { surfaces: Vec<VirtualNode> }`; topology fields remain in each surface's
   `properties` because deserialization does not recurse.

ADR-0023's layout engine built retained-scene reconciliation; ADR-0022 gives `Loader` a production
call site. ADR-0044 decision 3 rejects memoization: every signal re-resolves on every read.

## 0022. Minimal end-to-end slice ships one ad hoc signal, not the `obelisk.*` tree

Phase 11 wires Phase 9's socket to Phase 10's loader with audio-mixer payloads. It adds
`Signal::new_live` and `SignalKind::Live(Rc<RefCell<Value>>)` beside `Direct`/`Computed`; only
`Live` is overwritable through `LiveSignalHandle::set`. `Rc<RefCell<_>>`, rather than
`Arc<Mutex<_>>`, matches `supervisor/src/audio/mixer.rs`: `Loader` stays on the socket-client
thread with no `Send` bound. `Loader::set_global` and `Loader::to_lua_value` expose registration
and JSON-to-Lua conversion.

`handle_snapshot`/`receive_loop` in `renderer/src/socket.rs` convert each `StateSnapshot` to Lua,
push it into global `audio`, and re-evaluate hardcoded `PROOF_OF_WIRING_SHELL` through
`Loader::evaluate`. Decode failure ends this one-sender loop; the Supervisor's inbound
`CommandEnvelope` loop tolerates bad frames.

Not built: full `obelisk.*` namespacing (only `audio`); `expected_revision`/staleness rejection
(ADR-0004), safe on one ordered Unix-socket connection; generation-ID assignment (both sides
hardcode `0` from shared defaults, no handshake); reconnection (a dropped socket ends the thread,
per ADR-0020); real `shell.lua`; work past `LoadOutput`; capabilities besides `audio::mixer`; or
write dispatch (Renderer -> Supervisor remains push-only).

## 0023. Layout engine ships a stacking model, not a full constraint solver

Phase 12 built `renderer/src/layout/`: validated parsing in `node.rs` and
`resolve_and_reconcile` in `scene.rs`, combining constraint-down, size-up and position-down.

`rect`(with children)/`button`/`surface`(with `child`) resolve children against the full content
box by `align_h`/`align_v`; siblings may overlap. Only `row`/`column` implement intrinsic-size
formulas. Reconciliation matches children by parent-local position and reuses `NodeId` when kinds
match; top-level surfaces use `id`. Removed subtrees tear down child-first into `retiring`. A
`Fill`/`Percent` child of a `Content`-sized `row`/`column` gets a `0.0` axis budget when the
parent's size is unknown, matching CSS.

Rejected: a full two-pass constraint solver because this model covered the required shapes.

Not built: `list`/`textfield`, confirmed `Percent` syntax, live `wl_region` push, real per-output
pixels, GPU resource behind the retained-scene lease, shared `ShapingHandle` (a second duplicated
startup cost), paint pipeline, or recursion-depth limit.

Amended by ADR-0044: `Signal`-valued geometry, originally rejected outright, now resolves at layout
time; a live push marks the scene dirty instead of triggering re-evaluation.

Amended by ADR-0045: positional matching loses identity when a node is inserted above a sibling.
Parent-scoped `id`s pair identified children first; the rest use positions. Deferred `list` support
requires `key`.

Superseded by ADR-0077: taffy owns layout math.

ADR-0143 supersedes the retained-subtree lease bag and child-first teardown contract.

## 0024. In-place reload: the Renderer self-diffs topology, the Supervisor only dispatches

Phase 13 adds a Supervisor-side `inotify` watcher on `~/.config/obelisk/`. After fixed 200ms
(`RELOAD_DEBOUNCE`) debounce, the loader re-evaluates. Renderer returns `Unchanged`,
`TopologyChanged` or `Failed`; Supervisor dispatches.

Renderer owns classification because it has applied and fresh topology; Supervisor owns swap-vs-in-
place dispatch. `ReevaluateRequest { sequence }` goes Supervisor -> Renderer;
`ReevaluateReport::{Unchanged, TopologyChanged, Failed} { sequence, .. }` and
`ApplyPendingReload { sequence }` return Renderer -> Supervisor. Both guard `sequence`: Renderer
applies only matching `ApplyPendingReload`; Supervisor's `answer_unchanged_report` accepts `Unchanged`
only for the latest `Reevaluate`, closing the race where a superseded report could otherwise fire
`reset_registrations`/`ApplyPendingReload` after a newer edit had landed. `TopologyChanged` never
stashes a scene; only a swap may mutate that generation.

`applied_topology` is `Option<Vec<SurfaceTopology>>`: `None` means nothing applied; `Some(vec![])`
is a real zero-surface generation, so failed startup cannot make later fixes look `TopologyChanged`
against empty topology.

The debounce stores an absolute `Option<Instant>` deadline, not relative sleep, so directory events
cannot extend it. Filter `shell.lua` by name; atomic-save editors unlink and recreate its inode.

Rejected: reusing `shared::CommandEnvelope`. `SupervisorFrame`/`RendererFrame` are adjacently
tagged because `CommandEnvelope` carries a Lua-initiated Renderer -> Supervisor write, not an
engine-internal message.

Not built: generation swap on `TopologyChanged` (`run_swap` has no runtime caller, per ADR-0019);
useful `reset_registrations` (called but empty; ADR-0006 requires reset before fresh evaluation,
but this round trip cannot honor that ordering because the reset decision depends on the evaluation's
verdict; it needs the same pending/apply staging the Scene already has once a real registration
capability exists to stage against);
full `obelisk.*` tree (only `rescue`, as in ADR-0022's `audio`); startup fallback;
multi-generation bookkeeping (generation `0`, per ADR-0020); or real `layer`/`anchor`/`monitor`.

## 0025. Swap orchestrator wired with atomic per-candidate promotion, not true per-output streaming

Promotion is atomic per candidate. Evidence is per surface, and every expected surface must report
within one shared timeout. Otherwise the candidate is aborted and no swap occurs. Partial promotion
was rejected because aborting after two of three outputs transferred would black out those outputs.
This implements ADR-0019 item 5 only partially, not ADR-0003's independent timing.

The handshake stages null buffers, sends nonce-bound `ActivateDraw`, then collects
`wp_presentation_feedback`; wrong generation, type or nonce is dropped. Send input deselection,
promotion and reap in that order because the candidate link owns one connection. Timeouts: 2 seconds
readiness, 3 seconds evidence, 100 ms reap grace.

Rejected: hand-written presentation dispatch. SCTK 0.21.1 supplied it; the Renderer only correlates
feedback. Discarded feedback waits for the evidence timeout.

Not built: scene-to-GPU rendering, arbitrary surfaces, input/promotion effects, concurrent handshakes,
immediate failure on discarded feedback, installed binary lookup, or NetworkManager/BlueZ hydration.
The proof uses fixed surfaces and PipeWire or empty state.

Closes ADR-0019 items 1, 3, 6 and 7; item 5 remains partial and item 4 remains open.

## 0026. `process.run`'s Lua binding and piped stream registry ship; `textfield`/PAM stay deferred

Phase 15 implements the Lua process binding and non-blocking stdout/stderr piping, closing
ADR-0018 items 1–2. Textfield and PAM remain deferred; ADR-0027 corrected the dependency claim,
not the scene node or PAM design.

Commands reuse `CommandEnvelope` with capability `process` and actions `run`/`kill`. The Renderer
assigns monotonic IDs and returns a handle before a socket round trip; Loader and dispatch loop
share a thread and queue.

The Supervisor tracks children by generation and command ID without a spawn-registry mutex. Piped
spawning is separate from inherited-stdio spawning because Renderer generations need inherited stdio.
Missing or exited kill targets are no-ops. Superseding a generation reaps children without exit
events to its closed connection.

Callbacks are `out_cb(line, stream)` with `stdout`/`stderr`, and `exit_cb(code)` with integer or nil.
Output and exit use separate wire events. EOF is not process exit; inline waiting could wedge the
Supervisor, so removal is synchronous and waiting/reporting detached. Malformed runs and failed
kills report nil exit codes so callbacks do not leak.

Rejected: the broader claim that the codebase has no shared mutexes. The socket generation registry
already has one; only spawn tracking avoids it.

Not automated: the three-process Supervisor/Renderer/child workflow over a socket.

## 0027. Textfield wire shape: secure submit frame and text-input bridge

This entry proposes the text-input wire shape, not a completed implementation. Ordinary fields were
designed around `zwp_text_input_v3`; masked secure fields use `wl_keyboard` directly.

1. **Correction to ADR-0026.** `wayland-protocols` exposes `text_input::zv3` through its unstable
   feature; the missing pieces were the service and scene node.
2. **Seat binding.** Use one SCTK-bound `wl_seat`; no multi-seat support.
3. **Ordinary submission.** The original proposal named text-input `ACTION_SUBMIT` instead of a
   keyboard listener for IME-correct submission. Neither protocol nor implementation supplies it.
4. **Secret wire shape.** `RendererFrame::SecureSubmit` carries generation, capability, action and
   secret bytes. Generic JSON arguments were rejected because they retain plaintext outside
   `SecureBuffer` zeroization. Zeroize the source after building the frame.
5. **Cross-thread bridge.** The proposed edit diff carries commit/preedit text, delete lengths and
   a submit flag over the Wayland-to-socket channel pattern.

Amendment: secure fields bypass text-input entirely. Without compositor-side IME, no
`commit_string` arrives, and passwords stay out of IME candidate text. Lua cannot read their values;
their submit callback is argument-free.

Not built: `TextInputService`, textfield scene node, seat binding, secure frame dispatch or the
cross-thread channel pair.

## 0028. PAM: nonstick, a re-exec worker subprocess, one-shot protocol

PAM runs in a re-executed worker with one password captured before spawning.

1. **Crate: `nonstick`, not `pam-client`.** `pam-client`'s last release was July 2022;
   `nonstick` provided a maintained programmatic conversation API without terminal I/O. Its
   `OsString` and PAM's C copies cannot be zeroized by the source buffer; construct that copy last
   and zeroize the source. The machine lacked `polkit-1` PAM config; fallback: `login`.
2. **Isolation: re-exec, not fork or a third binary.** PAM modules cannot reliably be cancelled
   without terminating their process. Forking the multithreaded Supervisor risks inheriting locked
   mutexes. Re-exec the Supervisor with `OBELISK_PAM_WORKER=1` before D-Bus/tokio/audio setup; reuse
   process-group spawn/reap helpers.
3. **Protocol: one-shot, not interactive.** Write the captured password once to stdin and close
   the pipe; every PAM prompt receives it. A worker stdout frame distinguishes Success, StartFailed,
   AuthFailed, MaxTries, PamError and OtherError. Interactive relaying was rejected as unnecessary;
   Supervisor/Renderer frame reuse belongs to another protocol.

Still open: parse Polkit identities into the uid/Identity required by
`authentication_agent_response2`.

Not built: worker entry branch, PAM conversation, one-shot framing or identity parsing. Framing was
to reuse shared JSON-frame helpers.

## 0029. NetworkManager: capability-tagged state snapshot and secure connect flow

1. **Capability tagging.** Add a capability name, track revisions per capability, and hydrate its
   Lua signal. Keep payloads generic JSON until typed payloads are needed. Closes ADR-0022 item 1.
2. **D-Bus access.** Choose `rusty_network_manager` over hand-written proxies because it covers the
   interfaces with a compatible zbus dependency, following ADR-0013.
3. **Listener architecture.** Merge D-Bus streams into the async event loop. Unlike PipeWire, no
   callback thread is needed. Spawn scan/connect/forget writes so a hung remote call cannot wedge
   the Supervisor.
4. **Password via secure submit.** Reject the IDL's plaintext password argument. `connect(ssid,
   hidden)` stores one pending intent, followed by native secret submission. Empty means open;
   nonempty populates WPA-PSK. A password field remains necessary for open networks because hidden
   networks do not advertise security flags.
5. **Ethernet toggle.** Disconnect wired devices when disabled; activate existing autoconnect
   profiles when enabled. Missing profile is a no-op; software cannot fabricate link carrier.
6. **Push cadence.** No debounce. Rebuild on every relevant event; coalesce only if measured bursts
   justify it.

Still open: exact state struct, forgetting every matching saved profile rather than only the first,
and scan options, empty by default.

Amended by ADR-0212: item 2 is reversed, and the proxies are hand-written.

## 0030. BlueZ controller: hand-written proxies, Just-Works-only pairing, deferred codec control

1. **Proxy choice.** Reject `bluer` for a second D-Bus stack; reviewed zbus alternatives were
   unmaintained or unreviewed. Hand-write Adapter1, Device1, Battery1, Agent1, AgentManager1 and
   ObjectManager bindings.
2. **Pairing policy.** Register a default `NoInputNoOutput` agent. Reject PIN code, passkey requests
   and PIN display, so legacy PIN-only devices cannot pair. Auto-accept confirmation, passkey
   display
   and authorization because no confirmation UI exists. Cancel/release are no-ops. No PIN/passkey
   UI.
3. **Codec control deferred.** Switching needs PipeWire device profiles and `SPA_PARAM_Profile`,
   not the proposed route parameter. The audio thread lacks an inbound command channel; design it
   under audio ownership, not Bluetooth.
4. **Device tracking.** An object-path-keyed registry follows ObjectManager additions/removals and
   Battery1 changes. Hydrate once, keep a property forwarder per device, and abort it on removal.
   Scan-and-replace cannot represent these lifetimes.
5. **Category from Class, not Icon.** BlueZ Icon may be empty. Map computer, phone, headset,
   headphone and keyboard/mouse from major/minor class bits; combos use keyboard. Unknown and
   other audio/video classes stay generic.
6. **No debounce.** Connection and battery events do not justify it.
7. **Discovery list lifetime.** Clear on start, preserve on stop. No cap for short sessions.
8. **One adapter.** Use the first found; config has no adapter selector.

Reuse ADR-0029's capability snapshot plumbing.

Not built: codec selection, PIN/passkey UI or multiple adapters. Codec selection shares audio's
missing inbound-channel prerequisite with app volume/mute controls.

Items 2 and 3 are superseded. The agent now registers `DisplayYesNo` and holds each confirmation,
authorization and service request for the user through `pairing_request` and
`bluetooth:answer_pairing`, refusing devices that are not invited (adapter visible, or this shell
pairing them). Codec selection now lives in audio: `audio:set_bluetooth_profile` switches a BlueZ
device's PipeWire card profile. PIN and passkey entry are still refused.

## 0031. Tray controller: hand-written SNI/DBusMenu host, IconName preference, no cache-busting

1. **Proxy choice.** Reject `system-tray`: coupled Watcher/Host registration, verified pixmap
   height-index bug, and raw bytes needing validation and encoding. Hand-write the small interface
   and recursive menu bindings.
2. **Watcher/Host registration.** Request the watcher name without replacement or DoNotQueue. Treat
   NameTaken as success and register our Host against the existing owner for standalone use or
   desktop coexistence.
3. **Registry identity.** Resolve the caller's service to a D-Bus unique name before using it as key
   or spool filename. Raw service strings permit filename injection/path traversal. Historical
   spool:
   `/dev/shm/obelisk-$UID/tray/{sanitized_unique_name}.png`.
4. **Icon preference.** Pass IconName to the Renderer; decode only as fallback. Choose the largest
   pixmap up to 128 px, with no minimum; Lua owns display size.
5. **Menus.** Fetch full layout at registration and LayoutUpdated. Refresh lazy submenus through
   AboutToShow before rendering, avoiding empty menus.
6. **Click semantics.** Gate item-is-menu centrally: Activate no-ops for menu-only items; menu
   selection sends DBusMenu's clicked event.
7. **Deferred actions.** Do not build SecondaryActivate, ContextMenu or Scroll because no known
   consumer needs them; modern items supply a Menu.
8. **PNG encoding.** Choose encode-only `png` over `image`'s unused decoding machinery.

Reuse ADR-0029's capability snapshot plumbing.

Amendment, ADR-0054: the spool overwrites in place, but Renderer texture keys use path, modification
time and length, fixing stale icons without changing the no-spool-suffix decision.

Amendment to decision 5: `RegisterStatusNotifierItem` replies once the sender passes the check, and a
spawned task fetches the item, as KDE's and Quickshell's watchers do. `TrayController::new` no longer
awaits host registration or adoption, which froze the Supervisor when a tray app raced it at boot.

## 0032. Idle capability splits transport but keeps one controller

One Supervisor controller owns Wayland idle notification and logind inhibition. One idle-notify
listener per duration fans out registrations; reload cleanup uses generation reset. Use
get_idle_notification; no presence-sensor exclusion. Deliver idled/resumed through IdleEvent,
because these are edges, not revisioned state.

Use logind `Inhibit(what="idle", who="obelisk", why=reason, mode="block")` on the system bus. The
fd releases the hold after a crash. It covers automatic idle actions, not explicit sleep, shutdown
or lid-switch. Refcount generation holds; reset clears the count.

Rejected: Wayland idle-inhibit. It splits ownership and requires a surface the Supervisor does not
own.

Enable wayland-protocols' staging feature for idle-notify. Missing protocol or a failed dedicated
Wayland connection yields a logged inert notifier. Inhibition still uses the system bus; requests
may fail.

## 0033. Notifications advertises a real capability set, with Lua-configured sound and DND

Advertise action-icons, actions, body, body-hyperlinks, body-images, body-markup, icon-static,
persistence, sound and inline-reply. Exclude icon-multi because Notify has no multi-size shape;
inline reply follows KDE `x-kde-reply`.

Keep allowlisted bold, italic, underline, link and image spans. Image paths and action icons require
absolute regular files under allowed system/user icon directories and within the image-data cap.
Bare theme names and arbitrary markup fail. Renderer span support and full theme lookup are outside.

Keep two-argument ActionInvoked. Encode reply text as `inline-reply::<text>`; a bare key is
malformed. A third argument was rejected because it breaks client introspection.

Amended: that encoding reached no consumer. KDE `x-kde-reply` defines
`NotificationReplied(uint32 id, string text)` in Plasma 5.18; clients listen for that signal.
`reply` honours `resident` and emits `NotificationClosed(id, reason=2)` when it removes, as
`invoke_action` does. `ActionInvoked` keeps two arguments and its introspection.

Sound priority is suppress-sound, trusted sound-file, configured urgency sound, then silence. Ignore
sound-name without theme resolution. Playback uses an internal PipeWire channel. DND is Supervisor-
global, gates only sound and resets on restart. Critical notifications bypass DND and automatic
expiry; Lua owns popup policy.

Amended: sound-name resolves to the freedesktop theme's `sounds/freedesktop/stereo/<name>.oga`, only
in place of a configured urgency sound, so a config that registers none plays only a client's
sound-file. set_sound, sound-file and sound-name share sound roots (`/usr/share`, `/usr/local/share`,
`/opt`, `$XDG_DATA_HOME`), kept apart from the icon roots. Playback decodes Ogg Vorbis, what the
theme ships, and 16-bit PCM WAV, what Telegram's sound-file is, picked by header and capped at 4
MiB, 30 seconds, two channels and 8-192 kHz; one sound waits behind the one playing and later ones
drop, and playback gives up after the sound's length plus two seconds.
`set_quiet` gates non-critical sounds like DND without changing it, so a config can stay silent while
locked or blanked. `set_app_muted` silences all sound from an app that plays its own, matched exactly
on desktop-entry or app name. The app's stream starts after ours, so nothing can detect it.

Use snapshots because each mutation changes feed or DND state. A 20-entry feed views a 100-entry
FIFO so actions resolve outside the feed. Replacement without a fresh image deletes the old spool;
eviction deletes the evicted image. Historical spool:
`/dev/shm/obelisk-$UID/notifications/notif-{id}.png`.

Add reply, per-urgency sound and DND writes alongside dismiss; expose urgency, reply availability,
and structured body spans.

## 0034. Keyboard backlight, locks, layout, camera privacy, and Arch update checking

Add domain-named hardware capabilities rather than a generic adapter abstraction before any caller
needs polymorphic dispatch.

### 0034.1. Keyboard backlight rides UPower, not sysfs

Use the fixed UPower keyboard-backlight object. It was verified on the development machine, not
universally.

1. Convert cached raw steps to rounded, clamped percentages and back.
2. Missing hardware yields -1 and no-op writes, with one diagnostic.
3. Reuse the system-bus connection.

### 0034.2. Keyboard lock state uses evdev, keyboard layout gets its own narrow compositor trait

1. Evdev supplies initial/live lock LEDs; sysfs is a static fallback. Physical Caps Lock changed
   sysfs but emitted no inotify events. Missing access defaults false with a diagnostic.
2. Keep a narrow keyboard-layout trait for Hyprland/niri, not a workspace abstraction.
3. Select one primary keyboard and expose index-based switching; Lua computes cycling. ADR-0205
   keeps the one-keyboard selection and corrects what "primary" means on Hyprland.
4. Correction: niri reports layout index directly; Hyprland lacks reliable name-to-code correlation.
   Hyprland read-back remains last-known, a cycling gap. Superseded by ADR-0205: 0.56.2 reports the
   index itself, and its `main` keyboard is the one being typed on.

### 0034.3. Camera privacy: kernel-level detection primary, PipeWire supplementary

1. Detect raw V4L2 opens through device events plus fd inspection. The proposed streaming sysfs flag
   was absent on the webcam despite a recent kernel.
2. Use PipeWire only for application-name enrichment; raw camera clients never appear there. Fall
   back to process names.

### 0034.4. Arch update checking uses the `alpm` crate, not `checkupdates`/`expac` subprocesses

A user-owned database prototype synced as uid 1000 without fakeroot, downloading 8.9 MB and finding
the same three updates as checkupdates; this was a behavior test, not an independent C-source audit.

1. Keep updates separate from sysinfo scheduling; interval zero suspends it.
2. Publish packages, versions, sizes, last success and errors, not only a count.
3. The capability owns privileged installation and progress, reusing process helpers and Polkit.

Reject combining these unrelated hardware mechanisms into a flat capability bucket.

## 0035. Sysinfo capability: five IDL fields, two hwmon preference lists, watch-driven suspend

Expose five IDL fields through three independently scheduled tasks, with their own intervals.
RAM/swap share one read/interval; CPU/GPU temperatures share one scan/interval.

CPU uses busy/total procfs deltas, counting idle and iowait as idle; discard the prior sample on
resume so the first result is not an average across the dormant period. RAM uses MemAvailable; swap
uses SwapFree.

Resolve hwmon chips once because onboard sensors do not hotplug. CPU preference is k10temp, then
coretemp, with acpitz fallback; GPU preference is amdgpu, nouveau, then nvidia. Sort core
temperatures by core index; exclude package aggregates, Wi-Fi, NVMe and battery sensors. No matching
GPU returns -1, not zero.

Intervals start at zero. Zero awaits configuration changes with no timer or wakeups. Three producers
update fields under one mutex and signal one push channel; each push bumps the capability revision.
No snapshot precedes a real sample.

Configure takes whole-second interval fields. Missing keys preserve values; any wrong-typed present
key rejects the whole call. Scheduling belongs to the Supervisor, not a generation.

Parameterize procfs and hwmon roots for temporary-directory tests. Split CPU, RAM, temperature and
controller modules by concern; no speculative cross-controller trait.

## 0036. Mpris capability: playerctld excluded, track-identity caching, strict seek state

1. **Discovery filter.** Exclude the exact playerctld bus suffix because its Identity duplicates the
   proxied player; exclude CanControl=false at registration. Other clients may use playerctld.
2. **Selection policy.** Publish all players; Lua chooses the display. No native active player.
3. **Album art.** Accept canonicalized existing file URLs without a directory allowlist; player
   caches vary. No spool copy, HTTP fetching or network image loader.
4. **Identity and unavailable length.** Use the bus suffix as ID and reconstruct it on writes. Missing
   or wrong-typed length is -1.
5. **Seeking.** Use SetPosition with a known track ID, otherwise relative Seek. Clamp the target to
   cached bounds in the Supervisor. Live mpv-mpris testing accepted a wrong track ID, so the
   upstream staleness guard was insufficient. Update position only from real signals.
6. **Metadata caching.** Track identity combines track ID, URL and title. A change starts a new
   track; unchanged identity preserves art/length when updates omit or malform them.
7. **Failure and discovery.** A transient read failure degrades the field, not the player. Discover
   through startup ListNames and NameOwnerChanged.
8. **Ownership.** One capability with per-player producers sharing a state mutex, following
   ADR-0035. Split watcher, player and controller on the session bus.

## 0037. Capability roster: generic push, per-module dispatch, no merged channel

The 2026-08-27 review found contained capability logic but duplicated snapshot/dispatch edges, with
four Renderer seeds for nine snapshot capabilities.

1. **Generic push.** Replace per-capability snapshot helpers with one Serialize-based helper; the
   capability name selects the payload, following ADR-0029.
2. **Shared roster.** Pre-seed one nil-valued Lua signal per roster entry. Assert roster membership
   on Supervisor pushes to catch omissions before config boot. Unrostered names retain lazy lookup.
3. **Module-owned dispatch.** Each capability parses arguments, selects actions and spawns work;
   main keeps a literal match. Controllers own network/Bluetooth state and pending network intent;
   scan-start/discovery-clear events use normal channels for FIFO ordering.

Rejected: merging single-variant capability channels. One select arm per capability was cheaper than
controller serialization plus permanent idle/audio exceptions. Revisit if a producer cannot reach
main's select.

Amendment, ADR-0076: the shared Capability enum replaces the string roster and membership assert;
one exhaustive capability-module match replaces main's string match. Channel rejection remains;
ADR-0070's lazy-start wrappers had grown each one-line select arm to six lines.

## 0038. Surfaces come from `shell.lua`, not a fixed role enum

Delivery depends on sharing the scene and Wayland thread in ADR-0039.

1. **Declarations are the source.** Remove the fixed SurfaceRole enum and creation calls. Bar,
   overlay and wallpaper names become config IDs.
2. **Fixed declared set per generation.** Objects are created at startup. Adding/removing
   declarations or changing layer/anchor/monitor/namespace requires a swap. Visibility,
   protocol-mutable margins,
   exclusive zones, keyboard interactivity and size update in place. Object lifetime was amended.
3. **Per-output instances.** Expand declarations to `{id}@{output}`. Monitor hotplug adjusts
   instances without a generation swap because the declaration is unchanged.
4. **Role properties.** Add namespace for compositor rules, keyboard interactivity for typing, and
   margin for edge offsets. Padding cannot replace margin.
5. **Input regions.** Keep the bounding-box union per surface when content is smaller.

Rejected: one fixed popup overlay. It cannot provide independent namespaces, keyboard focus,
layering, per-output content or exclusive zones; zero-overhead did not justify it.

Amendments: ADR-0078 adds exclusive Ignore without changing the in-place property list. ADR-0049
makes popup/window visibility create/destroy protocol objects because popup creation needs an input
serial and consumes its positioner. ADR-0088 applies it to panels because layer-shell remapping
failed. The declared set remains fixed.

Scope reversals: ADR-0040 adds deferred window/popup roles and corrects the claim that click-outside
dismissal has no portable answer, using popup grabs. ADR-0042 reverses the
Supervisor-owned lock-client plan: the compositor stays locked after client death and hides
non-lock surfaces, so an ordinary Renderer surface cannot paint the lock UI.
## 0039. The Lua VM, retained scene, and paint pass share the Wayland dispatch thread

Move Lua, Loader, retained Scene and painting to the Wayland dispatch thread. Lua and its values
make the scene non-Send; it must share the GL context's thread. The socket thread becomes framed I/O
forwarding only.

1. Construct Loader, signals, rescue state and Scene on the Wayland thread, not by cross-thread
handoff.

2. Replace readiness/presentation/activation channels with direct calls; secure submission sends an
outbound frame.

3. Share one ShapingHandle between sizing and painting, avoiding two roughly one-second FontSystem
startups; shaping stays off-thread.

4. Delete placeholder output sizes and use real per-surface sizes. Deferred as amended below.

5. Give overlay input-region calculation its production caller, following ADR-0038 decision 5.

Amendment: decision 4 needed ADR-0038's unified surface IDs, not just shared-thread access, so it
moved to Phase 20. Decisions 1–3 shipped with this refactor; 5 was unaffected. The claim that the 5
ms Lua CPU cap was already enforced was also wrong: pcall could catch hook errors and coroutines
escaped per-thread hooks. Those gaps were found in review and were being closed.

Trade-off: slow config evaluation now blocks Wayland dispatch. Off-thread shaping and evaluating
only on config edits limit that cost; snapshot pushes do not rerun the config. The CPU-cap
qualification above remains part of that assessment.

Rejected: ship resolved trees between threads. It adds bidirectional crossings to input and surface
creation, must resolve/drop Lua values and reopens cache invalidation. Evaluation errors already
reach rescue in-process. Moving EGL to the socket thread merely moves the same lifetime problem
because configure events and EGL surface lifetime depend on Wayland dispatch.

Scope: thread ownership only. Painting, declared-surface management and input remain separate work;
the refactor initially retains the same fixed surfaces and handshake.

## 0040. Four surface roles: panel, window, popup, lock

Replace ADR-0038's window/popup non-goals with four Lua constructors matching Wayland roles.

1. **Separate constructors.** Panel, window, popup and lock map to layer-shell, xdg_toplevel,
xdg_popup and session-lock surfaces. Reject a shared kind-discriminated schema because most properties
are disjoint. Rename surface to panel without an alias; surface remains the umbrella term.

2. **Native popups.** Parent to a panel or window before first commit, using the null-parent
creation path. A real popup grab provides focus and click-outside dismissal, correcting ADR-0038.
Denied grabs are normal; grabs require a real input serial before mapping. Destroy nested popups in
reverse order.

3. **Click-derived positioning.** Pass the clicked node's parent-surface-relative rect to Lua. Popup
size and anchor rect must be nonzero. Defaults are flip-y and slide-x; protocol precedence is flip,
slide, then resize. No new node identity mechanism is needed.

4. **Reuse staging.** Windows follow null-buffer commit, configure, ack and attach like panels.
Window state arrays and xdg_surface acknowledgments differ; min/max hints are advisory and fullscreen
configure is binding. Expose title, app ID, size hints and a declinable close callback. Request server
decoration but build no client frame.

5. **Use SCTK with one escape hatch.** SCTK 0.21.1 wraps the required shell/window/popup/positioner
operations. Reach through to xdg_popup.grab where it lacks a wrapper; the engine owns bookkeeping.

Lock-client process ownership is deferred to the ADR-0010 reconsideration. Content requires the
paint pass and declared-surface manager; popups also need input routing. Adding/removing any role
still requires a topology swap, with instancing following ADR-0038.

## 0041. `obelisk.screens` is Renderer-sourced; variants are a Lua loop

1. Lua loops already provide per-screen iteration; no variants/repeater constructor.

2. Screens are a Renderer-local signal from existing output bindings, not a second Supervisor
geometry source.

3. Identity is the declared ID set. Monitor All changes instances in place; explicit loops can change
IDs and require a swap.

4. Hotplug reuses evaluation, topology comparison and rollback from the file-edit reload path.

Screens own geometry; workspaces reference connector names without duplicating it.

## 0042. The Renderer holds `ext_session_lock_v1`; the Supervisor supervises the lock client

Supersedes ADR-0010's lock-client ownership, not its idle-notify ownership.

The Renderer must hold the lock to paint its connection-scoped lock surfaces. The compositor stays
locked after client death. The Supervisor retains lock decisions, authentication and client
supervision; secrets use secure submission to the PAM worker.

No overlapping generation swap while locked; topology edits queue until unlock, but in-place reloads
continue. Maintain one lock surface per output. Handle denied and subsequently finished locks
distinctly; only successful authentication permits unlock, with a display sync before exit.

Recovery after client death depends on compositor lock-restore policy; respawning is not a portable
guarantee of recovery.

## 0043. Memory budget: declared fonts, atlas eviction, and PSS as the measurement

Target: 50 MB PSS per monitor for shell processes at steady state, not a measured achievement.

1. Report three measurements separately:
   1. Steady-state summed PSS across Supervisor and Renderers against the budget.
   2. Per-Renderer USS with private clean and dirty pages.
   3. Handoff peak while both generations live, not folded into steady state.
   GPU memory comes from DRM fdinfo, not smaps; deduplicate by device/client ID.

2. Load only config-declared font families and fallbacks at startup. No per-node family or live chain
reload; missing coverage can render tofu. Lazy system discovery is an upgrade.

3. Clear the whole glyph atlas above a page threshold while idle, then rebuild on demand. Pages are
512×512 RGBA8, 1 MiB each; no per-glyph eviction API exists.

4. Size buffers to surfaces. A 2560×1440 RGBA8 buffer costs about 14 MiB before double/triple
buffering.

No allocator replacement before measurement.

First measurement, niri/i915, one 1920×1200 output, Mesa 26.2.1: 149.7 MiB steady PSS, 196.9 MiB
handoff, exceeding the target. Renderer PSS was 137.0 MiB with declared fonts versus 2207.9 MiB with
2648 system faces. LLVM accounted for 82.3 MiB; mapped fonts only 0.1 MiB. The system-font fallback
remained a hazard. Atlas growth was not tested by this short run. Whether to exclude driver
libraries from the target remained undecided.

## 0044. Signals resolve at layout time, and a push marks the scene dirty

1. Resolve property signals at layout time; passing a handle is reactive, calling get during
evaluation is a snapshot. Nil uses the property default; topology fields reject signals.

2. A push marks one scene-wide dirty bit and reapplies the retained tree without rerunning Lua.
Per-surface invalidation is an upgrade only if profiling justifies a dependency graph.

3. No memoization or dependency graph. Value-cloned computed dependencies can grow exponentially;
shared identity would be needed for caching, and the evaluation CPU cap bounds the work.

4. Keep one Lua VM per generation and drop retained values before it. The corrected reason is weak
Lua references and invalid-state access, not a refcount leak. In-place reload does not reset it.

5. Named writable state survives in-place reload, not a generation swap. Changed scalar seeds reseed
it; unchanged seeds preserve runtime writes. Fresh table identities do not count as edits.

Rejected: rerun config on each push, which adds evaluation cost and changes callback identities.
List expansion remained deferred in this pass.

## 0045. Nodes reconcile by scoped `id`, and `list` items by `key`

1. Optional node IDs are parent-scoped reconciliation hints, not global addresses. Duplicate sibling
IDs are errors.

2. Match explicit IDs only to the same ID; anonymous nodes match only anonymous nodes by position.
Unmatched IDs never inherit positional nodes. Retire unclaimed subtrees child-first.

3. List keys are optional functions of source elements returning strings. Duplicate keys are errors;
no key means index matching, with rebuild cost after insertion.

List implementation was still deferred. Top-level surface IDs remained required and unique.

ADR-0143 supersedes decision 2's child-first retirement requirement; identity matching stays.

## 0046. Rescue renders out of band when no scene survives

1. Reload failure retains the working scene and reports through obelisk.rescue. Startup failure has no
scene and needs an independent display path.

2. The Supervisor re-execs a rescue process with hardcoded Rust drawing, no Lua, capabilities,
generation ID or authority; reap it once a real generation presents.

3. Display the error and config location only, not a fallback shell or recovery UI.

Rejected: a default Lua config depends on the failing machinery; stderr alone is invisible to users
launching from a session.

## 0047. The config is a directory, not a file

1. Restrict require to the config directory's ?.lua and ?/init.lua paths, not system Lua modules;
native module loading remains disabled.

2. Clear required-module caches on re-evaluation because the generation's VM survives reload.

3. Recursively watch Lua files and filter unchanged content hashes. Watching only successfully loaded
modules would prevent recovery from a broken first evaluation.

Lua's module cache provides singletons; parent-scoped IDs make modules reusable.

## 0048. The config VM drops the blocking parts of the Lua stdlib

Use an explicit Lua library set. Omit io and os, then restore only time, date, clock and getenv.
Keep coroutine/string/table/math/utf8; debug, FFI and native module loading remain unavailable.

Blocking syscalls stall Wayland dispatch and evade instruction-based CPU limits. Process commands
must use the managed callback API; os.exit would terminate a generation outside its lifecycle.
Direct file I/O is a deliberate loss, covered for now by require or subprocess helpers.

Rejected: merely document the hazard, or restore a separate Lua thread just to accommodate it. A
native asynchronous file reader waits for a caller.

## 0049. Popups and windows are created when shown, not at generation startup

1. Separate declaration lifetime from protocol objects: panels originally lasted a generation, locks a lock
session, and popups/windows only while shown.

2. Visibility creates/destroys popup/window objects through dirty re-resolution.

3. Opening a declared popup is a value change, not topology; unopened declarations allocate no
Wayland/EGL objects.

No live popup repositioning; recreate per open. Destroy nested popups child-first. Amendments: keep
the input serial armed through the poll-loop apply, then disarm it. A requested grab without a
serial refuses creation. Build PopupSpec from resolved properties so anchor signals follow clicks,
while validating literal mistakes during evaluation.

## 0050. Pointer hit-testing walks a path, a click is press-and-release on one node, and focus attributes the secret

1. Hit-testing returns the ancestor path. Half-open bounds and ancestor clipping must agree with
painting. Accumulate parent-relative rects for surface-local coordinates.

2. Press arms; release must match surface, rect and button. Moving the target cancels the click.
Support left/right/middle, not an ambiguous catch-all button.

3. Call on_click(rect, button) with surface-local logical geometry and a button-name string. Config
routes the rect to its chosen popup. Callback errors are logged, not fatal.

4. Focus selects the secure submission target. No focused field means no secret frame; zeroize either
way. Leaving focus clears it. All supported pointer buttons qualify as real input.

No portable panel click-outside grab. Existing button-agnostic callbacks now also receive
right/middle clicks; left-only policy belongs to config.

## 0051. A popup anchors to one parent instance, and a compositor dismissal latches

1. Anchor to the parent instance receiving the arming click. A non-grabbing open without a click uses
the first parent instance; never expand one popup across every monitor.

2. Compositor dismissal drops the handle, calls on_dismiss and latches recreation until new pointer
input. A false/true visibility cycle within one batch cannot be the latch.

3. A requested grab without a serial means no popup, not a silently ungrabbed popup. Compositor denial
follows normal dismissal handling.

Drop nested handles child-first; latches die with the generation. Known limit at delivery:
release-triggered grabs work on niri but fail wlroots serial validation on sway/Hyprland. An
on-press hook was not yet available.

## 0052. The session lock is commanded through a capability, and its surfaces live for the lock

1. Expose lock through generic capability invocation, with no Lua unlock action. Only PAM success
authorizes unlock; a script-callable unlock would bypass authentication.

2. Declare one root lock node. Retain it from startup, but create per-output protocol surfaces only
for the lock. Visibility, monitor selection and geometry are protocol-owned.

3. Refuse acquisition without a working lock tree and exactly one lock/authenticate secure field.
Veto in-place edits removing that field while locked; otherwise the user can be stranded.

4. Acquisition failures use rescue; authentication failures use lock state with an attempts counter
so repeated identical errors remain observable.

Secure fields read the keyboard directly. The Supervisor returns an authenticated unlock command;
the Renderer does not interpret PAM outcomes itself. Compositor-initiated teardown uses the
protocol's legal unlock-and-destroy verb without initiating an unlock. Idle callback delivery and a
built-in fallback lock screen were not built in this pass.

## 0053. Five specified capabilities were never given a phase, and a bar is what found them

A real bar exposed capabilities specified without implementation phases.

1. Build battery, system clock and missing audio fields first; schedule brightness, workspaces and
power separately rather than deciding compositor architecture incidentally.

2. Push time only when the epoch second changes. Minute-only UI still causes excess work; configurable
cadence remains an upgrade.

3. Align the audio payload with the spec while retaining PID attribution. Master volume/mute are real;
per-app fields are placeholders pending subscriptions. Distinguish mixer Props from unrelated ALSA
Props by channelVolumes.

Later brightness work uses udev plus a 30-second fallback, deterministic firmware/platform/raw
device preference, and unprivileged logind writes. No hardware means no snapshot, not zero. Later
power work makes fields independently optional across UPower and power-profiles-daemon; UPower was
live-verified, profile-daemon support was not. Writable system.state and its producer remained
undecided.

## 0054. The icon theme resolver lives in the renderer, and `image` is the node that draws a file

1. Resolve icon themes in the Renderer with freedesktop-icons; a synchronous Supervisor lookup would
block the dispatch thread and require a missing request/response protocol.

2. Absolute icon names draw files directly; other names use theme resolution.

3. Add image for non-square path-based content; icon adds square sizing and name resolution.

4. Rasterize SVG with resvg and decode raster formats with femtovg's image dependency. Cache by
resolved path and pixel size.

5. Defer app-ID/desktop-entry lookup; no separate Lua find_icon API.

6. Include file mtime and length in cache keys so overwritten tray spools refresh.

7. Queue texture deletion until before the next frame; recorded draws still reference IDs until
flush.

Byte-bounded LRU and off-thread misses were not built; the initial cache was count-bounded FIFO.

## 0055. Wallpaper is an `image` on a Background panel, not a capability

1. Remove the wallpaper capability. Config owns the Background panel, monitor and image source.

2. Change wallpaper through named state, not IPC. Durable runtime selection was not implemented.

3. Image fit modes are cover, contain and stretch. Cover is default; no tile without a caller.

4. Keep ADR-0002's transitions deferred. Immediate texture replacement implements its first-frame/
reload branch, not an animation system.

5. Expose config_dir from the actually loaded shell.lua location for bundled assets.

No picker or folder scan in this pass. A 3840×2160 RGBA texture costs roughly 32 MB; measurement was
left to the memory harness.

## 0056. `workspaces` speaks niri, and its documented contract is wrong in three places

1. Implement niri first without a compositor trait. Missing niri leaves state nil; a second tested
implementation must justify abstraction.

2. Use a separate event socket from keyboard to preserve controller lifetimes. A third consumer could
justify a shared owner; two did not.

3. Publish ordered workspace entries with stable ID, display index and optional name. Focus takes
the stable ID, not the index.

4. Focused-workspace is optional per output because global focus belongs to only one output.

5. Omit unavailable niri fullscreen state rather than fabricate false; class maps to app_id.

No window list beyond the focused client, or special-workspace model. Amendment, ADR-0075: move
compositor probing to a shared top-level module; keep niri types local.

## 0057. `json.decode` is one function on the engine's existing null mapping

1. Reuse the capability payload converter. JSON null becomes Lua nil, not a sentinel; null array
elements leave holes that stop ipairs.

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

2. Exit while locked without unlocking. Flush pending lock requests; avoid normal destructor teardown
that would send an illegal destroy for an acquired lock.

3. A tripped Renderer restart brake exits the Supervisor with code 3; the service unit prevents
restarting that code.

4. Let the service manager restart and collect the pair and its children, scoped to the graphical
session. Compositor startup commands alone do not supervise them.

Rejected: reconnect to fresh Supervisor state or let the Renderer spawn its own authority. A
persistent lock marker was not built yet.

Decision 4 is superseded, and 3 keeps only its exit code. Nothing restarts the pair; the service
unit is deleted.

## 0060. A restarted Supervisor learns the session was locked from a file in the runtime directory

1. Keep the lock fact in $XDG_RUNTIME_DIR/obelisk-session-locked so it survives SIGKILL but not the
login session. Do not serialize transient attempts or acquisition state.

2. Drive it from Renderer outcomes: Locked sets; Unlocked/Finished clear; Refused leaves it. Renderer
loss must not clear it, because the compositor remains locked.

3. Feed a startup marker into the existing reacquisition path, still gated by a valid on-disk
authentication field. Distinguish restart from crash replacement in diagnostics.

Nested niri verified recovery across Supervisor SIGKILL. File errors are logged; absence reads
unlocked. An externally unlocked session can leave a stale marker and cause one extra prompt; the
protocol cannot query lock state. Clean Supervisor shutdown must not clear the marker.

## 0061. Desktop entries are an enumerated capability, not a lookup call

Amends ADR-0054 decision 5.

1. Publish desktop entries as an applications snapshot, not a synchronous lookup.

2. Repeat entries in by_app_id rather than exposing zero-based indices to Lua. Exact matches precede
case-folded and reverse-DNS fallback matches.

3. Keep parsed argv Supervisor-side; launch by entry ID. Managed process.run has the wrong lifetime:
a generation swap would reap the GUI application.

4. Rescan at startup and explicit refresh, pushing only changes. No watcher generalization for
infrequent package-install events.

Deferred: localization, OnlyShowIn/NotShowIn, terminal guessing, embedded field-code stripping and
incremental scans. Terminal entries refuse without $TERMINAL; scans run off-thread.

## 0062. Hover is a signal the engine writes, not a callback it calls

1. Hover is readable state, not only callbacks whose leave edge can be lost during reconciliation.

2. The Renderer owns named, read-only hover and hover_rect signals. Keep the last rect on leave
because a closing popup still needs a valid anchor.

3. The hover property preserves its handle structurally rather than resolving it to a boolean.

4. Write only on boundary changes, not each motion event, to avoid unnecessary scene resolution.

5. Every node on the hit path is hovered, including composite ancestors.

No separate tooltip node, scrolling, animation, cursor changes or keyboard-focus equivalent in this
pass; a non-grabbing popup already supplies tooltip presentation.

## 0063. A display list is what makes a repaint skippable

1. Build and execute one flat display list. Compare it with the last painted list before touching
GL; a parallel hash could drift from actual drawing.

2. Compare plain Rust values, not Lua table identity. Exact float equality is sufficient for repeated
parsed inputs; NaN costs an extra paint rather than a missed one.

3. Store precomputed ancestor-intersection clips per draw. Exclude fully clipped subtrees.

4. Invalidate on new/undefined buffers and remember a list only after a successful swap.

Measured 25-second A/B: Renderer CPU 0.80% to 0.60%, niri 0.52% to 0.36%; wallpaper repaints fell
from 2.23/s to zero. Resolution and cloning still visit every surface on each dirty push. Skipping
invisible resolution remained separate because config maps could have side effects.

## 0064. A masked field draws from a count the tree never holds

1. Pass secure character count beside the scene into painting, never as a retained property. Count
Unicode characters, not bytes.

2. Match the focused capability/action destination so another field cannot display its length.
Unfocused fields show placeholders; focus changes clear the buffer.

3. A keystroke requests paint without scene resolution; display-list equality narrows GPU work.

4. Probe the installed Obelisk PAM service per authentication, falling back to login when absent.
Naming a missing service would hit pam_deny on the observed system.

Leave the machine's PAM failure delay and lockout policy intact. No caret, placeholder styling or
general text editor in this pass.

## 0065. A font file is mapped once and shared, not copied per reader

1. Map font files once and share their Arc-backed bytes across shaping, cosmic-text and femtovg. Avoid
Canvas's copying font API. Measured private dirty fell 49.7 to 26.4 MB, RSS 192.5 to 166.3 MB;
deleting emoji entirely saved less than another 4 MB. Mapping accepts the existing risk of a font file
being modified in place.

2. Keep femtovg/OpenGL ES. The measured Mesa pages were shared clean; removing their mapping did not
justify replacing the renderer. CPU wallpaper buffers would cost 18.4 MB private dirty when
double-buffered at 1920×1200. Judge physical/private cost, not RSS alone.

No effort to shrink reserved virtual address space or compensate for the missing CJK font package.

## 0066. The icon path lookup is the paint loop, not the GPU

Profile before replacing rendering machinery. In 30 seconds, recording draws cost 165.1 ms versus
8.3 ms for flush; icon lookup alone took 1638 µs of each 1645 µs icon call.

Memoize (size, name) to path or absence for the process lifetime, matching the cached theme's
lifetime. Negative results matter because misses search the whole inheritance chain. Parsed
theme-index caching did not cache this lookup.

Measured afterward: icon resolution 79 µs, recording 39.1 ms, bar repaint 3.9 to 0.57 ms, GL phase
0.64% to 0.19% of a core. Keep swap pacing, per-image revision stat and scene cloning; none was the
measured bottleneck.

## 0067. The Wayland client addresses surfaces by position, the retained scene by identity

Keep Wayland protocol instances in a position-addressed vector and retained nodes identity-keyed.
Their callers hold different information; rekeying protocol events would add scans or allocations.
Indices also allow methods to borrow EGL, painting state and surfaces together.

Rejected: named map-state transitions without a real invariant. Candidate null-buffer staging does
not share map-state lifetime. Keep ID-taking entry points where callers actually hold IDs.

## 0068. Paint properties are parsed once at apply time, and a bad one fails the pass

Parse paint properties once during scene apply and reject malformed values through rollback,
including on hidden nodes. Per-frame default substitution disagreed with geometry validation and
could repeatedly log huge values.

Painting keeps only scale/focus-dependent arithmetic over typed data. Configure-time surface updates
retain their separate last-good-value rule. Geometry already parsed at apply time; there was no
second parser migration to build.

## 0069. A scroll offset is engine state the layout pass clamps

1. Store and apply scroll offset in scene geometry so painting, hit-testing and hover agree.
Measured cached applies: 200 rows 2.19 ms, 500 rows 6.14 ms; virtualization stays an upgrade.

2. The engine writes a named read-only scroll signal; Lua lacks the measured extents to clamp it.

3. Add a property to flowing containers, not a duplicate node kind.

4. Layout clamps to content minus viewport and writes back the value actually used.

5. A content-sized viewport has no scroll remainder and no-ops.

6. Prefer compositor pixel deltas; otherwise use value120 steps of three lines. Ignore deprecated
discrete data.

Amendment, ADR-0077: apply the offset in the solved-geometry finish walk. Keep margins and spacing
in scroll bounds; taffy's CSS overflow bounds omit margins. No scrollbar until extent exposure is
needed.

## 0070. A capability starts when the config first reads it

1. First namespace access starts a capability, covering reads and invocations without a second
config-declared roster.

2. Startup is one-way for the Supervisor lifetime. Stopping would require bus-name, in-flight request
and snapshot/revision lifecycle rules.

3. Every generation resends idempotent starts; existing snapshots replay, new state begins nil.

4. Construct inline in the Supervisor loop. Startup enumeration can delay queued frames; asynchronous
construction is an upgrade if measured startup cost warrants it.

5. Secure-submit targets also start their backend; Polkit was then outside the snapshot roster.

6. Polkit registration/session-subject failures log and continue, including an already-owned agent.

7. Permit empty configs and zero surfaces; their presentation handshake completes without work.

Amendment to decision 4: every Supervisor D-Bus connection sets the 25s call timeout Qt, GDBus and
libdbus default to, so an inline start waits at most that per call and then takes its error path.

## 0071. The GL context is built by the first surface that needs it

1. Make EGL optional and initialize on the first surface bind. Hold the Wayland connection so its
lifetime guarantees the raw display pointer.

2. Candidates reach ready without loading GL; initialization moves after ActivateDraw. Measured
ready was 104 ms; first binding grew from about 2 to 30 ms.

3. Accept initialization failure occurring later rather than eagerly allocating a context solely to
prove it works. This creates a later-failure/rollback limitation.

Empty-config RSS fell 151 to 16 MB, PSS 37 to 13 MB. Verify with a real session and driver mappings;
unit tests cannot establish lazy GL loading.

## 0072. A tray item is addressed by the name it registered

1. Keep registered destination separate from resolved owner identity. Chromium tray properties accepted
the well-known destination but rejected reads addressed to its unique owner. The owner keys cleanup
and spool files; owner changes between lookup/read remain a risk.

2. Icon foreground supplies SVG currentColor and participates in the texture cache key. Rewrite
before usvg resolves it; leave files without currentColor unchanged.

No full CSS parser: textual color replacement can also match comments/attributes. Keep theme vectors
preferred over undersized pixmap fallback.

## 0073. The tray host asks the bus what is already there

At startup, enumerate both KDE and freedesktop StatusNotifierItem well-known names and adopt them
through the existing registration path. Keep live registration signals; duplicate adoption
overwrites the same identity.

This recovers apps that do not re-register after shell restart. Object-path-only registrations
remain undiscoverable without probing every bus connection; reject that expensive scan. Existing
liveness checks cover disconnects during adoption.

## 0074. The tray backend exposes what the spec defines

Framework coverage is judged against real application/protocol support, not only dev-config use.

1. Remove spools with items and sweep on startup. Concurrent Supervisors may temporarily erase each
other's icons; that debugging-only risk was accepted.

2. Publish Passive status; hiding is Lua policy, unlike side-effecting activation semantics.

3. Carry base, attention and overlay icon variants separately; Lua chooses presentation.

4. Add SecondaryActivate and Scroll. ItemIsMenu gates only primary activation; scrolling needs its own
parser rather than Activate's numeric coordinates.

5. Resolve item-local IconThemePath before theme names; reject path separators in icon names.

Deferred: unused legacy attention movies, X11 WindowId and category sorting.

## 0075. Compositor detection is session-level, and `workspaces`' seam is a file

1. Move session compositor detection out of keyboard; keep the keyboard-shaped CompositorLink trait
with keyboard.

2. Use an explicit probe-precedence table and name unsupported sessions. XDG_CURRENT_DESKTOP is
diagnostic context, not evidence that a compositor is running.

3. Reduce neutral workspace/window rows, not niri types. Keep protocol mapping local and share
publication/deduplication behavior.

4. The extensibility boundary is a module, not a speculative trait. A second backend adds a sibling
and exhaustive match arms, retaining neutral reducer tests and wire fixtures.

Whether a future abstraction needs one trait or two remains undecided.

## 0076. The capability roster is a type, and the module tree mirrors it

1. Replace the string roster with shared Capability and exhaustive matches. Typed snapshot names replace
runtime membership assertions. Idle/Polkit were then explicit non-roster cases.

2. Capabilities owns controllers; main owns the loop. Race only receives, then process the winning
signal, so cancellation cannot discard an event during an awaited state rebuild. Keep separate typed
channels, not ADR-0037's rejected merged payload channel.

3. Organize by capability rather than D-Bus/hardware transport. Lock remains boot-created for restart
recovery and is passed into dispatch.

Follow-up, 2026-09-01: derive channel senders, receivers, selection and construction from one macro
list, exhaustively checked against the roster. Lock is the stated channel exception. Keep the signal
payload enum hand-written and exhaustively dispatched. Do not reorganize unrelated cohesive Renderer
files.

Amendment, ADR-0207: a Renderer file may split by concern where its tests move with the code;
`layout/scene.rs` stays whole.
## 0077. The layout math is taffy's, not this crate's

Supersedes ADR-0023's hand-written arrangement, one-pass and descendant-positioning choices, but not
its unrelated deferred features.

1. Taffy sizes/positions; scene keeps identity, leases, parsing, scroll bounds and elision. Prepare,
   solve, finish; cache measurement; snap in paint.
2. Fix stretched descendants after content-size resolution; Fill/percent in indefinite flow is zero.
3. Hidden nodes leave layout; getters run once in declaration order. Keep Stretch overriding explicit
   size as existing behavior, not an unrelated config change.
4. Keep depth 64; worst-case stack fell about 1400 to 1040 KiB, including computed signals.

Fresh solver tree per apply. Persistent caching needs another reconciliation lifetime and waits for
measurement. ADR-0143 removes lease ownership in decision 1; transaction rollback stays.

## 0078. `exclusive` is three answers, not a boolean

1. Add Ignore: reserve, respect reservations, or ignore without reserving, so backgrounds do not shrink
   below bars.
2. Use Respect for unresolved signal placeholders; temporary reserve/ignore moves other UI.
3. Reject unknown strings instead of defaulting.

No numeric custom zone or wholesale enum migration without a caller.
Versioning remained pre-release; this entry did not bump the placeholder minor version.

## 0079. A rounded clip is an offscreen pass, not a rounded scissor

1. Render children offscreen, composite through the rounded path, then paint the border; keep the
   recursive display-list group.
2. Reject rounded-scissor intersection: femtovg's single rounded rect cannot represent it; the pill
   test leaked 8% along a straight edge.
3. Radius alone does not enable the pass; Box remains default; childless clips need no group.

Hit-testing remains rectangular. Allocate/free the target per repaint after consuming draws; a
size-keyed pool waits for a high-frequency caller.

## 0080. The battery comes from UPower, not sysfs

Replace the sysfs/udev source and ambiguous charging boolean with UPower.

1. Read DisplayDevice state names, presence and estimates in one snapshot. Presence requires battery
   type and IsPresent; unknown states stay Unknown; zero estimates become nil.
2. No sysfs fallback: it misses observed capacity changes and cannot supply equivalent estimates or
   pending-charge states;
   missing UPower yields no data, not a fabricated answer.

Capacity fell 69 to 65 with zero power-supply uevents while UPower tracked it. No speculative 0% glitch
filter or charge-threshold field. Brightness's verified udev path stays.

## 0081. The stubs are checked against the config, not just parsed

Check real configs against Lua stubs. A probe found 21 missing useful Signal unions; add them while
preserving structural restrictions.

Sample every declared type through a real scene apply for engine-rejected promises; language-server
checks catch omissions exercised by real configs. Check lua-meta separately so library diagnostics
cannot be suppressed; use LuaCATS prose markers on return annotations.

Reject generating types from the current parser layer: it relocates hand-written claims rather than
deriving them. Language-server checking is optional when absent, but the skip is explicit.
Superseded in part by ADR-0210: a missing language server fails the gate.

## 0082. `obelisk.network` is subscribed to the association, not just to the scan

1. Connectivity comes from PrimaryConnection, not AP identity; AP lists cannot describe wired routes,
   radio power or DHCP progress.
2. Subscribe to association/device/manager changes, not only scans; a 90-second association emitted no
   old scan wakeups.
3. Reduce all sources through one Changed event and full state rebuild.
4. SSID names the association; connected names the default-route result; wired default wins.
5. Ethernet enabled reports activation state, so disconnect has read-back, not carrier.
6. Property-stream cache hydration supplies startup state; avoid a duplicate explicit read.
7. Watch associated-AP strength; retarget/abort on association changes. Measured 26 versus 76 events over
   180 seconds did not justify debounce.
8. Retain AP proxies across rebuilds: 11.25 to 0.84 ms at ten APs. Sort connected AP before top-20 so
   a weak active connection survives.

Multi-adapter selection and hotplug discovery remained unbuilt.

## 0083. `network:connect` reuses a saved profile, and the AP order is deterministic

1. Reuse saved SSID profiles with ActivateConnection; create only unknown profiles. Unconditional
   creation produced duplicate UUIDs per rejoin.
2. Update a retyped password without deleting/recreating settings. Skip enterprise updates: GetSettings
   omits secrets; a proper secret agent is required.
3. Break equal-strength AP ties by SSID instead of HashMap order.
4. Reject signal-tier ordering: neighbours held strength between scans; associated AP was already pinned.

Association failure reporting remained missing until ADR-0084.

## 0084. A connect attempt reports its own outcome

1. Carry connecting_ssid and connect_error in snapshots as remembered attempt state across re-derivation.
2. Observe the activation object's StateChanged verdict, then read current state; a failure in that gap
   loses its reason, not completion.
3. Hand-write only the broken Active proxy: its dependency subscribed to lowercase state_changed, which
   NM never emitted. Keep the proxy crate elsewhere.
4. Accept replacements; discard verdicts for a no-longer-current SSID.
5. Saved networks activate without a new secret; unknown-network prompting remains separate policy work.
6. Report the attempt in the example panel header, not every AP row.

The 45-second timeout backstops completion; it does not poll.

## 0085. The Wi-Fi password prompt, and what it cost to give a popup the keyboard

1. Supervisor publishes password_ssid only for new secured connections needing input. Saved/open networks
   proceed; hidden/unknown security prompts.
2. Cancellation is idempotent and centralized on panel close, without clearing unrelated errors.
3. Keyboard focus includes shown child popups, allowing one secure field in the scope.
4. Originally claim focus before mapping the popup; changing focus on the mapped parent
   broke its grab. ADR-0087
   supersedes this bar-wide workaround.
5. Arm a newly visible sole secure field when focus exists, but never steal an explicitly selected field.

ADR-0087 puts this prompt on its own focused surface; popup focus-scope behavior remains useful.
Passwords never reach Lua callbacks.

## 0086. `lua-meta` types nothing unless a signal is `userdata`

1. Lua-language-server 3.19.1 treated class unions as arbitrary tables. Use userdata via Bound for
   signal-valued property unions.
2. Define generic Signal methods as fields so callback payloads bind; chains beyond one hop and computed
   callbacks remain weakly typed.
3. Promote mismatch diagnostics from Hint to checked severity in repo and generated configs.
4. Find the editor-bundled language server when absent from PATH; retain diagnostics instead of requiring
   an unrequested JSON report.

Lua any, permissive classes and list callback inference remain limits; runtime parsers are the gate.

## 0087. The panel host is a layer surface, and a staged layer request needs its own commit

1. Make the example host a layer panel with a full-size outside-click catcher under the card; respect
   the bar's exclusive area so bar clicks remain reachable.
2. Claim keyboard only while the password prompt needs it; restore the bar to None.
3. Replace popup SlideX with config clamping; retain the single-output assumption.
4. Commit changed layer-shell state when mapped and non-candidate. Paint deduplication may skip swaps,
   leaving nonvisual focus/margin/size/exclusive changes pending.
5. Centralize panel toggle and prompt cancellation; popup dismissal is no second writer.

Supersedes ADR-0085's popup-driven bar-wide keyboard claim.

## 0088. Hiding a `panel` destroys it, because the layer-shell re-map is not honoured

Amends ADR-0038: protocol-correct remapping failed on tested niri despite configure, acknowledgment and
fresh buffers. Cache, timing and missing-state-commit explanations were ruled out.

1. Hiding destroys child popups, EGL resources and role objects; showing rebuilds them.
2. A never-mapped hidden startup panel remains created for presentation staging; distinguish it from a
   previously destroyed panel.
3. Revalidate size/anchor constraints on every show because signals can change while hidden.

Accept per-toggle allocation and configure latency; reusing an object that never reappears is worse.

## 0089. A `text` can wrap, and an unwrapped one now measures the line it draws

1. Shaping returns shared visual lines. Slice by min/max glyph cluster bounds; LayoutRun.text repeats
   the original paragraph and visual-order glyphs may be bidi.
2. Wrap is opt-in; None measures without a width so reserved height matches single-line painting.
3. Absent/zero max_lines means unlimited; negatives error.
4. Elide the last retained line using remaining text, not only that line; rejoined dropped lines may
   collapse whitespace inside an already-truncated remainder.
5. Break lines after sizing, then paint one at a time; femtovg does not break lines.

No hyphenation or separate character-wrap mode. The unscaled HiDPI font-size defect remains disclosed,
awaiting hardware verification.

## 0090. A notification's actions are kept, and a config can invoke one

1. Store typed actions; separate default activation and inline reply from visible rows.
2. Action icons are theme names; reject path separators to prevent arbitrary file display.
3. Accept only keys the sender declared.
4. Invocation closes unless resident; emit the corresponding close signal.
5. Cap at eight actions with 64-byte UTF-8-safe labels.

Reply-placeholder metadata waited for the then-unimplemented ordinary input path.

## 0091. The attached picture and the sending application's icon are two fields

1. Split attachment image_path from sender app_icon; remove the competing precedence chain.
2. Carry bare sender theme names to the Renderer; validate paths through trusted roots.
3. Any slash makes a path candidate; is_absolute alone allows relative traversal as a theme name.
4. Rename old icon_path without an alias before release.

Desktop-entry metadata was deferred until a consumer needed it.

## 0092. An ordinary `textfield` reads the keyboard too, because text-input-v3 types nothing

Supersedes ADR-0027 decision 3.

1. Ordinary and secure fields read keyboard/xkb; text-input alone delivered nothing without an IME.
2. Composition, dead keys and compose sequences remain unsupported; later text-input integration may
   augment raw keys when an input method exists.
3. Plain fields initially require a click, without secure-field sole-target auto-arming.
4. Address plain focus by surface/rect; exclude secure nodes from plain paint. Retained NodeId is an
   upgrade; rect matching alone could expose plaintext on a password field.
5. Change/submit callbacks receive full text. Submit empties but keeps focus; Escape originally clears
   without dropping focus.
6. Do not focus a field with no usable destination/callback.
7. A textfield press arms no ancestor button click; reply entry cannot activate its card.

Plain draft state stays outside the retained tree; input repaints without forcing scene resolution.

## 0093. A notification carries when it arrived, because nothing else can work it out

1. Record arrival in epoch seconds, using system.time's helper and units.
2. Use wall time for human-readable age; clock adjustments can change it. No second monotonic field.
3. Replacement content gets a fresh timestamp when its ID is reused.
4. Set it when Notify assembles content, not on snapshot push.

Config maps cannot safely record arrivals, and reload would misdate existing entries. Do not publish a
fixed expiry deadline when holds can move it.

## 0094. Expiry is held off by a deadline, not paused by a flag

1. Hold expiry for a self-releasing duration; zero releases early; activity renews it without matching
   resume from a disappearing config process.
2. Clamp holds to five minutes.
3. Hold globally, not by notification ID.
4. Preserve remaining countdown; do not restart or immediately expire it on release.
5. Keep one task per notification and add a watch-driven hold, not a second shared deadline registry.
6. Do not publish hold state to Lua; log transitions.
7. Use tokio time for deterministic paused-clock tests.

Typing could renew holds immediately; resting-pointer holds still needed a hover callback.

## 0095. `on_hover`, because a config could see a hover but not act on one

1. Fire callbacks only on hover-synchronization boundary changes.
2. Require a named hover slot on the same node. Rect/position identity can transfer state to a
   replacement node; an inert callback is an error, not a fallback.
3. Pass only the boolean; hover_rect supplies geometry.
4. Log callback errors without aborting a working scene.

Live expiry-hold tests closed ADR-0094's resting-pointer case; a callback without a slot fails
reload.



## 0096. A theme name in `image-path` is the application's icon, not a picture

1. A bare theme name in image-path feeds sender app_icon, not the attachment.
2. Keep image_path as an existing absolute picture path; mixing forms restores the ambiguity.
3. Positional app_icon takes precedence.
4. Any slash makes a path candidate; validate it as trusted. Relative traversal is not a theme name.

Separate picture/icon rendering and notify-send's hinted theme icon are the supported forms.

## 0097. The notification card, and the four things the config had to decide itself

Share one notification-card component between popup and history.

1. Initially group by app_name and order by newest content; desktop-entry identity was deferred.
2. One hover region owns the stack's expiry hold; per-card leave/enter ordering could release a
   sibling's renewed hold.
3. Reply requests focus before showing the field; mapping notifications must not steal focus.
4. Card activation invokes the sender's default action, otherwise dismisses.

Suppress popup overlap while history is open. Keep expansion in bounded shared tables, not a named signal
per arriving app. Rich spans and animations were unavailable.

## 0098. A popup is retired, not hidden, and the config is what remembers

1. Config owns a seen-set; retirement is presentation, not notification removal.
2. Key by ID and timestamp so replacement content can pop up again.
3. Mark on history open/close, including arrivals while history is visible.
4. Replace the set with the current feed rather than merge forever.

Keep overlap suppression while history is open. The card's X still dismisses from history; changing that
behavior was left for discussion.

## 0099. `ResolvedNode` carries its `NodeId`, and a plain field's focus is keyed on it

1. Carry retained NodeId into resolved nodes; plain focus survives movement and inserted siblings.
2. It is scene-wide engine identity, not the parent-scoped config ID hint.
3. Require ordinary input; secure_submit can be added without changing identity.
4. Keep rect-based click arming: cancelled click differs from invisibly retained typing.

## 0100. Expiry retires a notification from the popup; it no longer removes it

1. Expiry flags the existing queue entry rather than removing history or creating a second list.
2. Still notify the sender of closure at expiry; later history actions may reach a sender that forgot the
   ID.
3. Transient entries are removed on expiry.
4. Repeated or stale-incarnation expiry is a no-op.
5. Keep the 100-entry queue and 20-entry feed cap.
6. Replacements reset expired and receive a new timer.

Popup config filters expired entries; history appearance and X-button retirement remain config work.

## 0101. `desktop_entry` and `reply_placeholder` are carried from their hints

1. Carry desktop_entry for grouping/application lookup, capped at 128 bytes; reject slashes.
   Unknown IDs miss and use config fallback.
2. Carry the KDE reply placeholder, capped like a button label; empty is absent.

Both fields support existing consumers, not speculative metadata completeness.

## 0102. `textfield` gains `on_cancel`, and Escape gives the field up when it is declared

1. Add ordinary-field on_cancel; it does not make an unreadable field focusable.
2. Escape clears the draft, emits changed-empty when needed, drops focus, then calls cancel; empty still
   cancels.
3. Without the callback, preserve clear-and-stay behavior.
4. Do not introduce a general key event or bare-surface Escape handling.

Secure-field behavior is unchanged. Live reply cancellation removed the row and released keyboard focus.

## 0103. `applications:open_url(url)`, so a link in a notification body can be opened

1. Add open_url to applications via detached xdg-open, not generation-owned process.run.
2. Allow only http, https and mailto. Reject local files and application-specific schemes from untrusted
   notification text unless a concrete future use justifies them.
3. Reject whitespace/control characters and URLs >=2048 bytes.
4. Refuse invalid input; do not sanitize it.

Config decides which affordance calls the action.

## 0104. `text.content` takes styled runs, drawn in the family's own bold and italic faces

1. Accept styled text runs beside strings; reject image spans rather than ignore them.
2. Represent runs as byte ranges over joined text so wrapping/elision can rebase styles. The ellipsis
   inherits the replaced character's style.
3. Send only measurement-affecting bold/italic ranges to shaping; color/underline share cached metrics.
4. Resolve primary-family variants once; fallbacks and missing variants remain regular in measure/paint.
5. Track faces, not just files, so collection face indices and weights agree across renderers.
6. Paint styled segments with accumulated measured advances and explicit underlines.
7. Multiply run color by opacity like the node foreground.

Tests cover parser, wrap/elision ranges and shaping/paint width agreement; hyperlink activation was
separate.

Amended by 0211: paint draws cosmic-text's glyphs, so a styled run's face comes with its glyphs and
no width agreement is left to test.

## 0105. The notification config pass: what the four Rust changes let the cards do

Config-only use of ADR-0100 through ADR-0104.

1. Preserve text styles and inline images; provide URL buttons while glyph hit-testing is absent.
2. Group by desktop ID with app-name fallback and installed metadata; omit transients from history.
3. Order critical, newest, then key for deterministic ties.
4. Derive urgency borders from the group's newest entry.
5. Render offered action icons.
6. Wire DND to backend sound suppression and config popup filtering, with critical bypass.
7. Suppress popups while locked without marking them seen; expired pings remain retired on unlock.
8. Section and date history rather than showing only relative ages.

X still dismisses; launcher overlap and animation remain unchanged.

## 0106. A press on a link's own words opens it: `href` on a run, `on_link` on `text`

1. Carry href on runs and report it through text.on_link; URL-opening policy remains Lua-owned.
2. Share segment splitting with painting and rederive hit geometry through cached shaping. Measure/paint
   divergence tests bound the difference to 2%.
3. A hit link wins over ancestor buttons; ordinary words remain transparent to them.
4. Release must match the armed href and paragraph rect.

Keep URL buttons for links elided from the text.

Amended by 0211: hit-testing reads the glyphs paint draws instead of re-measuring segments.

## 0107. The pointer takes a shape over what it is on: `cursor` on every node, a default in Rust

1. Accept CSS cursor names on every node; reject unknown names.
2. Defaults follow behavior: clickable links/buttons pointer, fields text, otherwise arrow.
3. Walk innermost first; explicit cursor overrides its default, not deeper children.
4. Use SCTK ThemedPointer for cursor-shape protocol with XCursor/shm fallback. Send on changes and
   reset on leave because shapes belong to enter serials.

The extra hit walk and link measurement cost were not measured.

## 0108. A reply's keyboard is on demand, and a plain field keeps its draft while it exists

1. Replies request OnDemand, not Exclusive; network prompts retain Exclusive with outside-click close.
2. Keep a plain draft while its node lives, separate from focus. Losing focus hides the caret but
   preserves text; removal/cancel clears it.
3. Suppress ancestor clicks only for presses landing on a field, not whenever a draft exists.
4. Request keyboard only for a feed reply, not a stale reply ID.
5. Disable card activation while replying; explicit X still works.

Empty popup space still claimed input/focus; narrowing that region remained deferred.

## 0109. The reply field is always there, the keyboard is asked for on hover, and the input region is what is drawn

1. Always draw reply fields; remove Reply-button state. Stamp drafts by card so another card's Send
   cannot submit them.
2. Request OnDemand on hover or with a valid draft; tested niri acquires keyboard on click. Network
   prompts remain Exclusive.
3. Recurse through transparent containers. Claim painted content and intentional invisible click
   handlers, not empty layout boxes.
4. Disable body activation while a draft is pending, not on an obsolete open flag.

A focus property alone would not acquire the compositor keyboard and was rejected.

## 0110. A panel is as tall as its content, up to a cap: `max_width`/`max_height`, and the host card centres under its indicator

1. Add numeric max_width/max_height, 0–8192, for content-sized nodes. Capped content leaves a scroll
   remainder; fixed/Fill sizing stayed separate.
2. Let the example host card size to content and cap each list instead of fixed panel heights.
3. Center beneath the triggering indicator, then clamp inside screen edges in Lua.
4. Recompose network/Bluetooth panels from Lua controls. Empty Bluetooth names fall back to MAC because
   empty strings are truthy.

Hidden-network entry, IP display, sectioning, visibility and codec controls were not all copied;
omissions stayed config scope, not proof of framework gaps.

## 0111. A flex item's cross-axis minimum is `auto`, because taffy 0.14 adds the container's margin to it

1. Use auto for flex-item cross-axis minimum, zero on main and stacking axes. This avoids taffy 0.14
   adding the container's margin to an explicit child minimum. A standalone reproduction and
   anchored
   notification-card test justify the mapping, not a fork.
2. Add opt-in per-instance layout dumps for comparing session geometry with test assumptions.

The card was measured at its 1521 px left margin rather than 378 px content width, underestimating
wrapped height by 13.2 px.

## 0112. A launcher's four missing primitives: `autofocus`, `on_navigate`, `scroll:reveal`, and `obelisk set`

1. Autofocus arms an ordinary field on a live focused surface when nothing owns typing. It opens empty;
   multiple candidates choose document order, unlike secure-target refusal.
2. Navigation callbacks receive up/down/page_up/page_down/tab/backtab without editing the draft; repeats
   work. This is not a general key handler.
3. Scroll reveal makes one-shot minimum movement to show a child, then normal clamping; later wheel input
   can change selection.
4. Carry application Comment for subtitles/search; localization, Keywords and GenericName stay out.
5. External set/toggle forwards named-state writes to the authoritative generation. Parse JSON or a
   string; reject undeclared state and nonboolean toggles. No arbitrary function IPC.
6. The example launcher becomes a keyboard-owning layer panel. Calculator/currency copy waits for
   clipboard support; web opening exists.

Same-day amendment:

7. Hover callbacks fire for Motion/Leave, not Enter or layout movement under a resting pointer.
8. Refresh hover signals silently after layout at the remembered pointer position.
9. Every autofocus arm emits an empty change callback, allowing selection/scroll reset when empty.
10. Two-stage Escape is config policy: clear first, close when empty.

Later amendment, reversing half of decision 4:

11. Carry `GenericName` and `Keywords` as well, unlocalized like `Name`. They are the only place an
    entry says what it is in the user's words: "text editor" is Zed's `GenericName`, "image" one of
    GIMP's `Keywords`, and neither word is in those entries' name or comment. `Keywords` arrives
    split on `;` rather than raw.

`Categories` still stays out. It is a menu taxonomy (`TextEditor;Development;IDE`), so searching it
makes "ide" hit every IDE-adjacent entry. That is noise, not recall. Localization stays out as
decision 4 left it.

## 0113. What a code review is worth: four fixes out of two hundred findings, and the two that were the review's own doc drift

Verify review claims against behavior, not finding counts.

1. Link the pacman local database instead of copying roughly 1500 package directories per check.
   Require a real directory; accept the concurrent-install window of checkupdates.
2. Let the consuming module configure sysinfo; unused temperatures stay dormant. Periodic update checks
   were initially explicit config policy.
3. Widen stubs to accepted scalar edges, border colors/signals, list arrays and optional offsets.
4. Correct integer output-scale documentation and remove config's second geometry division.

Also fix the starter's nil-before-hydration clock access.

Same-day amendment, retaining the original decision numbers:

11. Run update checks when due, including first startup; reload inside the interval must not restart an
    hour-long delay.
12. Remove direct install from the bar badge. Put installation beside a package list and deliberate
    confirmation, not behind a nil-sensitive count guard.
13. Add manual check, live checking state, exit code, last 200 log lines and failure count. Keep the
    privileged install command fixed; interpretation, thresholds and dismissal are Lua policy.
14. Add scalar system-state writes through temp/rename and namespaced keys. Remembered checked_at seeds
   but never overrides fresher checks. Automatic persistence still needed a push hook. Later
   amendment
   (2026-09-06): the package list seeds beside checked_at under the same rule and is ignored without
   it.
   Skipping the first check with an empty list read as "up to date" for the rest of the hour; the
   mirror
    never had the gap because its list lives in the persisted state object.
15. Build the update panel in Lua. No spinner or copy-log without animation/clipboard support; reuse
    the existing action-button pattern.

## 0114. `polkit` joins the roster, and the agent holds its reply until the prompt is answered

1. Put Polkit prompt state and cancel in the roster, retaining secure-target lazy startup.
2. Hold BeginAuthentication's reply until success/cancellation; returning early means failure.
3. Reject concurrent challenges rather than inventing a queue before needed.
4. Wrong passwords keep the prompt open; PAM owns lockout policy.
5. Use Polkit's setuid helper for authentication/response. The unprivileged lock worker cannot call the
   root-only response method; it remains the lock path.
6. A submit button sends the scope's armed native secret on release, without exposing it to Lua.
7. Destroyed surfaces clear focus and cannot auto-arm; the compositor owes no leave for them.
8. Clicking non-fields preserves secure focus; another field, leave or unmap still scrubs it.

Later amendment (2026-09-13), changing how decision 5 reaches the helper:

9. Reach the helper only through `/run/polkit/agent-helper.socket`. Arch's polkit 127 ships the helper
   without setuid, so spawning it fails unless someone runs `chmod 4755`. No spawn fallback.

Masked Escape-to-cancel, multibyte mask support and interactive prompt metadata were not implemented.

## 0115. A capability push can run a handler: `on_change`, and the five things it unblocked

1. Capability on_change runs after each pushed value, before layout, with current/previous. It may act;
   derived maps remain pure and rollbackable.
2. Deliver every push; Lua defines thresholds and edge comparisons.
3. Clear handlers before re-evaluation to prevent accumulation. During topology handoff, old and candidate
   handlers may briefly both fire.
4. Budget each handler at 5 ms; log failure and continue without undoing the value.
5. Config uses pushes for power notifications/actions, persisted check times and package announcements.
6. Share battery thresholds between pills and notifications.
7. Hold a spurious zero battery reading on mains after nonzero; pass genuine draining zero.
8. Drive OSD from state changes, including external commands, not only bar clicks. Lower-priority entries
   drop while a higher one is visible; equal/higher replace it.

Actionable update notifications, timer-based dedupe and OSDs without backend facts remained out of scope.

## 0116. Pointer drags and wheels on a button, and the microphone's volume

1. Buttons receive left-drag start/move/end with local unclamped pointer coordinates. Hold through
   release/leave; field presses do not drag. An inside release may still click after drag end.
2. Wheel callbacks receive vertical fractional notches, positive for increase. The innermost wheel
   handler or scroll container wins, with no chaining.
3. Drag/wheel handlers make invisible button boxes input-active.
4. Add default-source volume/mute and matching actions through the shared device write path.
5. Carry PipeWire device icon hints without resolving them in the Supervisor.
6. Retain volume's 0–1 clamp; no 150% headroom.
7. Implement quantized sliders and device/app controls in Lua, committing held drag values on release.

A brief old-snapshot snap-back remains possible. Microphone OSD and deeper app-icon lookup were not built.

Amendment to decision 2: notches come from `value120` when the compositor sends it, and from
pixels only without it (touchpads). Hyprland sends both, and its ~15px per notch arrived as 0.385.

Amendment to decision 6: output volume reads and writes up to 1.5 with channel balance kept, and the
default sink is written back to 1.5 when another client raises it past.

Amendment to decision 4: `set_balance` and `balance` pan the default output on the same write path;
the louder side keeps its level and never passes the cap.

## 0117. A workspace knows whether it is empty and what runs on it

1. Add populated and one representative app ID per workspace: focused window, else lowest window
   ID; absent IDs remain absent; reduction stays compositor-neutral.
2. No per-workspace window list; a switcher is a separate caller.
3. Example strip collapses on row hover and resolves installed icons, otherwise shows numbers.

A populated window without app ID remains populated. No width animation or opacity fade.

## 0118. `workspaces` speaks Hyprland, as a module behind the same publisher

1. Add a Hyprland module behind the publisher and exhaustive dispatch, no trait; documented IPC and
   synthetic fixtures, not live captures.
2. Re-read workspace/monitor/client/active-window JSON on relevant event-socket lines through direct
   command sockets, not four subprocesses; coalescing waits for measurement.
3. Regular workspace number is both ID and index; nonpositive and special IDs were initially omitted;
   specials are now published separately.
4. Active/focused follow monitor state; activewindow replaces stale focus history; representative app
   uses focus order.
5. Share socket-path resolution with keyboard and fix both callers' missing leading dots.

Deferred: padding, specials, fullscreen and compositor metadata.

## 0119. What one compositor has and the other does not is an absent key

1. Unsupported features are absent keys, not a supports table. Empty means supported without entries.
2. Specials are top-level, name-keyed, with optional shown-on output, not in regular lists.
3. Publish compositor name so Lua can choose policy, such as Hyprland padding.
4. Toggle specials by name on Hyprland; niri logs unsupported calls.
5. The example draws specials separately without a dynamic per-entry tooltip.

No overview or urgency without a consumer. Synthetic slots stay out of backend facts. Hyprland is
documented-IPC-only, not live-tested.

## 0120. A watched folder is a capability, `obelisk.files`

1. Watch requested folders in Supervisor, not blocking Lua reads or parsed ls output.
2. Key by requested path with trailing slashes removed; readiness/errors are per folder.
3. List one level of nonhidden files, filter extensions, sort case-insensitively.
4. Same-filter watches reuse/replay; changed filters replace. Unwatch aborts/removes the key.
5. Debounce settled writes for 200 ms using CLOSE_WRITE, not chunk-level MODIFY. Self deletion/
   movement reports a final result once then stops; defer reappearance tracking.

No user-state overload or unnecessary stat fields; application indexing keeps explicit refresh.

## 0121. A `panel` or `lock` may build its child per output

1. Panel/lock child functions receive the connector at per-instance apply, where output identity is
   known.
2. Invoke every pass and reconcile like list items; named state survives by key.
3. Nil yields an empty instance.
4. Evaluation probes use the fake connector PROBE.

Reject window/popup functions, which lack fixed output, and a global output signal with ambiguous
per-instance meaning. Config implements per-output wallpaper/persistence.

## 0122. Images decode to their box, and off the frame through the thumbnail cache when asked

1. Downscale raster textures to cover the physical box, never upscale storage; include box in every
   image cache key.
2. Opt-in async uses up to four workers and paints empty until completion; upload on the GL thread
   and invalidate lists naming completed files. Inline remains default for complete first frames.
3. Async work uses/writes freedesktop thumbnail cache, validates source mtime/URI, and writes
   private temp files followed by rename.
4. Enable WebP decoding.

No fail-directory cache, shared repository, byte budget or crossfade; no transient inline spools as
user thumbnails or redundant switch.

## 0123. Idle textures have a byte budget, and the allocator's mmap threshold is pinned

1. Evict least-recently-used unshown textures above a 16 MB idle budget. Pin displayed images;
   oversized working sets stay over budget rather than thrash; icons are not pinned.
   Six wallpaper changes used 79 instead of 123 MB GPU memory without continued growth.
2. Pin glibc's mmap threshold at 1 MB rather than raise its adaptive threshold, so freed decode
   buffers return to the kernel instead of stranding future buffers on the heap. Heap stayed near
   23 MB instead of 64 MB.

Do not shrink displayed wallpaper textures, change the Supervisor allocator or hide decode peaks.
Streaming downscale remains an upgrade if peak memory matters.

## 0124. A hidden subtree is frozen, the loop wakes on an fd, and an idle turn does nothing

1. Freeze hidden subtrees without retiring identity/geometry. Skip child resolution, list expansion
   and measurement; preserve hover clearing.
2. Block on Wayland and eventfd, not a 15 ms timer. Frames, decode results, socket termination and
   Supervisor disconnection wake the loop.
3. Run focus housekeeping only on relevant turns.
4. Use two Supervisor async workers, not one per CPU; blocking pool remains separate.

Measured debug idle cost fell 8% to 1.3% of a core and 64 to two wakeups/s; closed surfaces fell
from milliseconds to about 20 µs each. No per-surface dirtiness or streaming image decode without
further evidence.

## 0125. A panel shown in the turn that created it waits for its first configure

A kept but never-shown panel may lack first configure when a startup signal reveals it. Choose
AwaitingConfigure versus Mapped from the acknowledged configured size, specifically whether it is
still `(0, 0)`, not object existence.

The old path attached before acknowledgment and killed the Wayland connection. Fourteen clean boots
followed, versus three crashes in twelve. Leave the downstream EGL panic; root failure was a dead
connection.

## 0126. The release build is the optimisation, and `target-cpu=native` is not

Use the existing release profile. On one 1920×1200 output, debug versus release: first
frame 799 versus 198 ms, boot CPU 0.91 versus 0.12 s, picker CPU 0.73 versus 0.13 s, idle 1.6%
versus 0.4%, Renderer RSS 82.7 versus 69.4 MB, Supervisor RSS 38.2 versus 23.3 MB, binaries
238 versus 7 MB each.

Reject target-cpu=native: no measured gain beyond noise, less portable binaries. malloc_trim returned
none of the picker's retained 3.6 MB. No code change; profile already enabled LTO, one codegen unit,
aborting panics, stripping and overflow checks.

## 0127. The update check hands its pages back, and the rest of the memory is where it should be

Release steady state: 33.7 MiB Renderer plus 13.3 MiB Supervisor PSS on one output, within 50 MiB;
RSS overstated shared Mesa pages.

Trim glibc once after libalpm's blocking check releases roughly 52 MB parse data. Supervisor RSS
fell 84 to 32 MB three seconds after checking; legitimate 84 MB peak stays.

Reject forced Lua GC, which recovered only 93 KiB, and arena limiting, about 380 KiB PSS. Keep the
thumbnail budget. Wallpaper accounted for 27.5 MB of 41 MB GPU memory; profile the rest.

## 0128. The camera scan runs when a camera opens, not when PipeWire renames one

Allocation profiling found 3.2–4.7 MiB live Rust allocations in the Renderer's 16 MB heap; DHAT
required relaxing the CPU cap under emulation and never reached a steady GL frame.

Fix Supervisor camera-scan cadence. An fd scan consumed 19.4 MiB allocations and 37,364 readlinks
at boot; PipeWire name updates reran it. Scan device openers on startup/inotify only, then enrich
names from either source.

Idle never scanned continuously; this reduces redundant startup/event scans, not an idle leak.

## 0129. Measured against the mirror and against Noctalia, and what their renderer has that this one does not

Historical comparison on the same 1920×1200 machine, with both shells running during a 35.5-second
idle window: Quickshell/reference used 169.0 MB PSS plus 9.8 MB helpers, 214.2 MB GPU, 4.65% CPU
and 1.30% cava. Obelisk used 47.5 MB PSS, 51.5 MB GPU and 0.34% CPU. Not feature-identical: the
reference also ran a visualizer and animations.

The native Noctalia renderer supplied comparison ideas, not a rewrite. Obelisk shared one context and
used a byte-budgeted image cache. Dedicated shaders and in-process context recovery did not justify
replacing femtovg/process recovery without evidence.

Correction from ADR-0130: CachedLayer serves blur/backdrop scratch buffers, not general subtree
caching. Blur has no caller. Damage-region submission remains unmeasured.

## 0130. Noctalia read line by line: their animation model, and the two pieces of it this tree already has

Adopt animation's elapsed-time and idle-frame-loop rules, not an implementation here.

1. The animator is a scalar-setter collection, not a binding/property framework.
2. Derive progress from elapsed time since start, not callback deltas; sparse callbacks must not
   slow animation.
3. Arm compositor callbacks only while active; keep the chain alive without drawing when unchanged.
4. The declarative plugin layer cannot request arbitrary animations; native widgets own them, but
   Obelisk's config authors its UI.
5. Retained node identity and leases provide the lifetime basis; interpolation must survive
   reconciliation under that identity.

Reject arena limiting and background allocator machinery for unmeasured gains. Neither renderer
implemented general damage tracking. Correct ADR-0129's CachedLayer claim; binary sizes are
incomparable without shared dependencies.

ADR-0143 supersedes decision 5's lease assumption; retained node identity stays.

## 0131. What Noctalia has that is worth taking for memory, CPU and latency, measured

Measured release resolution: median 1.38 ms, p95 3.38 ms, max 5.48 ms over 62 samples. At 1.74 ms
mean, 60 resolves/s would consume 10.4% of a core before paint. Interpolate retained state and
repaint, not resolve Lua every frame.

Proposed work, not shipped by this entry:

1. Add an opt-in idle profiler with wake/work attribution and spin detection.
2. Use nonblocking EGL swap alongside compositor frame pacing when animation arrives.
3. Virtualize visible list rows plus overscan.
4. Investigate keeping alpha out of text raster keys.
5. Consider bounded shape-memo LRU if the working set outgrows the cap.

Reject unmeasured allocator changes, GL-state tricks absent from the reference, and a whole-run CPU
text cache over the GPU glyph atlas. ADR-0132 verifies/revises these proposals.

## 0132. Checking ADR-0131's five items against the tree, and building the two that survived

Verify ADR-0131; its survey is not implementation authority.

Item 4 is satisfied: glyph and shaping keys exclude color/alpha. Item 5 remains unjustified:
roughly twenty live text nodes do not warrant per-hit LRU bookkeeping for an approximately hourly
wholesale cache clear.

Item 3 needs viewport virtualization, not delegate memoization. Fifty tiles measured 0.916 ms versus
0.783 ms with literal children; delegate work is about 19%, remaining resolution/layout/measurement
about 81%.

Build items 1 and 2: opt-in idle profiler and swap interval zero per bound surface. Log refused swap
hints; retain fallback behavior. The profiler touches no clock when disabled.

Live idle windows showed about 17 resolves per ten seconds from clock/CPU/RAM schedules, not
spinning, at roughly 0.24–0.25% CPU. Nonblocking swap alone had no measured idle speed gain.

## 0133. `obelisk.battery` reads UPower uncached, because its wake-up races zbus's cache

1. Disable caching on DisplayDevice reads while keeping one whole-object subscription. A pre-refresh
   read compared equal, dropped the push and left state one event behind for minutes.
2. Five reads on infrequent changes cost less complexity than five property streams.
3. The power capability's cache-driven property streams remain unchanged (ordered correctly).

Live unplug/replug confirmed the fix; hardware latency was not the cause. A similar tray
custom-signal/cache risk remained unconfirmed and unfixed.

## 0134. `obelisk.updates` is a schedule with a package manager behind a trait, and says which one

1. Put manager-specific name, check, install, progress parsing and reboot detection behind a backend
   trait; the scheduler must not know pacman.
2. Detect executable availability in PATH once at capability start, not distribution branding.
3. Move pacman code and its libalpm allocator cleanup into that backend.
4. Publish optional package_manager so absence is a fact, not a path error.
5. Push initial state with no backend when no later scheduler event will arrive.
6. Refuse check/install without a backend; configure quietly no-ops.
7. Show the example indicator when supported; idle click checks for updates.

One backend exists. Do not invent untestable apt/dnf implementations; gain is ownership and
explicit unsupported-host behavior.

## 0135. An empty `textfield` shows its placeholder even with the keyboard, because `autofocus` made the alternative unreachable

1. An empty ordinary field shows its placeholder while focused, matching masked fields; autofocus
   cannot make the prompt unreachable.
2. Without a placeholder, retain the bare caret fallback. Nonempty draft/caret behavior is unchanged.

Reject per-config overlays and placeholder-plus-caret in identical ink. A distinct placeholder
color is the upgrade path, not hiding search prompts.

## 0136. Persistence is a JSON file the config names, and the framework names no path

1. Let Lua declare each store's absolute directory, filename and defaults; no framework-owned
   settings/state split or default file.
2. Reads are signals; writes update/push immediately; save one second after the last edit.
3. A storage capability owns files by joined path across generation swaps.
4. Defaults fill missing keys without overwriting existing values; removed defaults do not delete data.
5. Support nested JSON values; nil deletes.
6. Accept any user-writable absolute path, not a home-directory or extension sandbox.
7. Remove system.state/write_state and their fixed path; system becomes the clock.

Reject machine-written TOML, which loses comments, and a broad FileView clone without a caller.
Protocol/runtime files and interoperable thumbnail locations remain framework-owned.

## 0137. Privacy reports every capture; telling a video from a song stays in Lua

1. Add microphone and screencast user lists beside cameras; share one user type.
2. Use the existing PipeWire connection's stream classes, not a second connection.
3. Publish only Running nodes; allocated but inactive browser streams are not capture.
4. Exclude sink-monitor capture from microphones by stream.capture.sink, not app names.
5. Carry all lists through one mixer channel to keep ordering consistent.
6. A missing camera watch must not terminate microphone/screencast reporting.
7. Publish MPRIS URL and desktop entry; Lua decides whether media is video.

Reject hardcoded video-app lists and unnecessary PipeWire link tracking. Portal-owned streams may
identify the portal, not the app. Direct compositor screencopy is invisible here; device mute and a
running capture stream are distinct facts.

## 0138. `loginctl lock-session` locks the screen; `loginctl unlock-session` does not unlock it

1. Subscribe to this logind session's Lock signal and route it through the guarded lock path.
2. Log/refuse Unlock; authentication remains required.
3. Serialize SetLockedHint updates from confirmed Renderer outcomes, matching the runtime marker.
4. Subscribe at boot, not after config reads.
5. Missing logind degrades with a diagnostic; native shell locking remains available.

Reject optional Lua lock-session compliance. Lock-before-suspend delay inhibition is separate work
requiring its own fd/window/subscription.

## 0139. A held logind idle inhibitor stops idle events, because Obelisk is the idle daemon

Amends ADR-0032: a session running its own idle daemon honors logind inhibitors.

1. Watch BlockInhibited and match idle as a complete colon-separated token.
2. Suppress threshold forwarding while idle is blocked.
3. Emit sorted Resumed events for previously announced idle thresholds when inhibition arrives.
4. Replay nothing on release; without an idle-state query, wait for the next idle period, not an
   invented restart time.
5. Watch independently of Wayland notify; logind failure leaves the gate open with a diagnostic.
6. Queue registrations before notify becomes live; clear queued/live entries together on reset.

Later amendment (2026-09-13), replacing decision 4 and closing ADR-0159's hole: record events under a
block and replay `Idled` on release for thresholds still idle, so countdowns start at release, as
mutter and PowerDevil restart theirs.

Initially reject roster promotion until a UI wants foreign-holder state; ADR-0141 supplies it.
Keep inhibitor polling and unrequested idle-hint policy out.

## 0140. The config's idle module runs one threshold and a clock, and its settings are a modal

Config-side idle policy over ADR-0139.

1. Register one one-second threshold and use the existing clock for editable delays; no unregister
   API safely replaces separate registrations.
2. Arm each stage after predecessors report done, timing from then. Unlocking unwinds later timers;
   a terminal stage cannot enable a successor. Validate order.
3. Rely on native inhibitor gating/resume, not duplicate Lua guards.
4. Separate settings/facts from clock-driven actions; centralize inhibition writes.
5. Show AC/battery settings together in a modal, not squeezed into a bar panel.
6. Draw equal-width stage chambers from their armed delay, not a cumulative timeline.
7. Default idle actions off so first launch cannot blank the display.
8. Cycle the small timeout list rather than build a combo-box.

Reject ignoring foreign inhibitors and leaking replacement registrations. Fullscreen inhibition on
niri remains unavailable without a backend fullscreen fact.

## 0141. `obelisk.idle` joins the roster, because there is idle state worth reading after all

Amends ADR-0032 and ADR-0139: the bar needs foreign-inhibitor state.

1. Add IdleState with inhibited plus external who/why holders, excluding the shell's hold.
2. Read ListInhibitors on BlockInhibited changes, not a timer; publish holder changes even when
   blocked stays true.
3. Keep bespoke threshold/inhibit methods beside capability read/change methods; each requests lazy
   startup.
4. Remove off-roster dispatch/start exceptions; retain hand-written callback stub signatures.
5. Replay current state on lazy start; a quiet machine must not leave the member nil.

Reject a boolean-only answer, double-reported local reasons, or a Renderer-local signal for
Supervisor-owned state.

## 0142. The icon spool moves out of `/dev/shm`, which is world-writable

Amends ADR-0031/0033: a UID-named directory under world-writable /dev/shm does not establish
ownership; precreated symlinks could redirect sweeping or PNG writes.

1. Move spools to the user's private runtime directory under obelisk/{subdir}.
2. Fall back only to /run/user/$UID, never /dev/shm. Missing storage degrades to no icon.
3. Keep removal's prefix check based on the same directory helper.

Reject extra shared-directory hardening and mtime sweeping when a private directory solves ownership.
This is a cross-user, not same-UID, threat.

## 0143. Removed scene nodes need no lease without a holder

The retirement bag had no production holder; all three successful transaction paths drained it.
Remove the bag, per-node release protocol and child-first destruction requirement. Ownership drops
unmatched nodes; the rollback snapshot preserves the scene through admission and budget checks.
Supersedes lease clauses of ADR-0023, ADR-0045, ADR-0077 and ADR-0130; retain node identity and
deferred retention only for an actual animation or resource owner.

## 0144. A `text` node names its own font family, because per-glyph fallback cannot choose between two families that both have the glyph

`fonts { ... }` is one ordered chain; the codepoint picks the face. It cannot choose between
`CaskaydiaCove Nerd Font Propo` and `JetBrainsMono Nerd Font Mono`, both covering the private-use
icon block. Leading `Propo` wins over `Mono`; at the same `theme.icon.*` size its proportional icons
fill most of the em where `Mono` fits one cell.

Add `font = "<family>"` to `text`. It leads the declared chain, so CJK and emoji still resolve.
`fonts { ... }` remains default and fallback tail.

Use a family name, not fixed roles. An earlier draft added a second chain and `font = "Body" | "Icon"`,
mirroring `Theme.fontFamily` / `Theme.iconFontFamily`; every heading or monospaced face would need
another chain and enum variant. The icon pair remains `theme.icon_font`.

Resolve once lazily on the shaping worker through existing `fc-match` and `fontdb::Database`,
keeping one resolver as ADR-0043 decision 2 requires. Paint consumes exactly the face list it
produces, keeping measurement and painting on the same font set. On loaded-set changes, the worker
bumps a generation and `wayland::surface` re-registers faces with femtovg before drawing: one atomic
load per frame, sync only when a family first appears. Memoization makes an absent family cost one
`fc-match`, not one per measurement.

The family is part of the measurement cache key. A box measured in one family cannot serve another.

Do not validate at parse time. Parsing sees the property, not the loaded font set, which is not fixed
at parse time, and the engine cannot distinguish a typo from an uninstalled font. An unresolvable
family falls back to the declared chain and reports once on stderr, as `fonts { ... }` does for an
unanswered entry. Refuse an empty string, which reads as "no family named" with nothing to point at.
A named family never covers the declared chain, while the declared family covers every named one, so
text does not drift into an unrelated node's display font.

Amended by 0211: paint draws the face cosmic-text chose, and its fallback searches every loaded
family, so declared-chain text can draw a glyph from a named family's face, as measurement already did.

## 0145. `animate` is per-property tweening on the retained node, ticked by compositor frame callbacks, never by Lua

QML's `Behavior on width { NumberAnimation { duration; easing.type } }` is the model: 34
`Behavior on` blocks cover `color`, `opacity`, `width`, `x`, `border.color` and layout sizes;
`InOutQuad`/`OutCubic` run at 100-250 ms. Use retained structure:

1. A node names what eases: `animate = { width = 147, background = { duration = 147, easing =
   "OutCubic" } }`, possibly a nested signal. Allow numbers, `"NN%"` and colours; percent
   only meets percent. `"Fill"`, percent/number and edge-table pairs snap; other properties refuse.
   The first cut snapped every percent, leaving every meter's fill, the mirror's `FillBar`,
   unanimated; the amendment fixed that.
2. The tween lives on retained `ResolvedNode` state (ADR-0099, ADR-0130 decision 5): `properties`
   is displayed value; each `Tween` is its target. Changed targets start from retained screen value,
   resting or mid-flight. Unchanged targets survive ADR-0044 decision 2's re-resolve. First values
   are taken as-is, as QML does.
3. `Scene::tick` advances tweens and lays out retained maps with the same parsers, solver and
   frozen-when-hidden rule, but no signal read, item function or ID allocation. Resolve costs 1.4 ms
   release median before layout; retained relayout uses memoized measurements.
4. The compositor is the clock. `paint_surface` requests `wl_surface.frame` before commit only
   mid-tween; its callback sets a poll flag. Nothing is armed while still, so ADR-0124's
   timeout-free
   poll and ADR-0130 decision 3 hold. Progress is elapsed time since start, never frame deltas
   (ADR-0130 decision 2). Mid-tween surfaces commit unchanged lists because requests answer after
   commit.
5. Clamp overshooting easings such as `OutBack` to the property's legal range, so `width` easing to
   `0` never becomes negative. Default to `InOutQuad`, the reference's common easing, not QML's
   `Linear`.

Reject `animated(signal, spec)`: it resolves the whole scene in Lua per frame, as ADR-0131 measured.
Reject a per-frame Lua callback for the same reason and because config authors this UI, not native
widgets (ADR-0130 decision 4). Reject a generic timer; frame callbacks pace it.

Not built: exit animation (`visible = false` removes the node in the same pass; fade-out needs it to
outlive `visible` or a config timer, the roadmap's "exit-resource lifetime"), looping/indeterminate
motion, edge-table/per-edge tweens, and transforms (no `x`, `y`, `scale`; ADR-0149 added them). One
flag ticks the scene, so outputs at different refresh rates tick at the union rate. Tweens start from
ADR-0045's paired node, so id-less siblings ripple on removal; give them IDs or a `list` key. Hidden
subtree tweens freeze; thaw retargets them.

## 0146. Tweens are typed by value shape, enter from `from`, and exit under `delay(signal, ms)`, because the reference config's remaining motion was blocked by names, tables and the lack of a clock

ADR-0145 shipped a property-name list, snapped edge tables, and took first values as-is. The
remaining 34 `Behavior on` blocks exposed notification `x`, panel `y` as `margin`, OSD/panel fades
from nothing, and exit mapping. Qt Quick's design, with Quickshell adding only `EasingCurve` and a
render-loop hook, settled what to copy.

1. **Type by value, not by name.** Qt registers interpolators per `QVariant` type and `Behavior` can
   sit on any property. `animate` names any property accepted by `lua::nodes::accepts`;
   `Animatable::from_value` chooses number, `"NN%"`, `#` colour or numeric edge table. Different
   shapes snap; name lists are gone.
2. **Edge tables tween per edge.** `Animatable::Edges([f32; 4])` uses absent edges as `0`, as
   `parse_edge_insets` does, and writes a four-key table per frame. This supplies the reference's
   `x`/`y` slide and drop without a transform.
3. **`from` is the entry.** A spec may carry `from = <value>`; an undisplayed property starts there.
   Absent retains ADR-0145's first-value rule and QML's initial-binding behavior. The reference uses
   three `enabled: root.settled` guards in `PanelHost.qml` because `Behavior` fires on construction.
4. **`delay(signal, ms)` is a pull-based clock.** A read records value/due time, returns the held
   value and arms `DelayDeadline`; a later read adopts it, an earlier reversion cancels. The loop is
   timeout-free when idle (ADR-0124). Round the remaining hold up to a millisecond: truncation
   returned hundreds of microseconds early, about 500 times per close on the first live run.
   `visible = linger(open, ms)` keeps a surface mapped through exit; hidden subtrees retain content
   (ADR-0124). `PanelHost.qml` uses a `Timer` and six `retained*` properties.

Keep away from a `Behavior` that overwrites the real property: downstream bindings would re-evaluate
per frame, while the pass's target remains truth and only the retained node holds display value; no
Lua runs per frame (ADR-0131). Qt animates unmapped windows; frozen subtrees do not tick. One table
per node replaces a six-line per-property `Behavior`.

Not built: removed `list` items have no node left to ease, so dismissal snaps; `scale` and `rotate`
need paint-only transforms; sequences and loops, such as the battery plug flash, need a running-state
model.

## 0147. `geometry(name)` publishes a node's laid-out rect to Lua, written quietly by the pass, because a reveal that slides a card by its own height needs the height

`PanelHost.qml` slides from `y = -height`. Porting that under ADR-0146 used a theme constant sized
to the tallest card, which made a 250 px card travel 760 px in 147 ms, off screen for the first 60
ms of open and gone within two close frames. Lua needed the measured height.

1. `geometry(name)` is a name-keyed signal like `hover`/`scroll`; `:set()` is refused. A node
   declaring `geometry = geometry(name)` is measured; pass/tween tick write absolute
   `{ x, y, width, height }` after solve, in the space `on_click` and `hover_rect` report.
2. A tick writes quietly, so moving a card does not run Lua every frame (ADR-0131). A changed pass
   write earns exactly one follow-up (same-day amendment): a growing section's switched panel
   `height` cannot leave the card one pass behind. One, not two, prevents a self-measurement loop.
3. Not a `hover_rect` extension: it is written only while the pointer is on the node and names an
   input region. Measuring must not change hit-testing.

Not built: `width`/`height` bound to an ancestor's geometry signal is a QML-style binding loop; only
the one-pass lag prevents further protection.

## 0148. `obelisk toggle <name> <value>` sets a state or restores its declared initial, so one keybind opens and closes a modal named by a string

The three modals became one `state("modal", "")` holding the name, like `activeModal`, so two cannot
stack. `toggle <name> <value>` stores the value or restores the registry's initial on a match. It is
a `set` with one comparison, the scalar `literal_was_edited` makes, so `obelisk toggle modal launcher`
works for any scalar state. Not built: toggling between two non-initial values; use two bindings or
a boolean.
## 0149. `scale`, `rotate`, `translate` and `origin` are one paint-only affine on every node, because the solver must never see a transform

QML gives every `Item` `scale`, `rotation` and a `transform` list. The reference config uses
`scale` on launcher rows, wallpaper tiles and the modal card. This tree had none, so a hover zoom
had to tween `width`, moving siblings.

1. Every node has four properties shaped like CSS `transform`: `scale` (number or `{ x, y }`),
   `rotate` (degrees), `translate` (`{ x, y }` px) and `origin` (box fractions, centre by
   default). They compose into one matrix about the origin.
2. The transform is paint-only. `LayoutStyle` parses it into `ResolvedNode::transform`; the solver,
   `geometry` and siblings see the untransformed box. `layout::paint` emits the node and subtree
   as one `Draw::Transformed` group and sets the canvas matrix, so text, images and rounded clips
   need no transform knowledge. femtovg's scissor follows the matrix.
3. Hit-testing maps the pointer through each inverse, so a scaled tile is clicked where painted;
   input regions use painted bounds. A zero scale paints nothing and takes nothing.
4. The existing shapes tween them: a number, or `{ x, y }` as a second table-tween key set. An
   absent axis uses the property's default, `1` for `scale`.

Not built: ancestor clips travel with the group, so a scaled child overflowing its parent is cut by
the parent's box scaled with it; nested transforms are not composed into input regions; skew and
3D. Each is a few lines when a consumer appears.

## 0150. A dropped child with an `animate.exit` block stays as a leaving node until its tweens finish, because a node the tree no longer holds has nothing left to ease

`animate` (ADR-0145) eases a property between passes, and `from` (ADR-0146) covers a node's first
pass. A dropped child was destroyed immediately. `util.linger` holds a whole surface whose
`visible` source dropped, not one list row.

1. `animate.exit = { duration, easing, <property> = <target>, ... }` is one spec for every target,
   like QML's `ViewTransition` on `remove`. It runs after the model row is gone and parses on every
   live pass, so a bad block is refused while its node can still identify the error.
2. Reconciliation keeps dropped children after live children as **leaving nodes**, outside the
   solver and holding their laid-out rect. Each pass advances tweens, reparses paint and any pixel
   `width`/`height`, then drops the node when nothing is in flight. `depart` starts each target from
   its displayed value, or its identity if unset (`1` for `opacity` and `scale`, `0` otherwise),
   so `exit = { opacity = 0 }` fades from opaque.
3. They are painted and nothing more. `in_flow()`, used by flow measurement, hit-testing, input
   regions, `geometry` writes and autofocus, is `visible && !leaving`. A leaver cannot swallow a
   click intended for the card that moved into its gap. `contains_node` treats it as a child for
   painting, but not as a live tree node, so a held draft from ADR-0108 cannot keep keyboard focus
   or run `on_submit` for a removed card.
4. A leaving node is never paired again. A re-added `id` is a new node beside the fading one, as
   with a fresh QML delegate. A hidden child or child without an exit block disappears immediately.

Not built: `visible = false` runs no exit (`util.linger` is the surface-level answer, and a hidden
node's subtree is frozen); an exit below a dropped parent does not run, because reconciliation
only asks the dropped child to depart, so declare it on the node the tree actually drops; siblings
snap into the gap rather than easing, which would be a second move-transition mechanism; and a
leaver does not reflow. Only painted properties move
it, so `translate` slides it out while `margin` eases a number nothing draws, `width` resizes the
clipped subtree, and changing `text` keeps the string it was fitted to while colour and other
paint continue moving. An absolute-positioned solver pass is the upgrade for reflow.

## 0151. The easing set is QML's whole `Easing.Type` list plus CSS's cubic Bezier and steps, because eight curves is a menu and the ninth request is always the one missing

`animate` shipped with eight easings (ADR-0145), matching the reference config. A fixed menu leaves
no answer for motion that needs different weight.

1. Provide all thirty-one QML names without the prefix, so `easing.type: Easing.OutBounce` ports by
   dropping four characters: `Quad`, `Cubic`, `Quart`, `Quint`, `Sine`, `Expo`, `Circ`, `Back`,
   `Elastic` and `Bounce`, each with `In`, `Out` and `InOut`, plus `Linear`. `InOutQuad` remains
   default. The full list is the port surface; a partial list turns a moved `NumberAnimation` into
   a runtime error. Fifteen are a name, table row and one-line arm, covered by one reflection test.
2. Write most `In` arms once; reflect them for `Out` and use two halves for `InOut`. `Back` and
   `Elastic` are written out because Penner changes `Back` to `s * 1.525` and `Elastic` to a period
   of `0.3 * 1.5` for `InOut`; reflecting `In` gives a different curve. The test compares all
   thirty-one at one quarter, one half and three quarters with Qt's `QEasingCurve`, not this
   module's other arms, avoiding a reflection tautology that let both exceptions be wrong.
3. A table adds CSS's two unnamed curves: four numbers mean `cubic-bezier(x1, y1, x2, y2)`, solved
   for `y` at the parameter whose `x` is progress. `{ steps = n }` means `steps(n, jump-end)`.
   Both are one `easing` value.
4. Only Bezier control `x` values are bounded to `[0, 1]`, because otherwise the curve doubles
   back and one progress has multiple answers. `y` values are free, allowing `OutBack`-style
   overshoot.

`dev-config` is unchanged: it uses seven of the original eight and `OutBack` once, all already
available. This is framework generality, with tests as its only consumer until a config asks for it.

Not built: parameters for named `Elastic` period or `Back` overshoot, as QML's
`easing.amplitude`/`easing.overshoot` provide. A Bezier covers smooth cases; a spring uses velocity
rather than a progress curve and is a separate mechanism.

## 0152. `keyframes` walks a property through a list of values and `loops` repeats the walk, gated by nothing but whether the entry is there, because a shell has no way to call `restart()`

Five reference animations are a `SequentialAnimation` or looped `NumberAnimation`: two spinners,
the power menu's breathing countdown, the battery's plug flash and the lock screen's shake.
`animate` (ADR-0145) could only ease one property from its current value to one resolved target.

1. `keyframes` is a list of at least two values. The first is the start; each later value is a
   segment eased over the entry's `duration` and `easing`, or its own `{ value, duration, easing }`.
   A zero-duration segment jumps like QML's `PropertyAction`; equal values hold like
   `PauseAnimation`.
2. `loops` is a count or `"Infinite"`; absent means one.
3. A sequence owns its property and reads neither the pass-resolved value nor a target, as
   `SequentialAnimation on <property>` does.
4. There is no `running` flag. `animate` is already bindable, and entry presence gates it, so
   `animate = counting:map(function(on) return on and { ... } or {} end)` starts and stops it.
   Without an entry, the property falls back to the pass value, mirroring the reference's
   `onRunningChanged: opacity = 1.0`.
5. Phase uses whole nanoseconds against the frame list, not elapsed seconds in an `f32`. An
   endless sequence runs for the process lifetime; a 24-bit mantissa loses millisecond resolution
   after two hours and a whole 100 ms cycle after a fortnight, making a spinner freeze and jump.
6. The run is stateless in its value and stateful only in `Tween::resting`. Frame selection derives
   from elapsed time, so reconciliation carries nothing; a completed counted sequence remains at
   its last frame, while `resting` distinguishes it from a never-started entry. Without that bit,
   completion would drop the tween and the next pass would restart it.

Not built: a trigger. Two sequences run forever; the battery plug flash, click flash
(`clickFlash.restart()`) and lock-screen shake (on failed unlock) are imperative `restart()` calls,
but this engine has no imperative node call. A one-shot runs once per entry; refiring removes and
re-adds it across two passes. A pulsing signal belongs with `delay`, not here. Also not built:
easing between whole sequences or a sequence on a property already driven by another sequence.

## 0153. `pulse(signal, ms)` is the trigger and `delay` is a spec's lead-in, because the three animations left in the reference config are fired by a call this engine will never have

ADR-0152 defines a one-shot but no way to fire it twice. The battery plug flash, click flash and
lock-screen shake are `restart()` calls from signal handlers; the other two reference sequences run
forever. A declarative config has no call site, so it needs a signal meaning "a change just happened."

1. **`pulse(signal, ms)` is `delay(signal, ms)` read from the other side.** `delay` returns the old
   value until a change settles for `ms`; `pulse` returns `true` for `ms` after a change. Both are
   pull-based, compare against remembered state and arm one poll-loop timeout, renamed from
   `delay` to `WakeDeadline`, `next_wake_deadline` and `take_due_wake`. A change inside an open
   window restarts it rather than extending it, like `restart()` on a running `SequentialAnimation`.
2. **A pulse gates an entry; it does not start an animation.**
   `animate = pulse(clicks, ms):map(function(on) return on and { opacity = { ... } } or {} end)`
   adds a sequence while open and removes it after. ADR-0152 decision 4 already makes entry
   presence the gate; `pulse` only causes the two passes.
3. **The window belongs to config.** A short pulse cuts its sequence off; the engine does not size
   the window from the animation, since pulses gate non-animations too. The whole window accepts
   `[1, 60000]` ms, the `delay` bound. A value rounding to zero is refused.
4. **A pulse fires on any change, and one edge is a `computed` away.** The reference's
   `onIsPluggedInChanged: if (isPluggedIn)` becomes
   `computed({ pulse(plugged, ms), plugged }, function(fired, on) return fired and on end)`.
   `edge = "rising"` would duplicate that composition.
5. **`delay` on a spec is the lead-in a sequence lacks.** An equal-value segment pauses between
   frames (ADR-0152 decision 1), but a sequence cannot pause before frame one. `delay = ms` holds
   the property, then runs, like CSS `transition-delay` and the wrapper QML's `PauseAnimation`
   needs. It is one saturating subtraction in `Tween::progressed`; `at` and `done` both include it.
   A sequence offsets the whole run once, loops included, from when its first frame is left.
6. **A delayed tween still asks for frames while waiting.** It repaints its unchanged value up to
   sixty times a second. Avoiding that would make the frame loop distinguish waiting tweens; the
   one flag already ticks the whole scene (ADR-0145), so the delay costs no extra mechanism.

`dev-config` gains the plug flash: `pulse` on charging state, gated to the rising edge by
`computed`, driving `loops = 2` of `PropertyAction`/`PauseAnimation` pairs represented by
zero-duration and equal-value segments. The click flash and shake use the same shape over a
handler-written `state` counter and remain for future ports.

`delay` has no consumer in `dev-config` or the reference. This is not ADR-0151 restated: that ADR
completes QML's 31-name list already reached by the reference; this supplies the lead-in ADR-0152
made unwritable before frame one.

Not built: predicate pulses (`computed` composes them), zero-width pulses for a dirty pass, or
per-keyframe `delay`. Frame `duration` already handles the last, and a zero-value segment before
the list provides a hand-written per-sequence lead-in.

## 0154. A `spring` is a third kind of motion, not a thirty-second easing, because the one thing an easing cannot do is keep its speed when the target moves

The reference has no `SpringAnimation` or `SmoothedAnimation`, and the roadmap requires a real
consumer before adding one. The need is behavioural: `animate` retargeting starts a fresh curve
from a standstill at the displayed value (ADR-0145's `retarget`). Hover, drag or measured geometry
therefore visibly stops and restarts. A spring carries velocity instead of position on a curve.

1. **Use displacement units.** `s` is the fraction left to cross, `1` at start and `0` at target;
   the value is `to + s * (from - to)`. One scalar works for numbers, percentages, colours and edge
   tables through existing `lerp(from, to, t)` with `t = 1 - s`. Overshoot is `t > 1` and the
   property's range clamps it as `OutBack` does. The rest threshold is dimensionless and set to
   `1e-3`, a thousandth of the displacement; property units would need an epsilon that
   distinguishes pixels, opacity and 8-bit colour channels.
2. **Solve in closed form.** `Tween::at` remains a pure elapsed-time function, as both passes and
   ticks call it and ADR-0152 forbids running value state across reconciliation. Per-frame velocity
   integration would drift with refresh rate and put outputs at different rates out of step. The
   underdamped, critically damped and overdamped cases solve
   `s'' + damping * s' + stiffness * s = 0`; the sign of `stiffness - (damping/2)^2` is compared
   with a threshold relative to `stiffness`, not a fixed threshold, so soft and stiff springs are
   classified consistently.
3. **Require `stiffness` and `damping`; omit `mass`.** Mass divides out and would only rescale the
   other two. There are no defaults: implicit spring constants cannot be read or tuned. These are
   not QML's `spring`/`damping` scalars because the reference never uses the type.
4. **A spring has no `duration`; only it may omit one.** `duration`, `easing`, `loops` and
   `keyframes` beside a spring are refused, as ADR-0152 refuses `from` beside a sequence. A
   `loops` without `keyframes` is refused too. Two timing descriptions are a config bug, not a
   precedence rule.
5. **Live fields are a type.** `AnimationSpec` becomes a `Motion` enum: `Eased { duration,
   easing }`, `Sequence`, `Spring`. The old doc comments said when `duration`, `easing` and
   `sequence` were dead; this third motion made that a three-way puzzle, resolving ADR-0152's
   deferred review request. `delay` stays on the spec because it applies to all three.
6. **`done` uses an envelope bound.** Each regime bounds its solution by
   `amplitude * exp(-rate * t)` and inverts it, computing settle time at parse time. The bound is
   late, never early: late only leaves the property on its target for an extra frame. It is capped
   at sixty seconds so near-zero damping cannot request frames forever. `at` pins exactly `1` after
   settle so the property reaches the pass-resolved value.
7. **Retarget hand-over is a projection.** Both runs use `value = to + s * (from - to)`. Matching
   value rate projects the old rate onto the new displacement, exact for one number and the closest
   scalar for colours or edge tables whose components differ. A target nearly equal to the current
   value can make the projection enormous, so it is bounded.
8. **An unchanged pass carries only the spring.** `velocity` is the rate handed over at retarget,
   never config data, so re-parsing otherwise yields rest. When the two constants match, the running
   spring survives; `delay` is reread and editing it lands on the moving spring, while editing a
   constant creates a parsed-rest spring. Neither restarts a run whose target did not move.

`dev-config` does not use this; tests are its consumer, as with `delay` under ADR-0153. The roadmap
gate remains unchanged: decide against a real consumer before adding a spring. The trade is roughly
two hundred lines for three second-order regimes, with the hand-over ceiling recorded on
`Spring::handed` and no config to catch a regression; a gate edited by the change it gates is not a
gate.

Not built: `mass`; springs on individual sequence segments; per-channel colour velocity (the ceiling
is `Spring::handed`); or `SmoothedAnimation`, which limits velocity and would be a fourth motion.

Amendment: the spring stays. The owner kept it, so removing it is no longer an intent, only a note
that the code is separable. The roadmap gate still governs the next addition to that row.

## 0155. The engine's test suite guards no `dev-config` component, because nineteen files nothing ships are sample usage and not product

Six tests in `renderer/src/lua/mod.rs` loaded `dev-config/obelisk` to test what
`components/panel_card.lua`, `panel_header.lua`, `toggle.lua` and `panel_toggle_card.lua` build and
how `on_click` filters a button. ADR-0154 moved four engine tests off that load; these remained
because replacing them seemed to require a Lua runner.

1. **`share/starter` ships one file, `shell.lua`.** The nineteen files under
   `dev-config/obelisk/components/` are not installed, referenced by the starter, or delivered to
   engine users. They are one config's implementation, so tests that fail on a sample restyle test
   the sample.
2. **The six engine contracts are already fixtures.** The non-pure-Lua contract is `on_click`'s
   second argument, covered in `wayland/input.rs` by
   `on_click_takes_the_button_name_as_a_second_argument_beside_the_rect`,
   `on_clicks_argument_is_the_buttons_rect_as_four_named_fields`, and the `BTN_RIGHT`/`BTN_MIDDLE`
   mapping. The six tests called handlers directly from Rust and only tested each component's
   `if button == "left"`.
3. **Delete, do not move.** These components need `panel`, `text`, `state` and node builders from
   the engine VM, so a Lua spec runner would require a new engine subcommand beside `obelisk check`
   for a sample-only consumer. `just check` already parses every Lua file and type-checks
   `dev-config` against `lua-meta`; runtime behaviour belongs to running the sample shell.
4. **Keep `require_resolves_the_nested_modules_the_shipped_dev_config_actually_splits_out`.** It
   tests the shipped tree's `?` substitution across dotted `config.theme` directories, which a flat
   fixture cannot test. That is the config-shape case ADR-0154's sweep retained.

This gives up a guard on four components exercised by a live shell. If one moves into `share/starter`,
it becomes product and decision 3's runner is justified. Until then, engine tests test the engine.

Amendment, ADR-0208: decision 4 is withdrawn; no engine test loads `dev-config`.

## 0156. A swap handshake hands back every frame it is not the reader of, because the Candidate asks for its capabilities before it signals ready

On 2026-09-07 a live session locked with no PAM worker behind the lock screen. It rendered and took
keystrokes but could not authenticate under `ext-session-lock-v1`, a lockout rather than a failed
unlock; recovery used ADR-0060's takeover marker.

The Renderer evaluates `shell.lua` before `ReadySignal` (`wayland/mod.rs`: evaluate, bind,
clear, signal). ADR-0070 decision 1 makes reading `obelisk.<capability>` queue a `StartCapability`,
so Candidate starts precede the signal awaited by the swap. `SocketCandidateLink::recv_matching`
logged and threw away everything that was not the frame it wanted, on every swap, not only a rushed
one. Idempotent `Capabilities::start` hid it because the prior generation usually had the same
names; the first read at boot is `lock`.

1. **Defer non-handshake frames.** `recv_matching` keeps them in arrival order for the caller. The
   shared bug affected `StartCapability`, `Command`, `SetState`, `LockReport`, `RequestReload` and
   `ReevaluateReport`; `main.rs`'s stale-`LockReport` comment states the cost.
2. **Still drop `ReadySignal` and `PresentationEvidence`.** This link is their only reader, so an
   out-of-order one is stale or a wire desync, not work owed to another handler.
3. **Replay through the main-loop `match`, from a queue before the socket.** Re-queueing onto the
   inbound channel is unsafe: `MAX_INBOUND_FRAMES` bounds it for flood control, and replay would
   await a path not being drained or `try_send` and drop under load. A `VecDeque` in `next_inbound`
   preserves order, needs no second handler, and its synchronous pop is cancel-safe.
4. **Replay whether the swap succeeded or failed.** The handshake consumes the frames before its
   result is known, and its Candidate may still become authoritative.

Not built: acknowledgement for `StartCapability`. A connection dying mid-write still loses one, and
the Renderer-side `started` set asks once per generation. That narrower hole needs a second occurrence
before it grows a protocol.

Measured live on 2026-09-07 on the same machine and swap, with one surface added to `dev-config`,
by A/B'ing the binaries: **26 frames dropped before, 0 after**. Twenty-one were `StartCapability`
(every config capability, including `lock`); five were `Command` envelopes carrying
`storage.open` for the state file, `files.watch` for the wallpaper directory, `sysinfo.configure`,
`idle.register` and `updates.configure` with the whole package list. Thus every topology reload lost
capabilities, hidden by inherited controllers. The roadmap called the drop silent, but
`recv_matching` logged each frame and both generations; the missing step was reading the log.

## 0157. The layout pass owns the evaluation memo, because a config's shared computed was answering once per property rather than once per pass

ADR-0044 decision 3 only memoized within one `EvaluationMemo`; `node::resolve_properties` called
`Signal::get_value` once per property, so a computed reached by twelve properties across four nodes
ran twelve times per pass. `signal.rs` deferred widening it for a real measurement.

Every live `dev-config` signal getter was timed with `CLOCK_THREAD_CPUTIME_ID` under 40 spinners on
20 cores, the contention a `cargo build` produces:

| | worst getter CPU | getters over 500us in ~20s |
| :--- | ---: | ---: |
| Per-property memo | 1.35 ms | 87 |
| Per-pass memo | 0.57 ms | ~3 |

Against the 5ms cap, headroom changes from 3.7x to 8.8x. `row.padding` in
`components/expanding_pill.lua`, a four-link `linger`/`delay`/`hover` chain, exceeded 5ms once during
boot under a parallel build, so the scene kept its prior frame; the chain did no real work and
measured 1.20ms cold, second worst.

1. **`LayoutPassBudget::enter` opens the memo and `Drop` closes it.** The existing RAII holder's
   `owner` flag makes `EvaluationMemo::enter` inside a pass non-owning, so the table outlives it.
   The pass takes it unconditionally: no `Computed` runs at pass start, and both budget fields
   assume
   one live holder. The change is four lines, two setting and removing the table. Nesting would let
   the inner `Drop` clear the outer deadline; a flag on one field would falsely imply nesting
   safety.
2. **Outside a pass nothing changes.** Startup evaluation and `notify_change` still let the outermost
   `Computed` own the table, preserving a handler that `:set()`s between its own `:get()`s.
3. **Two mid-pass writers now land a pass later for every reader.** `layout::scene` writes a
   `Scroll` cell for the clamp and publishes a `geometry(name)` rect while walking the tree. All
   readers now see the pass-start value, rather than pre-write above and post-write below the
   writer.
   That makes `LiveSignalHandle::set_quiet`'s next-pass contract true; `Scene::settle_geometry`
   already
   schedules the follow-up pass.

Not built: caching across passes. No code here observes a `state` or `Live` cell changing between
passes, so `Watcher` still decides staleness; an invalidation graph is a separate design.

## 0158. The Renderer says when it forgot its idle thresholds, because the Supervisor was clearing them after the replacements arrived

`Loader::evaluate_named` drops local threshold callbacks before evaluation, then the config
re-registers them. The Supervisor separately cleared its fan-out from `answer_unchanged_report`; the
order deleted the new registrations.

1. Supervisor sends `Reevaluate(sequence)`.
2. Renderer forgets callbacks, evaluates, and `register_threshold` sends an `idle`/`register`.
3. Renderer replies `ReevaluateReport::Unchanged`.
4. Supervisor calls `reset_registrations`, deleting that registration.

After an in-place reload no config threshold could fire. `cleanup_generation_thresholds` emptied the
fan-out, so `spawn_idle_event_forwarder` expanded each `idled`/`resumed` to no events. Live config
callbacks confirmed zero `on_idle` and `on_resume` calls across 22 minutes without an inhibitor.
`modules/global/idle.lua` clears `idle.since` and its armed stamp in `on_resume`; a lost resume makes
a five-minute lock count from the first idle despite input. `power-off-monitors` changes `wl_output`,
the Renderer sends `RequestReload`, and the reload breaks the next cycle.

1. **`IdleRegistry::forget_thresholds` sends `idle`/`forget_thresholds`.** Clearing and sending are
   one ordered act on the generation's socket, so new registrations follow it. It is silent with no
   registrations, so a config without thresholds does not start `idle` through reload.

   `replay_pending_registrations` takes queued registrations into a local vector before installing
   them. A forget in that window clears the queue and fan-out, then replay reinstalls what it took;
   this is reachable only before notify goes live.
2. **Both threshold arms of `dispatch` run inline.** Spawning each would let forget and register
   apply in either order. Neither awaits: fan-out uses `std::sync::Mutex` and Wayland calls are
   synchronous. `Inert`/`Live` becomes `std::sync::RwLock` and both methods are `fn`; inhibit stays
   async and spawned because it makes a D-Bus call.
3. **`answer_unchanged_report` resets nothing.** It also zeroed generation inhibit counts and closed
   the logind fd while the live VM retained its hold record. The shell dropped a `microphone`
   inhibitor on the first reload, then believed it still held one and never reacquired it. An
   in-place reload keeps the generation; only thresholds were forgotten.
4. **Move `reset_registrations` to reap.** Once decision 3 landed it had no other caller, and swap
   never called it. A superseded generation kept its fan-out for the Supervisor's lifetime, logging
   pushes to a dead connection after every idle transition. This was the roadmap row *Idle
   registrations outlive their generation*.
5. **Exclude `forget_thresholds` from generated `invoke`.** It is an `IdleAction` on the socket but
   a config call would unregister its own thresholds. `idle` now pairs with no action schema in
   `stubs::capability_schemas`, so `IdleCapability` has no `invoke` at all.

A doc comment on the new variant would cause a third bug: schemars emits a flat `enum` for a plain
unit enum and `oneOf` once any variant is described, while the stub generator then read only the flat
form. One `///` emptied `IdleCapability`'s `invoke` union without failing anything but the golden
test.

Not built: registration acknowledgement. A lost `register` remains lost, the roadmap's *Capability
start acknowledgement* row for a different frame.

## 0159. Re-arming the idle listeners on release was tried and reverted, on evidence that turned out to be something else

`IdleGate::observe` returns before recording, so a seat that idles during a logind block is absent from
the `idled` set. `set_blocked(false)` has nothing to replay and no later event to expect, because a
notification that has sent `idled` never sends it twice. The hole is real; ADR-0139 decision 4 calls
it "still wrong, but safe".

`notify::rearm_listeners` tried to destroy each live `ext_idle_notification_v1` on release and create
a fresh one. It was reverted the same hour.

1. **The triggering symptom was not this hole.** A countdown failing after a dropped logind hold was
   read as the replay gap. ADR-0160 measured a browser's Wayland surface idle inhibitor during the
   same event: the compositor withheld `idled` from every gated listener, with or without a block.
2. **The revert symptom was not rearming.** Idle went silent after running and needed a restart, but
   three Supervisors shared one config directory and truncated the same log; the Wayland inhibitor
   remained up throughout.
3. **The revert stays.** It is not known bad, but nothing measured it. A release one log line before
   the lock stage fired two lines later looked like compositor resolution against last input rather
   than creation time. Log lines have no timestamps, and the stage deadline could have arrived
   anyway. The question is open.
4. **The hole goes to the roadmap** as a code-level gap with no demonstrated live cost; every symptom
   attributed to it has another explanation.

The method lesson is to instrument the layer that distinguishes "not being told" from "told and
dropped": the Wayland event handler, first and last layer reached. Three diagnoses were previously
treated as settled from correlation after changes whose symptoms moved independently.

## 0160. `obelisk.idle` reports that the compositor is withholding idle notifications, because nothing else can see a surface inhibitor

ADR-0141 put foreign logind inhibitors in `IdleState`, but `zwp_idle_inhibitor_v1` is surface-scoped.
A browser can hold it during a video call, and the compositor then withholds `idled` from every
`get_idle_notification` listener. `BlockInhibited` does not report it, so `inhibited` was false and
the countdown sat at zero without an explanation.

During a Google Meet call in Zen, zero `idled` events reached the Supervisor's Wayland handler over
four minutes of an untouched seat, with the logind gate open and threshold registered.

Quickshell exposes both protocol choices through `IdleMonitor::respectInhibitors`;
`idle_notify/proto.cpp` selects `get_idle_notification` or `get_input_idle_notification`. This
derives one answer from both.

1. **Bind `ext_idle_notifier_v1` at `1..=2` and pair every listener.** Version 2 adds
   `get_input_idle_notification`, which the compositor may not withhold. A duration has two
   `ListenerId`-distinguished listeners. Input events never reach config; the twin makes silence on
   the gated listener evidence rather than evidence of a busy seat.
2. **Report "the compositor is withholding notifications", not "an application holds a surface
   inhibitor".** The observation does not prove the cause: niri folds freedesktop screensaver
   inhibition into the same flag, sway adds focus and fullscreen policy, and the protocol lets an
   ordinary notification weigh inputs the twin does not. The holder has empty `who` because no
   protocol names one, and `why` states the observation.
3. **Only the shortest fired threshold votes.** Any-of is wrong: releasing an inhibitor restarts
   gated timers, so 1s and 300s listeners fire a second and five minutes later; any-of would call
   that five-minute gap a held inhibitor. The first implementation shipped with a test asserting
   any-of.
4. **`Resumed` clears both halves of its pair.** Clearing only the reporting half makes the answer
   depend on read order: a gated `Resumed` alone leaves the input half idle, which reads as a
   holder,
   and the input `Resumed` behind it is no evidence and preserves the false positive. Every wake
   could then latch a false holder while the seat stayed busy.
5. **No evidence is not evidence of nothing held.** Divergence exists only while idle, so
   `wayland_inhibited` returns `Option<bool>` and `PublishedIdle` keeps its last value through an
   active seat. The value has no staleness bound and can persist through continuous use.
6. **`PublishedIdle` merges both sources and holds its lock across send.** Logind and compositor
   watchers run in different tasks; releasing between settle and send let the other writer overtake,
   leaving an older payload last with `last_sent` already past it.
7. **A version 1 compositor falls back to logind-only** rather than failing to bind.
8. **A generation appears once per duration in the fan-out.** The list is destinations, and the
   Renderer already runs each callback held at a duration for every event. Two
   `register_threshold(300, ...)` entries therefore ran every callback twice, or four times; each
   half had passed alone, but the pair was wrong.

Draining the raw channel before answering narrows the window where one pair half is read alone, but
does not eliminate ordering artifacts in the published answer.

Not built: naming the holder or checking while the seat is in use, because no protocol provides
either. A dedicated zero-timeout detector could answer live, and `timeout: 0` is valid, but it is
deferred: always-idle behaviour is compositor-specific (Hyprland matches it; Smithay reinserts a
timer) and wakeup cost was not measured.
## 0161. The PAM worker is reached through `/proc/self/exe`, because reading that link strands a locked session

`spawn_worker_and_exchange` re-execs this binary to run PAM off tokio (ADR-0028). `current_exe()`
*reads* the magic link into a pathname. After the binary is replaced, the kernel appends " (deleted)"
and the path no longer exists, so the spawn fails with `ENOENT`. The lock screen reports "could not
start authentication" and cannot unlock the session. This occurred twice on 2026-09-07, including
with the session locked and the user on a TTY:

```
/proc/3342256/exe -> /mnt/Work/0Coding/1Rust/obelisk-shell/target/debug/obelisk (deleted)
```

A `cargo build` triggers it here. `pacman -Syu` does the same to an installed `obelisk` during a
locked session, which is the ordinary upgrade path rather than an exotic deployment.

1. **Execute the link, do not read it.** `SELF_EXE` is the literal `/proc/self/exe`. The kernel
   follows it to the inode already pinned by this process, including after unlinking, so the worker
   starts from the same code as the running Supervisor. One line.
2. **`renderer_binary_path` is not affected and is unchanged.** `with_file_name` replaces the whole " (deleted)"
   filename with `obelisk-renderer` and produces a real sibling path.
3. **Rejected: an `O_PATH` fd pinned at startup, exec'd with `execveat`.** It works and is needed
   only if procfs becomes unreachable; for an ordinary upgrade it adds machinery at the exec
   boundary
   for the same survival.
4. **Rejected: a long-lived worker started at boot.** It adds supervision, restart, and per-request
   state, while a crash reintroduces the exec problem. A transaction per attempt is correct.
5. **Rejected: caching or re-resolving an install path.** It selects new worker code for an old
   Supervisor's protocol, fails during the replacement gap, and creates a TOCTOU: a writable install
   directory could make replacement code receive the password.

Pinning the executable does not pin the PAM stack. A fresh exec loads shared libraries through the
ELF interpreter, so libpam and its modules come from the current installation. The old Supervisor
holds the unlock decision, so replacing its file does not patch it. Security fixes apply at a
controlled restart after unlocking; this only guarantees a way to unlock.

The failure path is fixed too, because a worker can fail to start for other reasons. `apply` already
held the lock and cleared `authenticating`, allowing a retry, but counted every failure as an attempt.
It now counts only `AuthFailed` and `MaxTries`. Otherwise a config limit based on `attempts` could
lock the user out for an unanswerable failure. `StartFailed` names the repair rather than only the
error, and `dev-config`'s lock status now wraps; at 380px it had clipped to
"could not start authentication: pam worker f".

Not fixed: removing the loader or a library required by the old executable still stops the worker;
no exec strategy survives that.

## 0162. The network panel's missing facts are capability gaps, not config workarounds

`NetworkPanel.qml` draws six facts that `obelisk.network` cannot answer. Config fakes two and drops
four. This list lets us remove the fakes when the capability grows instead of hardening them into
config idiom. `NetworkState` carries `available_networks`, `connect_error`, `connected`, `connecting_ssid`,
`ethernet_enabled`, `networking_enabled`, `password_ssid`, `scanning`, `ssid`, `strength` and
`wifi_enabled`; `AccessPointInfo` carries `active`, `band`, `secure`, `ssid` and `strength`; `invoke`
accepts `set_networking_enabled`, `set_wifi_enabled`, `set_ethernet_enabled`, `scan`, `connect`,
`cancel_connect` and `forget`.

1. **`AccessPointInfo.saved`.** The mirror shows forget only for a known network (`network.known`).
   Without it every row offers to forget a network with no NetworkManager profile, a no-op the user
   cannot predict.
2. **A `disconnect` command.** The mirror separates leaving a network from deleting its profile.
   Config offers only `forget`, so dropping a link also destroys credentials.
3. **`NetworkState.link_type`.** The mirror reads `linkType`; config infers wired from the
   `ssid == "Ethernet"` sentinel. That display string is not a type and fails for a real SSID of
   that
   name.
4. **`NetworkState.ip_address`.** The mirror's wi-fi tile shows the address. Config repeats strength,
   which the adjacent glyph already conveys.
5. **Ethernet `speed`.** The mirror shows the negotiated rate; config shows no detail.
6. **`ethernet_interface` and a `ready` flag.** The mirror disables the wired tile without an
   interface and says "Unavailable". Config cannot distinguish absent hardware from a disabled radio
   and says "off" for both.

1 and 2 come first: together they are the difference between a panel that manages saved networks and
one that only joins them. 4, 5 and 6 are readouts; 3 removes a workaround rather than adding a
feature.

Rejected: deriving these fields in config from what already arrives. `saved` is not implied by
`active` or `strength`, and the wired sentinel is the existing attempt at deriving 3. It is the bug,
not a pattern to extend. NetworkManager already holds every fact
(ADR-0037's reasoning for `password_ssid`: what NetworkManager knows does not belong in config).

Rejected: one `network_details` blob. Each field has an independent consumer, and panel drift is
separately visible, so the fields can land one at a time.

## 0163. `PolkitState` cannot describe polkitd's prompt, so the dialog hardcodes it

`PolkitDialog.qml` draws three properties of the authentication request that `obelisk.polkit` cannot
answer. `PolkitState` carries `action_id`, `active`, `authenticating`, `error`, `icon_name` and
`message`; `invoke` accepts `cancel`, and authenticating is a `secure_submit` target.

1. **`input_prompt`.** The mirror draws polkitd's own prompt and hides the line when it is empty
   (`inputPrompt`, `visible: text !== ""`). Config prints fixed "Password:", which is what pam_unix
   asks for but mislabels a fingerprint or one-time-code prompt.
2. **`response_visible`.** polkitd says whether the answer should echo. The mirror switches
   `echoMode`; config always masks it, so a prompt whose answer is not secret is still typed blind.
3. **Whether the field holds text.** The mirror disables Authenticate until the field is non-empty
   (`passwordField.text.length > 0`). `textfield` keeps content in a native buffer no callback can
   read (ADR-0092's reason the mask stays server-side), so the button is always live and an empty
   submit costs a PAM round trip.

1 is first: the string already exists at the agent boundary, and without it the dialog only serves
password authentication. 2 arrives in the same message. 3 is a `textfield` question, not a payload;
if needed, an `empty` boolean signal is the smallest option that does not expose the text.

Rejected: reusing `message` as the prompt. It is the explanatory sentence drawn above the field;
polkitd sends both, and collapsing them loses the field's label.

## 0164. `PlayerState` describes the track but not what the player will accept

`MediaPanel.qml` greys each transport control from a capability flag and offers a stop button.
`obelisk.mpris` answers neither, so `modules/bar/panels/media_panel.lua` draws every control live and
omits stop. Record these capabilities rather than faking them from `play_state`; they are facts only
the player knows.

`PlayerState` carries `album_art_path`, `artist`, `desktop_entry`, `id`, `identity`, `length`,
`play_state`, `position`, `position_updated_at`, `title` and `url`; `invoke` accepts
`control(id, command)` for `play`, `pause`, `play_pause`, `next` and `previous`, plus `seek` and
`seek_relative`.

1. **`can_go_next`, `can_go_previous`, `can_seek`, `can_control`.** MPRIS publishes all four and
   the mirror disables matching controls. Without them a radio stream shows a useless next button;
   `can_seek` is worse because dragging the seek bar is silently discarded.
2. **`stop`.** `controller.rs`'s `VALID_COMMANDS` has five entries and excludes stop, although its
   test uses `"stop"` as the invalid case. MPRIS `Stop` releases the track instead of holding its
   position as `Pause` does.
3. **`album`.** The mirror's second line falls back title -> artist -> album -> identity. Without
   album, a classical track whose artist tag is empty falls straight to the player's name.
4. **A monotonic clock, or a pushed position while playing.** `position` is valid only at
   `position_updated_at`, which is `CLOCK_MONOTONIC`, but no Lua global reads that clock. The panel
   anchors each push with `os.time()` in an `on_change` handler and adds elapsed seconds; a clock
   adjustment skews the bar until the next push. A monotonic reading beside `obelisk.system.time`, or
   a cadence while `Playing`, removes the workaround. The clock is smaller and can time other
   durations.

1 comes first: four booleans already exist on the bus, and without them three of six controls are
decorative on some players. 4 follows because it is a general capability, not an mpris one.

Rejected: inferring 1 from `play_state`. Seekability is independent of playback; a paused local file
seeks while a playing stream does not.

Rejected: computing elapsed time from `os.clock()`. It returns CPU seconds for this process and stops
advancing whenever the shell is idle, precisely when a track is playing and nothing is being drawn.

## 0165. `Position` is read twice around a state change, and never fabricated

Two changes to `capabilities/mpris/player.rs` follow watching a browser drive the media panel.

**A failed read keeps the last reading, with its timestamp.** `resync` used
`player.position().await.unwrap_or(0)`, turning "the player did not answer" into "the track is at the
start", contrary to ADR-0036 and unlike the adjacent `PlaybackStatus` and `Metadata` arms, which retain their values. The
timestamp now travels with the value; a fresh timestamp on stale data makes extrapolating clients
draw a backwards jump. `-1` means nothing has ever been read, matching `length`.

**A `PlaybackStatus` change schedules a second read 100ms later.** Some players publish `Position`
after the new state, so the signal-time read catches a transition value, sometimes zero for Firefox.
Quickshell uses the same remedy in `MprisPlayer::onPlaybackStatusUpdated`: request the property, then
request it again on a 100ms `singleShot` commented for YouTube. One delayed re-read in the forwarder's
`select!` avoids polling.

Rejected: polling `Position` while playing. It adds a D-Bus round trip per tick for values clients
can extrapolate; ADR-0036 says the value and timestamp pair is enough. Recheck on a transition, not a
cadence.

Rejected: filtering zero in config. `dev-config` did carry that workaround while this was being
diagnosed, and it needed a track identity to distinguish a bogus zero from a legitimate start,
reconstructing in Lua what the Supervisor knows. Handle the player response where it is read.

## 0166. What the player says about its own position is not evidence

Extends ADR-0165, which was written before the player was measured. Driving Firefox over D-Bus:
seek to 340s, then read `Position` back as 340s, `0`, `0`, and finally the true 363s, with
`PlaybackStatus` `Playing` throughout. `Rate` reads `0`; some tracks publish no
`mpris:length`. The zero is transient and can recover after seconds, beyond ADR-0165's 100ms wait.

**Discard a zero `Position` on a track we were already inside** (`resolve_position`). It is a stale
player value that restarts the progress bar. Publish zero for a new track, where it is valid.

**A failed read retains the previous reading only for the same track.** ADR-0165 retained it
unconditionally, so a 30-second track could inherit the previous track's 5:40 and draw past its end.

**`seek_relative` sends the offset to the player** (MPRIS `Seek`) instead of reading a position and
converting to `SetPosition`. That read is the one path `resolve_position` does not cover, so "forward
five seconds" from 5:40 became
`SetPosition(5s)`. When an absolute target still needs conversion and no usable trackid exists, an
unavailable position refuses the command instead of becoming zero, and subtraction is checked.

**The 100ms recheck is disarmed only by its own timer.** Clearing it on any event let a `Metadata`
change 20ms later cancel the correction it was meant to provide.

Rejected: filtering zero in config. It needs track identity to distinguish a bogus zero from a valid
start, duplicating Supervisor knowledge in Lua. Handle the player response where it is read.

Rejected: polling `Position`. It adds a round trip per tick for values clients can extrapolate. The
recheck fires on a transition, not a cadence.

## 0167. A signal read inside a `:map` is not a dependency

`modules/bar/panels/media_panel.lua` called `chosen:get()` inside every map. The switch-player button
therefore changed the value maps would return without changing a declared dependency, so the panel
kept drawing the previous player until unrelated data moved.

One `computed({ obelisk.mpris, chosen }, ...)` now resolves the player, and every reader uses that
signal. `false` is the no-player value because a `computed` yielding `nil` has no value to hold.

The rule generalises: `:get()` inside a `:map` or `computed` callback reads an undeclared graph input
and is correct only for a value that cannot change while the map lives. `on_click`, `on_commit` and
`on_change` callbacks may read freely because they are not re-evaluated.

`just types`, `luac -p` and `obelisk check` cannot detect this. The code is valid and the scene
resolves; only a control that does nothing exposes it.

## 0168. Chromium's tray object lives on one of its several connections

ADR-0072 decision 1 keeps a tray item's registered name as the message destination, but wrongly says
Chromium dispatches property reads on the destination field rather than the owner. The decision stays.

Measured against Slack 699047 with destination and object path varied independently:

| destination | object path | `Get Id` |
| --- | --- | --- |
| `org.freedesktop.StatusNotifierItem-699047-1` | `/StatusNotifierItem` | fails |
| `org.freedesktop.StatusNotifierItem-699047-1` | `/StatusNotifierItem/1` | answers |
| `:1.2656` (its owner) | `/StatusNotifierItem` | fails |
| `:1.2656` (its owner) | `/StatusNotifierItem/1` | answers |

The destination does not matter; the path does. Slack holds two connections, `:1.2655` and `:1.2656`;
only the one owning the well-known name exports the object. The other returns "Object does not exist"
at every path.

Address the registered name because the bus routes it to whichever connection owns it. This avoids
guessing which connection a process uses. ADR-0072's accepted risk, a well-known name moving owners
between lookup and read, is smaller than that avoided risk.

This also explains the September observation behind ADR-0072: reads addressed to owner `:1.659` failed
because it was Chromium's other connection, not because destination fields matter.

Rejected: collapsing `ResolvedRegistration` to one name (~35 lines). The split is load-bearing.

Rejected: keeping `DEFAULT_ITEM_OBJECT_PATH` as the well-known branch's path. Slack's object is at
`/StatusNotifierItem/1`, so that default cannot answer; the registration-string split supplies the
real path, and the default now applies only to a `service` that names none.
## 0169. A lockout leaves evidence, or it is not a diagnosis

On 2026-09-08 the lock screen refused a correct password with `pam worker failed: i/o error:
early eof`. The worker exited without its outcome frame and left no inherited stderr, coredump or
journal entry. `faillock` was empty; the previous acquisition was `Success` minutes earlier; a later
attempt restored the session. The cause remains unknown.

1. A failed exchange reports how the worker died. `reap_process_group` already collects the exit
   status; `exchange_over` now includes its signal number or exit code. It cannot say why, but it
   separates "killed" from "returned non-zero", which is the fork the next occurrence turns on.

2. Layout errors carry the walk to the node, such as `column[0] > row[1] > text[1] > ...`, rather
   than only the surface name (ADR-0024).

Correction: a `:map` returning `nil` leaves the property absent, not `Integer(0)`.
`resolve_properties` skips nil-resolving signals, as ADR-0044 decision 1 specifies; a probe config
renders empty and raises nothing. `Integer(0)` is a real zero: a `delay`'s pre-change identity is
`0`, the `util.linger` bug. Suspect a signal without a value, not a nil return.

Not done: one bad property still discards the whole re-resolve, freezing the lock screen during
authentication. Partial application is a larger decision than this incident settles.

## 0170. A memo key is an identity, not an address

On 2026-09-08 the lock screen's keyboard label and wallpaper path both resolved to `Integer(0)`,
although their Lua functions return a string, `"--"` or `"!"`, and a path or default. ADR-0169
located the nodes but blamed an unvalued signal, which was wrong.

`EvaluationMemo` (ADR-0157) keyed computations by `Rc::as_ptr(deps)`. `computed()` and
`Signal::mapped` each allocate a fresh `Rc<Vec<Signal>>`, but the raw pointer keeps nothing alive.
After a computed is dropped, the allocator can reuse its same-sized address, so the memo serves its
dead value to the next computed. A layout pass continuously creates and drops computeds in every
`:map` inside a `list`'s `itemfn` and in surfaces rebuilt on each resolve. An eight-line Lua
reproduction reads a computed returning `0`, drops and collects it, builds one returning a string,
and gets `Integer(0)`.

The key is now a construction counter copied by `Signal::clone`: a clone is the same computed and
must share its entry. The counter is never recycled.

Any pass that freed a computed could serve a later computed a stale value of any type, silently and
intermittently. The lock screen exposed it because typed properties rejected an integer and
ADR-0169 named the nodes.

## 0171. Adoption tries the paths items actually use

ADR-0073 adopts bus items by walking well-known
`org.{kde,freedesktop}.StatusNotifierItem-PID-N` names. Without a
`RegisterStatusNotifierItem` argument, it guessed ADR-0031's default object path. That is the wrong
path for every Chromium application: Chromium uses `/StatusNotifierItem/1` for Slack. Until
ADR-0168's liveness probe, the guess produced a blank item rather than nothing, so the gap surfaced
as a duplicate-key freeze in the bar instead of as the missing icon it always was. After the probe
began rejecting non-responsive objects, Slack and vesktop vanished on every shell restart.

Adoption now tries `/StatusNotifierItem`, `/StatusNotifierItem/1`, and
`/org/chromium/StatusNotifierItem/1` in order, keeping the first that answers `Status` and reporting
all refusals if none does. An item at the last path costs three startup round trips.

The ayatana form, `/org/ayatana/NotificationItem/<id>`, is excluded because its final segment is an
application-chosen id. Those clients re-register on `StatusNotifierHostRegistered`; yerd returned
within the second after a restart on 2026-09-08. Other paths still need the connection introspection
ADR-0073 declined.

## 0172. An item is a connection and a path, the way KDE says it

Reading how the established hosts do this settled a question ADR-0031 left open. Qt's client, KDE's
watcher, Plasma's system tray, Quickshell and Noctalia v5 parse `service` the same way: a leading `/`
means the sender plus that path; otherwise it is a bus name with
`/StatusNotifierItem` as default.

KDE's watcher composes `QString notifierItemId = service + path;` and publishes it. Plasma rejects an
id without `/`. Registration resolves the pair once while the sender is known; downstream hosts do
not guess.

Ours resolved the pair and keyed the registry on it, then passed config only the unique name.

1. `RegisteredStatusNotifierItems` returned `":1.42"`, invalid to Plasma. It now returns
   `":1.42/StatusNotifierItem"`, as does `StatusNotifierItemRegistered`.

2. `TrayItem.id` was only the connection. Two items from one connection therefore shared an id, and
   a `list` with two rows and one key rejects the whole tray, causing the 2026-09-08 freeze.
   Chromium
   uses `/StatusNotifierItem/1` and `/StatusNotifierItem/2` because one process can export several.
   The id is now the sanitized name plus path, `"1.42/StatusNotifierItem"`. Icon spooling folds
   separators into a flat filename stem and doubles `_` first, preventing collisions.

Not taken from KDE: its watcher accepts every registration and lets `NameOwnerChanged` clean up.
ADR-0168 refuses objects that answer nothing, avoiding blank icons for items at another path. The
probe is optional because duplicate ids cannot reach config.

Amendment: `StatusNotifierItemUnregistered` is emitted now, from
`tray::registry::spawn_name_owner_changed_forwarder`, under the same `service + path` id
`RegisterStatusNotifierItem` announces. It is sent after the registry entry is dropped and its
spool files are reaped, so a stalled D-Bus write delays only the signal. Nothing of ours consumes
it -- our own tray reads the registry -- which is why announcing arrivals and never departures went
unnoticed; a second host on the bus kept every item that ever left.

## 0173. The notifications panel is the shell's status sheet, not a feed

`NotificationHistoryPanel.qml` opens with `WeatherWidget`, then `SystemInfoWidget`, and only then its
masthead with the bell, summary and list. Two thirds of the panel were missing: ours had only the
masthead and list, with the readout as a two-glyph pill in a settings toplevel the mirror lacks.

Restored:

1. `SystemInfoWidget` is the factory from `modules/bar/indicators/system_info.lua`, instantiated by
   `modules/bar/panels/notification_history.lua` as in the mirror. Each instance owns `expanded`.
   The settings toplevel keeps no second instance. Its `window {}` is the config's only one and its
   only exercise of a toplevel.
2. `Components/InfoBadge.qml` became `components/info_badge.lua` and carries the header's urgent
   count. `bluetooth_panel.lua` already had the same capsule as local `battery_badge`; it is now
   shared for six call sites, as in the mirror.

### What is absent, and why absent beats faked

The mirror's `SystemInfoService` shells out for GPU load, per-disk usage, uptime and boot time.
`obelisk.sysinfo` exposes only `cpu_percent`, `ram_percent`, `swap_percent`, `temp_cores` and
`temp_gpu`: "CPU, memory and temperatures", exactly as the capability table names it, and nothing
here proposes to grow it. So those tiles are absent. The panel uses swap as the memory tile's
second line, where the mirror prints `used / total`, and GPU temperature where the GPU tile stood.
That tile is conditional on `gpuTemp > 0`; with no sensor `temp_gpu` is `-1`. The collapsed summary
is `CPU`/`RAM`/`SWAP`, not the mirror's `CPU`/`RAM`/`GPU`/`DISK`.

`temp_cores` has one value per hwmon sensor, while `cpuTemp` is a package figure. The tile shows the
hottest sensor; averaging the list could include a cooler chipset probe.

### Polling has no off switch here

The mirror ref-counts `SystemInfoService.refCount` from `active`, so pollers run only while the panel
is open. The capability's `configure` sets an interval, and zero stops every reader rather than one
widget. Polling always is the choice: against never polling and leaving the status stale, two
`/proc` reads every couple of seconds is the cheaper mistake. `temp_interval` follows RAM at 5s
because the tile's second line is not watched.

### An unrelated swap this uncovered

`config/icons.lua` reversed `cpu` and `ram`: F035B is the pinned processor and F061A the DIMM stick,
as `SystemInfoWidget.qml` uses them. The old readout labelled memory "CPU"; nothing else referenced
either name.

### Still a deviation

`DateTimeDisplay.qml` opens the panel from the whole clock and puts `MinimalCalendar` in its hover
tooltip. Here the bell opens the panel and the clock opens the calendar as its own panel; a hover
calendar cannot be exercised in this session. Contents match; the opener does not.

## 0174. The clock is one control, and the calendar is a tooltip

`DateTimeDisplay.qml` is a `Rectangle` with a `Row` holding bell and clock and one `MouseArea`
covering both. Its click opens the notifications panel; `MinimalCalendar` is a hover tooltip under
the weather description, not a panel.

Ours split the control: the bell opened history and the date opened a calendar panel. The bar's
always-visible readout opened a month grid half the time; the system readout, greeting and feed sat
behind a two-character glyph. It now has one `button` over the pill opening
`notification_history`; `MinimalCalendar` is in `date_time.lua`'s tooltip and removed from
`panel_host.lua`'s list. The pill uses the mirror's third state, `border.color: panelOpen ?
activeColor : ...`, and rings while its panel is open.

### Tooltips stand down while a panel is open

The mirror gates this tooltip on `mouseArea.containsMouse && !panelOpen`. That gate is in
`components/tooltip.lua`, covering all seven slots and any panel; a tooltip in the panel card's space
would cover the requested sheet.

`MinimalCalendar` sizes to its month with `rowCount:
Math.ceil((firstDayOffset + daysInMonth) / 7)`, giving four to six rows. Fixed six-week sizing left
seven blank cells under September 2026. A `popup` surface has explicit size under `lua-api.md` § 6,
so height is a `Bound`, which is why that section accepts `integer|Bound`; the tooltip follows it.
Its vertical padding is `spacing.md`, not shared `xs`, because one- and two-line tips already count
`xs` in fixed height.

`NotificationHistoryPanel.qml` uses `readonly property int padding: Theme.spacingMd` and
`anchors.margins: root.padding`, sizing as `contentColumn.implicitHeight + padding * 2`.
`panel_card`'s `sm` top and bottom default versus `md` left and right put the first line on the top
edge. `panel_host.lua` now names padding once and derives `CARD_CHROME` for the animated height.

### Twelve-hour, decided rather than derived

`TimeService.qml` asks `Qt.locale().timeFormat(Locale.ShortFormat)` for an `AP` marker and chooses
`HH:mm` or `hh:mm AP`. Config has `os.date` but no locale, so the bar and tooltip seconds line use
`%I:%M %p`.

### A greeting the mirror does not have

The panel opens with the account's full name and `Tuesday 08th of September 2026 03:11 PM` above
the system readout. `NotificationHistoryPanel.qml` lacked both, showed a 420px sidebar, and used
`Tue 08 Sep` on the narrow bar; the panel spells it out.

Identity moved from `modules/global/lock.lua`'s block to `lib/identity.lua`, using `getent passwd` for
GECOS and `uname -n` for the host. Two readers require extraction; its `state` guard keeps reloads
from spawning the two subprocesses more than once per session.

## 0175. A process the user would notice stopping belongs to the session, not the generation

`process.run` gives a child the generation's lifetime, and `reap_generations_processes` kills its
group on every swap. That suits a helper that exits, not a screen recorder whose stop the user would
notice.

`Services/SystemInfo/ScreenRecordingService.qml` is 223 lines, about 180 of them a workaround for
Quickshell replacing singletons on reload. It orphans `gpu-screen-recorder` with a 900-character
`sh` script, checks `/proc/$pid/exe`, reads field 22 of `/proc/$pid/stat` for kernel start time,
and stores pid, start time, path and launch epoch in a lock file. Each signal repeats the probe;
pid alone can be recycled. A two-second `Timer` catches crashes. `PersistentProperties` and the
lock-file launch epoch reconstruct elapsed time and can disagree about paused seconds.

Copying that design was rejected despite it working on this machine and matching the mirror. `setsid`
is enough to escape our `killpg`, where Quickshell needed nothing, but the Supervisor does not restart
on config edits. It
already holds a `Child` for every `process.run`; no orphan liveness probe is needed.

1. **`obelisk.processes` is a roster capability, declared as `session_process { name, stop_signal }`.**
   It matches the `obelisk.storage`/`persistent_table` shape: config names the thing, the Supervisor
   owns it, and state returns by name. Unlike `storage`, which keeps a file config could read, this
   keeps a handle config cannot hold.

   It is not an option on `process.run`. `detached = true` would report exit to callbacks in a dead
   VM and return a handle nothing can re-find. Detachment without an owner is the same workaround.

2. **One task per running program owns its `Child` and is the only place that signals its pid.**
   `supervise` selects between cancel-safe `child.wait()`, re-armed after each request, and a
   request
   channel. The task signals before reaping, so the kernel reserves the pid. Signalling from the
   controller's map would reopen the pid-recycling window that start-time checks cover.

3. **The stop signal is declared, and shutdown uses it.** `SIGTERM` is the default but wrong for
   `gpu-screen-recorder`, which finalises its container on `SIGINT`; skipping that leaves an
   unplayable file. The grace is five seconds, not the usual 100 ms, because this long-running
   program needs time to close its output.

4. **stdio is inherited, not piped.** A session process outlives its generation, so no callback can
   receive its output. Piping would drop lines or require an owner across generations. The shell's
   own
   log is the honest destination, and a config that wants a program's output wants `process.run`.

5. **`start_error` is state, not just a log line.** Config waits on `running`. A command absent from
   `PATH` never sets it; without a readable reason, that looks like a slow start while the reason is
   only in Supervisor stderr.

6. **`start` on an undeclared name is refused rather than creating one.** Declaration makes a name
   exist, so a typo reads `nil` instead of looking like a program that never starts.

The wire name is `processes`, distinct from the off-roster `process`; they use different arms of
`main.rs`. A test pins both: `from_name("process")` is `None`, and `from_name("processes")` resolves.

The config will no longer need the launch script, lock file, `/proc` probe, liveness poll,
restore-on-restart path or split elapsed-time accounting. It will use `rec.running` and
`rec.started_at`.
## 0176. The recorder is argv, a file name and pause arithmetic; everything else was the lock file

`ScreenRecordingService.qml` is 223 lines. `lib/screen_recording.lua` mirrors it in about 250, but
ADR-0175 deleted the mirror's subject. The 900-character launch script, `/proc/$pid/exe` check,
kernel start time, lock file, re-probe before every signal, two-second liveness poll, and
restore-on-restart path are gone. The remaining configuration is argv, the file name, and pause
arithmetic.

The panel and indicator are `ScreenRecorderPanel.qml` and `ScreenRecorder.qml`.

### Where this deliberately leaves the mirror

1. **`-w <WxH+X+Y>`, not `-w region -region <WxH+X+Y>`.** The installed gpu-screen-recorder
   deprecates `-region` and fails with `gsr_encoder_receive_packets: failed to write frame index 1
   to muxer, Invalid argument (-22)`, writing nothing. The same geometry through `-w` records
   cleanly.

2. **The exit status picks the notification.** `_clearRecording(true)` announces "Recording saved"
   however the recorder ended. Observed live: a bad `-a` argument killed it at once and the popup
   offered to play a file that did not exist. `gpu-screen-recorder` handles `SIGINT` by writing the
   container index and exiting 0, so zero offers the file and any other status reports failure with
   its code.

3. **Three mouse buttons on the indicator.** `components/icon_button.lua` guards left-click only,
   matching the mirror. The two captures differ only in extent, so one click each avoids a panel
   round trip; the panel still labels them. The escape hatch is `on_button`, not `on_activate`.

4. **Four buttons in two slots.** `OButton` binds `bgColor` and `variant` live; `action_button`
   chooses its grounds from a static `tone`. Making `tone` a signal means mapping rest, hover,
   border and ink through it, for one caller. An invisible node takes no size or spacing, so each
   state keeps the same row, one label, one tone, and one job.

5. **The settings section stays open across a close.** `onIsOpenChanged` collapses it, matching
   `modules/bar/indicators/system_info.lua`, the config's other expandable section. There is no
   close edge for reset without making `lib/ui_state.lua` require a panel back.

### Components that grew, and why each was the mirror's own parameter

`panel_header` gains `accent`. `PanelHeader.qml` has `property color accent`; the old boolean
`active` cannot distinguish a live capture, `critical`, from a ready one, `activeColor`. `info_badge`
ink follows a live ground because `badgeColor: paused ? warning : critical` changes peach to red
mid-capture. `action_button` gains `danger`, `height`, and a `glyph` slot. `panel_toggle_card`'s
icon and height become optional because a frame-rate tile has no glyph, and
`modelData.icon ?? ""` would draw an empty line.

### Every glyph on the bar was a third too large

`components/icon_button.lua` defaulted its glyph to `theme.icon.lg`, despite claiming it matched
`iconSizeFor("md")`. `IconButton.qml` defaults `size: "md"` and bar indicators do not override it:
the mirror uses `iconSizeMd`, `s(18, 14)`, while `icon.lg` is `s(24, 18)`, the mirror's
`iconSizeLg`. Every extracted bar circle therefore drew an oversized glyph. Filled squares exposed
the error first.

### The hole, and the one thing not built

Pause arithmetic is `state`: it survives an in-place reload and resets on a generation swap, after
which paused seconds count as recorded. The mirror has the same hole across a Quickshell restart; a
debounced disk write per pause is not worth closing it.

`IPC.qml`'s `rec toggle` has no equivalent. `obelisk set` writes a value and a reader re-renders,
but starting a recording is a call. The only config hook that runs code is a capability's `on_change`;
on `obelisk.system` a keybind answers up to a second late. This remains a roadmap item.

A live capture paused at `1:47` for seven seconds, resumed, then read `1:51` four seconds later.
The stopped file was 9.3 MB and `ffprobe` reported `duration=112.905512`; `SIGINT` closed the
container and gpu-screen-recorder's duration agreed with the arithmetic within two seconds.

## 0177. The bar draws glyphs in the body font; only panels use the icon font

The private-use characters extracted from `Modules/Bar/Indicators/*.qml` match `config/icons.lua`
for 25 of 26 entries; `wallpaper` is the exception. Size was already corrected by ADR-0176.

`Config/Theme.qml` declares two faces:

    readonly property string fontFamily:     "CaskaydiaCove Nerd Font Propo"
    readonly property string iconFontFamily: "JetBrainsMono Nerd Font Mono"

The split is bar versus panel, not glyph versus text. `IconButton.qml`, `NetworkIndicator.qml`,
`DateTimeDisplay.qml`, and `BatteryIndicator.qml`'s `OText`s use `fontFamily`; `PanelRow`,
`PanelHeader`, `PanelToggleCard`, `OSDCard`, `AppLauncher`, and `LockContent` use `iconFontFamily`.
`components/icon_button.lua` passed `theme.icon_font` to every bar circle, so the codepoint was
right but the face was wrong. `components/glyph.lua` needs no change: its callers are the panel
components. ADR-0144's rule that a glyph uses the icon font is narrower than stated.

### The battery pill, which the comparison also settled

1. **Green for a healthy battery.** `batteryColor` is `critical : warning : Theme.activeColor`.
   Green was a fourth state absent from the mirror, so it is removed.
2. **A fill tinted to 38%, and one readout colour.** Full-opacity accent matches the other lit
   controls, so the tint is removed. The readout uses
   `textContrast(percentage > 0.6 ? batteryColor : bgColor)`: at 60% the text centre crosses from
   the fill onto the pill. Both lines are `bold: true`.
3. **The charge glyphs were swapped.** `isPendingCharge` is tested first and gets the bolt;
   everything else on mains gets the plug. Charging therefore draws the plug, while a battery at a
   charge limit draws the bolt. The mirror uses that order; ours did not.

The power glyph measures 13x15px here against the mirror's 14x16, confirming the size and pointing
to the font face.

## 0178. A tween that only changes what a node paints does not lay the tree out again

ADR-0145 made a compositor frame callback the tween clock and `Scene::tick` the frame. That tick
cloned the retained tree, built a fresh taffy tree, re-parsed every `LayoutStyle` and `PaintStyle`,
solved, and measured. `opacity`, `background`, `border_color`, `foreground`, and `radius` are read
by neither `taffy_style` nor `measure_for`, whose text inputs are content, size, family, and
wrapping. `dev-config` has `background` in eight `animate` blocks, `border_color` in four, and
`width` in five.

If every running tween names a paint-only property, `node::advance` writes displayed values into
the retained map and the node re-derives its opacity and parsed paint in place. No clone, solver,
measurement, or geometry publish is needed. `width`, `padding`, and a leaving node whose exit drops
it still use the existing relayout path.

`transform` stays on the layout path although the solver ignores it. ADR-0149 maps the pointer back
through a node's inverse transform, so input regions must be rebuilt with it. It stays on the layout
path until something rebuilds those regions without a full pass.

**The clone was rollback, and the fast path still needs one.** `node::advance` may write a value a
parser refuses: a spring can overshoot, and `parse_opacity` rejects values outside `[0, 1]` rather
than clamping (ADR-0068). Working on a clone made refusal free because the half-advanced tree was a
copy. In place, a refused value left in the retained map is re-read by the next pass and fails too,
turning one bad frame into a scene that stops updating. The fast path saves the values it is about
to move and restores them on refusal, bounded by that node's tweens rather than its subtree's
properties.

**`Scene::tick` returns the instances it advanced, not whether any did.** The poll loop used to
repaint every mapped surface after any resolved turn, rebuilding display lists only to reject them
as equal. The tick records touched trees before advancing them, because the frame ending a tween
shows its target even though the tree is no longer `animating`.

**A mid-tween surface whose list is unchanged commits without drawing.** The commit makes the frame
request effective, but identical pixels need no redraw. A hold, lead-in `delay`, or step easing on
one value costs a commit instead of make-current, clear, draw calls, and swap.

Measured on the real `dev-config` during a continuous keyframe tween, debug build, 60Hz, idle
machine: tick turns fell from p50 4.98ms / p90 6.27ms / max 30.41ms to p50 1.42ms / p90 2.15ms /
max 3.83ms. Turns over the 16.6ms frame budget fell from 2 in 2959 to 0 in 2670. Every dropped
frame after the change was a full capability pass.

Rejected: replacing the whole-scene rollback clone at `Scene::apply_admitting` with borrowed
preparation. It measured p50 0.38ms against a p50 10.79ms pass, about 4%, so the ownership and
staging were not worth the risk.

Rejected: per-surface dirtiness instead of ADR-0044 decision 2's single flag. It needs the sources
each surface reads, but config getters are arbitrary Lua and may read the clock or a mutable upvalue.
That requires a reactivity contract or conservative fallback.

Rejected: using the frame callback's `time` instead of `Instant::now()`. It is milliseconds against
an undefined epoch, needs wrap handling and a multiple-surface rule, and the presented-frame step
already measured p10 16.34ms / p90 16.83ms. Its target is only the residual 2.9% of outlier steps.

**Amends ADR-0152.** A sequence's `resting` flag is re-derived from the clock used by the pass, not
carried from the tick. `delay` sits beside the motion in the spec, so a played-out run handed a fresh
`delay` still matches as the same list and carries `resting = true` across; `advance` skips a resting
tween and `animating` does not count one, so nothing asks for the frame that would start it and the
run stays on its old last frame. Under one monotonic clock, a counted sequence that is done stays
done, so re-deriving costs nothing and differs only where carrying was wrong.

**What this does not fix.** The first frame revealing a large surface still costs 204ms for the
update panel, 97ms then 50ms for the notification area, versus 5.84ms for later panel tween ticks.
The cost survives surface recreation. The repaint phase still spans mapped surfaces, EGL binding,
swap, and `ImageCache::upload_landed`, which charges landed background decodes to the first surface
that paints.

## 0179. A dev build optimizes its dependencies, because the frame is mostly their code

ADR-0178's first update-panel frame cost 204ms, versus 5.84ms for later ticks. Splitting
`layout::paint::execute` by draw kind found text drawing at 272.8ms of a 301.5ms frame,
`ImageCache::upload_landed` at 0.0ms on every frame, and `draw_clipped` at 37.1ms on the cold frame
and about 0.3ms elsewhere. femtovg rasterizes each glyph and size into its atlas on first use;
the `TextPainter` and warm atlas outlive the surface. `draw_clipped` has no cold/warm distinction,
so its proposed pool is not worth adding. Layout measurement uses the cosmic-text worker, while
paint hands the string to femtovg separately.

Workspace code was not the cost. Building dependencies at `opt-level = 3` while workspace crates
remain unoptimized reduced first paint from 272.8ms to 9.3ms and a 5162x2160 wallpaper decode from
9636ms to 162.7ms, within about 1.5x of release. The one-time dependency build took 3m52s; an
incremental workspace rebuild stayed at 0.7s. ADR-0178's measurements are `dev` comparisons, not
release-shell comparisons.

Rejected: warming the atlas by drawing chrome text before display. It moves the cost to startup,
needs a config-synchronized warm list, and misses runtime-generated text.

Rejected: setting `opt-level` on workspace crates too. Debugging workspace code is the point, and
its code was not the expense.

Still open: the wallpaper. It costs 162.7ms in dev and 114ms in release on the render thread at
startup and every change because `Load::Inline` is the default chosen by ADR-0122 decision 2, and a
wallpaper box exceeds `image::thumbnails::size_for`'s largest thumbnail. Decoding costs 40.7ms and
resizing 11.1 megapixels to 3.4 costs 54.5ms, so there is no scale-on-decode shortcut: a 5162x2160
image covering a 1920x1200 box needs 2868x1200, while the next DCT step undershoots. Move the work
off the frame rather than optimize it; the tradeoff is a blank first frame.

Amendment (ADR-0180): closed. `retain` moves it off the frame without the blank by drawing the
picture already held until the replacement lands. A cold start still shows the panel's ground while
the first file decodes because nothing exists to retain.

## 0180. An `image` can hold the picture it already has while the next one decodes, because the alternative to a stalled frame was a blank one

ADR-0122 left the wallpaper without `async`: a complete first frame beat a fast one. ADR-0179
measured 162.7ms of dev decode and 114ms in release at every wallpaper change. Enabling `async`
moves decode off the frame but flashes the panel's ground because it draws nothing until landing;
leaving it off keeps the stall. Nothing in between existed because a pending image had no memory of
what it drew last.

1. `retain = true` on an `image`. While the resolved `source` has no texture, the node draws the
   last source that had one. It is opt-in: a picker tile changing files must not show the previous
   picture, while a wallpaper should.
2. Store the memory in `ResolvedNode::displayed_source`, carried across passes beside `tweens`, not
   in an `ImageCache` node table. The cache is keyed by path, box, and file version and cannot know
   which node drew a source.
3. `ImageCache::poll` already names files whose pixels arrived, and `App` invalidates every list
   drawing one. `Scene::note_landed_images` reads that list one line earlier, so the existing
   repaint
   builds from the source the node has caught up to. No second readiness channel or restructuring
   of `paint_surface` around its list-equality check is needed.
4. The cover reaches the display list only while it differs from `source`, preserving ADR-0063's
   repaint skip once settled. It is pinned for `ImageCache::trim` at its drawn box, or the 16MB idle
   budget can free the texture covering the gap.
5. A decode failure keeps the old picture. `Load::Background` returning `None` covers decoding,
   failure, and pool refusal; all three mean "show what you have". The cache already logs failure
   once.
6. `retain` is inert under `Load::Inline`, which finishes before drawing. It is accepted without
   `async` because `async` is a signal-valued property a config may flip.

The node must survive source changes, so the wallpaper `image` gets a stable `id` and puts the path
in `source`. An image keyed by path becomes a new node each time and has nothing to retain. That is
right for picker tiles and wrong here.

Rejected: starting the transition from the scene by giving `Scene::apply` the cache. Layout and
paint share a thread, but the cache reference would cross a file whose paint properties are parsed
once per apply and read every frame. The cue already exists.

Rejected: making retention the default for every `async` image. It suits a wallpaper and breaks
album art.

This closes ADR-0179's open item without a transition. A cross-dissolve needs the same landing frame
to start.

## 0181. A `transition` crosses an image from the picture it is holding to the one that landed, and femtovg draws the dissolve because two draws is all a dissolve is

ADR-0180 swapped the held picture for the landed one in one frame. The planned GL stage would port
the reference config's six `.frag` files and fall back to a cross-fade. The fade needs no GL: it is
one of those six effects and the two draws are its implementation, so it comes first.

1. `transition = { duration, easing }` on an `image`. It implies `retain`, because a dissolve needs
   the held picture as its outgoing endpoint. Writing both would permit inconsistent config.
2. No `effect` key yet. Cross-dissolve is the only effect until masks arrive. Unknown keys are
   refused, so `effect = "Wipe"` fails instead of silently selecting a fade.
3. The run is `ResolvedNode::dissolve`, holding the outgoing source, start instant, and eased
   progress. `displayed_source` has moved to the incoming at landing, so the outgoing needs its own
   storage.
4. Progress is stored on the node, not read from the clock during paint. The display list is built
   once and compared for equality, so a clock-read value would defeat the skip.
5. It advances in the paint-only walk outside that walk's `!node.tweens.is_empty()` gate and is
   dropped when its duration ends. It counts for `animating`, asks for another frame, and is not
   tested by `tick_is_paint_only` because it changes only alpha and ADR-0178 can carry that change
   in place. The old placement behind the tween gate left an image with only `transition` frozen at
   progress zero; a test now ticks exactly that node.
6. The outgoing stays at full alpha while the incoming fades over it. Two source-over draws at half
   alpha compose to three quarters, exposing the ground through the missing quarter. The bottom
   layer never fades.
7. One `Draw::Image` field carries both the ADR-0180 cover and dissolve source. A node does not draw
   both at once: the cover exists while `displayed_source` differs from `source`, and a dissolve
   starts when they match. Both are pinned for `ImageCache::trim`, or the outgoing is freed
   mid-cross.

Not built: a queue of parked sources. A mid-dissolve change replaces the run on the next landing,
matching the reference config's one-slot `pendingUrl`. Also not built: holding an incoming decode
until the current dissolve ends. The pool is bounded and the cover stays visible, so ordering gains
nothing.

Rejected: shipping the atomic-swap placeholder and drawing the fade in the shader stage. It is more
code before the two `draw_file` calls that already implement the fade, leaving a commit whose only
visible effect is no change.

Rejected: expressing the dissolve as a `Tween` on a synthetic property. Tweens write to the retained
property map, which `paint_style` re-reads and `lua::nodes` validates names against; a private key
would collide with a future config key.

## 0182. The pin set is what a mapped surface shows, not what it last painted, and the texture budget is a property of the displays

ADR-0181 made picker thumbnails blink during wallpaper changes. `ImageCache::trim` reported:

```
trim resident=25807KB budget=16384KB pinned=2 evicting=33 first="kitty.svg"
```

The two wallpapers were pinned; the 54 thumbnails on the visible picker were counted idle.

1. `forget_painted_lists_drawing` cleared `last_painted`, which is also the pin set read by `trim`.
   A `stale` flag now forces repaint while preserving the list for pinning. The clears for a
   destroyed surface or fresh EGL surface remain, because those pixels are gone.
2. The bug was harmless for a single landing because the surface repainted in the same turn. A
   wallpaper mid-dissolve repaints every frame while `trim` runs, so thumbnails were evicted,
   decoded, landed, and unpinned again.
3. `TEXTURE_BUDGET` becomes `wayland::output::texture_budget`: one physical-pixel RGBA screenful
   per output, floored at the old 16 MB, recomputed when outputs change.
4. `ImageCache` receives the number instead of deriving it. Display geometry determines the working
   set; the cache only knows paths and pixel boxes.

The constant was measured on one 1920x1200 laptop against one 54-file picker. A single 4K
wallpaper is 33 MB, so it can exceed 16 MB permanently; three monitors changing wallpapers were
never represented. Display geometry scales with each texture's drawn box.

Rejected: raising the constant. It moves the cliff to the next display size and still needs the
geometry already available.

Rejected: skipping eviction when pinned entries alone exceed the budget. `victims` would evict all
idle entries and remain over budget. With correct pins there is no consumer for that behavior; fix it
when one appears.

## 0183. Readiness is what a paint drew, not what a decode landed, because only the draw holds the key

ADR-0180 used `ImageCache::poll`'s landed-file list as readiness, but that answer is wrong in three
cases:

- `poll` pushes a path for a **failed** decode as for a successful one (`image/mod.rs`), so the node
  dropped its retained picture despite ADR-0180 decision 5.
- A source already in the cache never lands, so it triggered no move or dissolve.
- A landing names a **path**, while the cache key includes path *and* box. A thumbnail decode could
  therefore declare a full-size image ready when no full-size texture existed.

1. `layout::paint::execute` returns the `image` nodes whose named source it drew, and
   `Scene::note_drawn_images` moves those nodes on. The draw holds the exact key and distinguishes
   failed, cached, and fresh sources. `poll` becomes only a repaint cue.
2. `Dissolve` carries both endpoints. A third source arriving mid-run no longer swaps the
   destination under it and removes its pin. A successor waits and crosses from where the current
   run leaves the node, one slot like the reference config's `pendingUrl`.
3. A transition covering a gap draws the incoming at zero before proving its texture. Readiness now
   asks at the alpha where the cross opens, so the first frame with the texture draws it whole,
   snaps back to the outgoing, then crosses.
4. `animating` is re-read after the report. A dissolve starts during paint, after the decision to ask
   for another frame; this avoids the ADR-0181 tween-gate mistake.
5. Progress is clamped where stored. `Easing::apply` clamps input, not output, so Back, Elastic, and
   Bounce can leave `[0, 1]`; this value is an alpha, not a property parser input.
6. `DisplayList::drawn_images` pins the box under which the cache entry is drawn through shared
   `image::cache_box`. A vector key is squared, so an SVG drawn at 200x40 could pin 200x40 against
   an entry at 200x200 and `victims` would miss it.
7. `CACHE_CAPACITY` evicts the least recently asked-for entry, not the oldest insert. This path
   never sees the pin list, and the oldest entry is typically the wallpaper. The insertion-order
   queue is deleted; `last_hit` ties break by insert order now that `insert` takes its own tick.

Amends ADR-0181 decision 7. A node can cover a gap and have a dissolve active when a third source
arrives, but they never need drawing at once; the run owns the frame until it ends.

Amends ADR-0182 twice. `texture_budget` no longer multiplies by `scale`: `App::paint_surface` builds
and executes every list at scale `1.0`, so `ImageCache` boxes are in `Screen::width` units and the
old calculation budgeted four times a HiDPI output's capacity. Also, the budget is not an allowance
for idle textures beyond surfaces. `trim` compares total resident bytes and evicts only unpinned
entries, so a pinned working set over budget leaves it evicting everything idle and still over.

Not fixed:

- The two source-over draws are exact for opaque images at `opacity = 1` and approximate otherwise.
  Transparent incoming pixels show the outgoing through them, and node `opacity` below 1 reads
  denser mid-cross than at either end. Exact mixing needs one offscreen target per frame or the
  shader stage. A per-frame full-screen target is not worth adding before that stage.
- A pool-capacity refusal retries on the next paint, but an unchanged list can skip that paint
  indefinitely. Retention leaves the old picture up, so this is a stall, not a blank. Retry when
  capacity frees.

## 0184. A transition's effect is a config's fragment shader, because the reference's six were never Quickshell's

ADR-0181 left five effects for a GL stage with engine-owned names such as `effect = "Wipe"`. The
reference's six `wp_*.frag` files are in `~/.config/quickshell/`, the user's directory; Quickshell
ships `ShaderEffect`, and the shaders are user code. ADR-0055 already rejected a `wallpaper`
capability and wallpaper-specific Rust code, so a `Wipe` arm repeats that mistake.

1. `transition = { duration, easing, shader, params }`. `shader` is an absolute path named through
   `obelisk.config_dir`, as the default wallpaper is. Omit it for cross-dissolve. The engine ships no
   effects; `dev-config` ships five samples.
2. The engine owns the vertex stage, prelude, and epilogue. The prelude declares the contract and
   sampling helpers, then `#line 1` makes compile errors point into the config. It `#define`s the
   config's `main` to `obelisk_effect`; the engine's `main` calls it and multiplies the result by the
   node's `opacity`. A config shader cannot be trusted to preserve the node's inherited `opacity`.
   Documenting that rule and trusting the config would be a promise without a mechanism.
3. The CPU fits each endpoint and hands over a rect, not a normalised plane. The shader does not
   repeat fit arithmetic or disagree with ordinary image drawing, and nothing renders twice. The
   ported files are about 20 lines each because the reference's `sampleWithFillMode` prelude is
   unnecessary.
4. `params` are named floats, all set on every draw. Otherwise programs sharing a shader inherit
   each other's uniforms; omitted parameters are zero.
5. Programs are keyed by path and file version. Editing an effect recompiles it and un-refuses one
   that failed to build, so reload replaces restart.
6. Every texture is premultiplied at upload. `image` decodes straight alpha while `resvg` decodes
   premultiplied; a shader cannot receive both conventions. Multiplying after sampling fails because
   lookup filters between texels first, interpolating hidden straight-alpha colour.
7. Failure falls back to cross-dissolve. Compile, link, non-float parameter, and quad failures log
   once and fall back. This is why the dissolve preceded the stage (ADR-0181).

The stage flushes femtovg, captures and restores changed GL state, draws one quad, and never binds
framebuffer zero. It applies scissor because femtovg's path clipping does not clip this quad. The
quad carries the node affine and target origin computed on the CPU.

Not claimed: containment. A shader that loops forever hangs the GPU and session. Config shader code
already has the trust level of `process.run`, with a worse failure mode.

Not fixed: the fallback dissolve remains two source-over draws, exact for opaque endpoints at full
opacity and approximate otherwise (ADR-0183). An engine-owned exact fallback is the next step, but
putting this new stage on every transition now is the documented limit.

Rejected: `effect` as either a built-in name or a path. One field with two meanings reads as a menu
with an escape hatch and must answer which names exist forever.

Rejected: a general shader node over an arbitrary subtree. Two endpoints and progress are a stable
contract; an arbitrary subtree needs offscreen targets, clip interaction, and defined inputs.
`docs/roadmap.md` keeps it parked.
## 0185. A request the image cache drops has to say so, because the retry is a paint and an unchanged surface never paints again

`ImageCache::image` drops requests when the pipeline reaches `MAX_INFLIGHT_DECODES` or its job
channel is full. The old retry assumed that "the next paint asks again", one retry per frame. It
does not:
`paint_surface` returns before `execute` when the display list equals `last_painted` and the surface
is not `stale` (ADR-0063). An unchanged wallpaper therefore never asks again.

Two halves are needed and neither is sufficient:

1. **A frame callback**, so a turn happens at all. `take_deferred` is read straight after the
   surface's own `execute` and carried into `animating`.
2. **The surface staying `stale`**, so that turn's paint does not skip on an unchanged list.

Arming the callback only sets `animation_frame_due`; it advances no tween, so `Scene::tick` names
nothing and tick-based repaint selection reaches nothing. `stale` is therefore its own repaint
reason, and `narrowed_repaint_targets` must include it rather than narrow it out by tree identity.

A `Pending` entry evicted for capacity or budget leaves the pool's `wanted` set; the worker skips it
and sends no result. A surface holding a `retain` cover would wait forever. Cancellations therefore
drain through `poll`, whose existing meaning is "these files changed, invalidate the lists that draw
them", alongside landings.

A poisoned `wanted` lock is separate from a full one. `std` poisoning is permanent, so retrying every
frame would spin forever; capacity clears and owes a repaint, while poison says nothing.

The admission gate is a method, and repaint narrowing a free function. `image` needs a GL canvas and
`TrackedSurface` holds Wayland objects, so neither is testable directly. This follows `victims`
(ADR-0123).

Tests cover the refusing ceiling, consuming the flag, one invalidation for a cancelled decode, and a
stale surface surviving narrowing to a different tick. The main loop's outer gate, where a stale
surface makes the turn reach a repaint, is verified by reading `wayland/mod.rs` because it needs a
compositor.

Amendment, ADR-0192: reading it was not enough. The gate was right and the branch under it was not,
and the decision is extracted and tabulated there for the same reason `narrowed_repaint_targets` was
extracted here.

## 0186. The engine's cross-dissolve is a shader like any other, because two source-over draws are not a cross-dissolve

ADR-0181 built the dissolve from two femtovg draws: the outgoing at the node's `alpha`, then the
incoming at `alpha * progress`. ADR-0184 added a shader stage for config effects but left those
draws as the fallback, "exact for opaque endpoints at full opacity and approximate otherwise".

At `alpha` 0.5 and `progress` 0.5 the pair composes to 0.625 opacity where 0.5 is correct. A
transparent incoming pixel also keeps the outgoing pixel underneath it.

The engine now owns [`FADE`], written and assembled through the same prelude and epilogue as a config
effect. Textures upload premultiplied (ADR-0184), so `mix` of the endpoints is the composite; the
epilogue applies node opacity once. Sampling is consistent with and without effects.

A config shader that will not build falls back to `FADE`, not the two draws. Losing an effect is not
a reason to lose correct compositing.

The two draws remain for no GL context, an endpoint without a texture, or an engine shader that will
not build. The first is the test harness; the others are real.

Every transition now pays `canvas.flush()`, GL state capture and restore, and one quad. An opaque
wallpaper at full opacity used to pay two fills and already be correct; ADR-0184 left this alone for
that reason. The stage now runs constantly, so faults surface without a config effect.

Not tested by construction: the composite itself. The stage needs a GL context and the paint
harness has none, so this is verified live -- a translucent image mid-cross over a known ground,
measured against the value the arithmetic predicts, with the old code as the control.

## 0187. What fits is a property of the decode pool, not of one worker's share of it

`MAX_DECODE_ALLOC_BYTES` was 64 MiB passed to `image::Limits::max_alloc`, while its doc made the pool
ceiling 64 MiB times `MAX_DECODE_WORKERS`, or 256 MiB. A 6024x3401 wallpaper decodes to 78 MiB,
inside 256 MiB but above a 64 MiB quarter, so it was refused while three quarters of the budget sat
idle. Its file was only 380 KB on disk.

The ceiling is unchanged. Where it is enforced moves: `Budget` is a byte count shared by the pool,
a worker waits until its decode fits, and the permit is RAII because `decode_raster` has a dozen
`?` exits and every one has to give the bytes back.

Details that are the whole difficulty:

1. **The size comes from the decoder, not dimensions.** `total_bytes()` is exactly what `max_alloc`
   checks. `w * h * 4` is wrong both ways: 16-bit needs `w * h * 8` and would be admitted at half
   its
   cost; the RGB8 JPEG needs `w * h * 3` and would be charged a third more.
2. **One decoder answers and works.** `into_dimensions` reads a whole JPEG, 8.7 ms against a PNG's
   27 µs here. A second reader would pay that twice.
3. **Probe after source selection.** A covering cached thumbnail returns before opening the source;
   an SVG rasterizes to `box_px`. Charging before dispatch would queue a 256-pixel thumbnail behind
   a wallpaper.
4. **An empty budget admits any decode.** No waiters can wait only on each other. A single decode is
   still refused above `DECODE_POOL_BYTES`, admitting one *at* the ceiling but not above it. Without
   that refusal, an 8192x8192 16-bit source would run alone at 512 MiB, twice what four workers can
   reach.
5. **An inline decode charges but never waits.** It runs on the Wayland dispatch thread; waiting
   stalls dispatch, Supervisor reads and input. It is counted for worker accounting and allowed
   through.
6. **A worker rechecks wanted after waiting.** An entry can be evicted while it sits in `acquire`.

The bound counts the decoder's output while it produces it. `decode_raster` scales and converts
alongside the charged buffer; a finished decode holds pixels in the result channel until `poll` and
in `landed` until paint uploads them; an inline decode charges without waiting. High-water therefore
exceeds `DECODE_POOL_BYTES` by the largest of those amounts. The upgrade path is holding the permit
until `upload_landed` consumes the pixels, with the permit travelling with the result.

Tests cover the pure admission rule and deadlock case, permit return and waiter wakeup on every
decode exit, eviction during the wait, and the 78 MiB case end to end. The fixture is a solid colour
at the real dimensions, 400 KB on disk and 19 ms to encode: the decoder charges dimensions, not
entropy.

Amendment, ADR-0193: the thumbnail reader's `Charge::Free` claimed a bound this pool never checked.

## 0188. A program handed to the user is let go of, not merely put in its own process group

`applications.launch` and `open_url` used `spawn_group_leader` and dropped the handle, calling that
detached. A process group is not detachment: the program remained a direct Supervisor child and one
registry entry away from reaping on config reload.

`spawn_detached` does the real work: `setsid` in the forked child, fork again, and exit the
intermediate immediately. The orphaned grandchild is adopted by `init`. Calling `setsid` before the
second fork prevents it acquiring a controlling terminal, since only a session leader can.

Its three standard streams go to `/dev/null`; inherited streams would let it write to the shell's
log after it stopped being related to it.

The config API is `process.detach(cmd, args)` beside `process.run`. They are separate actions because
`run` has a handle, two pipes and an exit code, none of which survives detachment. A
`detached = true` flag would make three of `run`'s four arguments meaningless and leave
`ProcessHandle:kill` pointing at an unnameable process. `detach` returns nothing, retains no callbacks
and registers nothing to reap.

The config cannot kill, wait on or read a detached program. That is required for an editor opened from
the launcher to outlive a config edit or shell exit.

The program writes its own `$PPID`, which must not be this process. `spawn_group_leader` fails that
assertion.

## 0189. A submit reaches `on_submit` before the `on_change` that reports the field clearing

Enter on a `textfield` empties the buffer and reports `on_change("")`. That report used to precede
`on_submit`, so the config received the empty field first.

`dev-config`'s launcher derives selection from its query. Typing `calc` and pressing Enter ran
`on_change("")`, rebuilt results for the empty needle, then ran `on_submit`, launching the first
entry of the full list, Avahi SSH Server Browser, for every query.

The list had already been rebuilt against the empty query before drawing, making this look like
"Enter ignores the selection". No error or callback failure occurred; `on_change` reported the right
text on every keystroke.

Submit carries the user's intent and goes first. The clear follows and still delivers the empty string
because the field is really empty by then. Only the order changes.

The delivery is a free function rather than four blocks in the key handler, whose whole `App` cannot
be tested. Two tests pin it: a submit delivering
`on_submit("calc")` then `on_change("")` in that order, and an ordinary keystroke still reporting
its text through `on_change` alone.

The running config showed the ordering bug; the config itself was correct.

## 0190. A config declares how long its lock screen takes to leave, and the Supervisor still owns the leaving

`LockScreen.qml` animates in and out. Its exit is two 147ms stages, ending with
`LockService.finalizeUnlock()`; QML holds and releases the lock.

That shape cannot be copied. ADR-0042 deliberately gives `obelisk.lock` no `unlock` action: a Lua
callback or `finalize_unlock` would create a one-click path past PAM. It would also let a config
exception or unfinished animation leave the lock up after the correct password.

The config declares its duration once with `("lock", "set_unlock_animation")`; the Supervisor
schedules release when PAM answers. `LockState.unlocking` reports that window, and config code cannot
extend, cancel or fail to end it.

The delayed release names its acquisition. `record_authentication` refuses a PAM answer for a lock
no longer on the glass, but a timer can outlive that check and release a later lock. It therefore
re-reads the acquisition and `unlocking` before sending, and does nothing if either changed.

Runtime shutdown inside the window drops the sleeping task and leaves the compositor locked after a
correct answer. The window is at most 600ms, but the hole is real. The fix is to hold the pending
release in `main.rs`'s loop and flush it on shutdown.

`MAX_UNLOCK_ANIMATION` is a 600ms ceiling. A ten-second request would look like a hung shell while
the user is authenticated. A malformed argument means no animation rather than refusal, so the lock
still comes down.

The entry animation was broken. `dev-config`'s lock used `animate`'s `from` (ADR-0146), which applies
only when a node has no displayed value. Its subtree outlives the lock, so the card already had
`opacity = 1`; locking was instant.

Both edges use a computed `up` flag, meaning the compositor granted the lock and PAM has not
answered. It drives opacity and scale in both directions on an existing node. It is keyed on
`active`, not surface existence, because `ext_session_lock_v1` withholds `locked` until every output
has presented a frame; a fade during that handshake would finish before the screen appeared.

Every lock ending shuts the window, including a relock during one. An idle timer may fire while the
last unlock plays, and the new lock screen must not start in its exit state.

## 0191. A list's frame time is set by the length of its source, and the viewport that would fix it is invisible to the stage that builds the items

Measured on a cached re-apply, six nodes per row, one text
(`layout::scene::tests::list_pass_cost`, release):

| rows | p50 |
|---|---|
| 12 | 0.53 ms |
| 50 | 1.70 ms |
| 125 | 3.98 ms |
| 500 | 15.1 ms |

The cost is linear, about 32 us per row. Twelve rows fill one wallpaper-picker viewport; 125 rows
represent 500 wallpapers at four per row; 500 rows represent 2000 wallpapers and exceed the frame
budget before painting. Config data, not tree shape, sets this open-list cost. ADR-0044 decision 2
makes the pass per-capability-push, while ADR-0124's hidden-subtree freeze keeps a closed picker at
zero.

Two cheaper answers are closed. ADR-0132 measured delegate memoization at 19% of the pass; the rest
is resolution, layout and measurement. Text nodes account for 30% of the 125-row figure, including
resolution and layout, while every measurement is a shaping-cache hit. A config cannot window its
own `source`: ADR-0069 decision 2 hides measured extents from Lua, so visible-row data would make
the scroll clamp collapse to the supplied height.

The engine must window it. Items are built in `children_of`, which receives only the property map;
the list's solved viewport is unavailable because taffy has not computed it this pass. The previous
pass has both: `prepare` holds the retained node, children have solved rects, and `scroll` is in the
property map. Read that rect with overscan, accepting one pass of staleness.

This is a structural change, not a filter, and is separate from the four bugs that prompted the
measurement:

1. **Skipping items breaks positional pairing.** `pair_children_by_id_then_position` falls back to
   position, but a window has no stable position. Virtualization would require `key`, which configs
   can be told but cannot receive by default.
2. **A scrolled-out item is not removed.** Unclaimed retained children become `leaving` and animate
   out (ADR-0150). The window must retain them without layout, close to `frozen` but distinct.
3. **The content extent must survive the window.** Twelve children replacing 125 collapses the scroll
   bound from ADR-0069 decision 4. Leading and trailing extent must be restored without affecting
   hit
   testing, duplicate-key checks or `spacing`.
4. **A `geometry` signal on an unbuilt item goes stale.** ADR-0147 publishes solved rects to a config
   handle; a windowed list publishes only its window, which must be documented.

Not built. The real consumer is the wallpaper picker over a large folder, where rows are reached by
scrolling rather than search. The table is the reproducible trigger.

## 0192. A pass and a tick owe the screen different things, and one flag cannot say which ran

ADR-0178 narrowed a tween repaint to instances named by `Scene::tick`; ADR-0185 added `stale` for
decode refusals that no tree can ask for. The main loop used `re_resolved` for both meanings: it
starts as "a pass ran" then widens to "a pass ran or a tick advanced something", and narrowing read
the widened value.

A re-resolving turn does not tick. With any `stale` surface, the narrowed arm therefore received an
empty tick list, repainted only the stale surface, and hid the panel, window or rewritten surface
changed by the pass until another repaint reason appeared.

The meanings now have two names. Pure, tabulated `repaint_for_turn` makes `passed`, `typed` and
`landed` scene-wide; `ticked` and `stale` narrow the set; nothing owed paints nothing. It sits beside
`narrowed_repaint_targets` because the decision needs no `TrackedSurface` and has been wrong twice.

The same flag chose how much protocol state to push. `apply_resolved_surface_state` runs
`apply_resolved_state` over every tracked surface, parsing the role spec and doing a `wl_region`
create/add/set/destroy per surface. `apply_input_region` deliberately does not diff because a GPU
repaint costs more than the round trip. That is true for a pass, not a tick: one panel fading at
60 Hz walked eighteen trees and issued seventeen region round trips per frame for unpainted surfaces.
The narrowed apply takes the instances named by the tick. `Scene::tick` mutates only returned trees,
so unnamed surfaces still match their last pushed tree.

One push item is outside the scene. ADR-0051's popup latch reads `pointer_input_count`; its reopen
serial is armed by a press or release and cleared at that turn's end (ADR-0049 amendment). An
`on_click` that sets already-true `visible` re-resolves nothing, so narrowing could leave the popup
shut. The latch now has its own popup pass whenever a serial is armed and the full apply did not
cover it.

Both halves are pure functions, `repaint_for_turn` and `surface_state_for_turn`, tested over inputs.
ADR-0185's main-loop gate needs a compositor (`wayland/mod.rs`); these decisions do not.

## 0193. A thumbnail is only a thumbnail because of the directory it is in, so the reader has to check

`$XDG_CACHE_HOME/thumbnails/normal/` is shared by every process of the user. Its path contract caps a
file at 128 pixels on its longest edge. `read_valid` checked `Thumb::MTime` and `Thumb::URI`, then
decoded any pixels. That decode was `Charge::Free`, taking no permit from the pool ADR-0187 built, on
the grounds that a thumbnail is bounded by the caller's slot size. Nothing enforced the bound, so a
full-size PNG with matching mtime could decode uncounted up to `MAX_DECODE_EDGE` beside four
charged workers. The `png` reader's header is already open, so checking it is free.

The header check alone is not enough: `read_valid` validates one open, then decodes a second pathname
open that this user-writable directory can change between them. The slot size is therefore passed to
`decode_within_limits` as the decoder's edge limit, using the same mechanism as `MAX_DECODE_EDGE`.
The header check remains as the cheaper early refusal; the decoder limit makes `Charge::Free` safe.

The writer also trusted `.obelisk-{pid}-{mtime_secs}.png.tmp` to be unique. The four workers share a
process, so two sources with the same mtime second chose it. The loser failed `create_new`, then its
cleanup unlinked the winner's file, making both renames fail and forcing full-size re-decodes. A
process-wide counter now names temps, and creation is outside the cleanup closure, so cleanup cannot
reach a file this call did not make. The final md5 name cannot be the temp name because two writers
for one source share it.

A pid is unique only among live processes. A temp orphaned mid-write could block a reused pid, and
nothing above `write` retries. A taken name is therefore retried a few times, costing four `open`
calls in the impossible case but avoiding a cache that needs manual clearing.

## 0194. The thumbnail a decode just wrote is the right source for the texture it is about to make

`decode_raster` scaled the full source twice on first open: to `slot.px` for the freedesktop cache
and to `stored_size` for the texture. Two downscales of a 4096x4096 image took 54.4 ms versus 26.8
ms for one, with the second starting from pixels already reduced to 128 on a side.

Every later open reads the cached PNG and scales it to `stored_size`. Scaling from the thumbnail
makes first and later opens produce the same pixels.

The thumbnail must cover `stored_size` in both axes. `stored_size` fills the box while `thumbnail`
fits inside it: a 16:9 wallpaper in a 128 box becomes 128x72 and stores at 228x128, so reuse would
upscale it. Square-ish sources take the shortcut; wide ones keep full-source scaling.

## 0195. `blur` is opt-in per node and the region is derived from where that node is painted, because "what can be clicked" has one right answer and "what should be blurred" does not

`ext-background-effect-v1` has three requests -- `get_background_effect(wl_surface)`,
`set_blur_region(wl_region)`, `destroy` -- and niri implements it. The no-client alternative is a
niri `layer-rule` matching our `obelisk-{id}` namespace.

The rule was measured and rejected. With a full-screen click-catcher and one 620x260 card over a
striped backdrop, luma spread far from the card went from 224 off to 5 on. It blurs the surface
rectangle, and `panel_host`, `modal_host` and the wallpaper are screen-sized, so opening a panel
would blur the desktop. It is right for a bar and wrong elsewhere.

1. **Per node, opt-in, and never inferred.** The first design inferred surface regions from
   `background` alpha under 1. `dev-config` sets `background = "#00000000"` on eight invisible
   controls, while border-only or image-backed glass has no background alpha. A node that wants blur
   says so.
2. **`blur` is a `BOX_PROPERTIES` name, so the four surface roles accept it too.** A root that paints
   its own box can ask, which the earlier surface-level design would have made a second mechanism.
3. **The region is where the node is *painted*, not where `overlay_input_regions` puts it.** That
   walk composes no ancestor transforms (`painted_bounds`) and carries no ancestor clip. This walk
   composes the matrix and intersects the clip, mirroring `layout::paint::build_node`, which clips
   each child to its parent's box. A translated notification must blur where it paints; a history
   card outside a `max_height` list must not blur. The result is exact for these translations;
   rotated
   or scaled nodes contribute their bounding box.
4. **A claiming node does not stop the walk.** The input walk stops at a claimed box because it asks
   whether a point hits something. Blur asks for every painted contribution, so a marked child
   inside
   a marked parent unions. A rounded parent that does not clip can have children painting outside
   its
   corners.
5. **`wl_region` has no radius, so a rounded box is sent as strips.** The middle is one rectangle;
   the corner bands use merged equal-inset rows, about `radius` rectangles rather than one per row.
   A 620x260 card at `radius.md` measured 27 wire rects versus roughly 600 for scanline
   rasterisation.
6. **This one diffs, and `apply_input_region` still does not.** One `wl_region` round trip is cheaper
   than the GPU repaint that follows input changes. With 27 rects per card and unchanged regions on
   every fade frame, blur does not resend an unchanged region: one `set_blur_region` versus four
   `set_input_region` calls over the same startup.
7. **Absent support is silence, not an error.** No manager, or a capability the compositor withdraws,
   means nothing is pushed and the config hears nothing. The capability is tracked live because the
   protocol permits the bit to go away with existing regions.
8. **The effect object is lazy and dies with its `wl_surface`.** Most surfaces never set `blur`.
   `unmap` drops the object and clears the last region; the next map builds and pushes a fresh pair.

- **The object outlived its `wl_surface`.** A tooltip is destroyed and recreated on every hover;
  `drop_popup_object` keeps the `TrackedSurface` and swaps the surface under it, while `unmap`,
  which does drop the effect, returns early for anything that is not a panel. `set_blur_region` on
  the inert object is a protocol error: the compositor killed the client, the renderer panicked,
  and three generations died inside 60s. Store the `wl_surface`'s `ObjectId` beside the effect and
  rebuild the pair before comparing regions, since a same-size reopen would otherwise keep it dead.
- **The clip was intersected in the wrong space.** `layout::paint::build_node` intersects ancestor
  boxes untransformed, then applies one composed canvas matrix. Intersecting transformed boxes drops
  a child that its parent's translate carries back into view.
- **Rounding was applied after clipping.** A half-scrolled card's straight cut was rounded too,
  pulling blur off its visible straight sides. Round the node's box, then cut.
- **A box as small as its own rounding lost everything.** An unconditional middle strip emitted an
  empty rectangle at twice the radius; sampling the corner at each row's outer edge made a 2x2 at
  radius 1 emit nothing. Sample at the row's centre.
- **A changed region needs a commit.** It lands on the next `wl_surface.commit`, while
  `paint_surface` skips draw and commit for an unchanged display list. A lone `blur` flip would stay
  pending; marking the surface `stale` costs one repaint.

Not built: a per-node opt-out. Suppressing a child cannot remove blur its parent requested, which is
hole-punching and a different feature; `dev-config` has no translucent box without blur. Also not
built: blur *parameters*. Strength, passes and xray live in compositor configuration, and the
protocol carries none, so `blur` is a boolean.

Also not built, and both are edge cases with no consumer: a descendant of a `clip = "Rounded"`
parent inherits only the rectangular clip, so a square child inside a pill would blur masked corners;
and regions are not re-derived when the blur capability returns, beyond clearing the record so the next
resolve pushes again.

Verified on the wire against niri: `capabilities(1)`, one `get_background_effect`, one
`set_blur_region`, and 27 `wl_region.add` calls, with the middle band first
(`add(200, 280, 620, 220)`) and corner strips inset symmetrically. The lifetime fault was pinned by
27 tooltip cycles with no protocol error; 14 had killed the previous build.

## 0196. The name is Obelisk, in the record as well as the code

"Oblisk" was a misspelling of the object this is named after, carried from the first commit through
1,303 occurrences in 200 files. All of them moved together: the Lua namespace is `obelisk.*`, the
binaries are `obelisk` and `obelisk-renderer`, config is `~/.config/obelisk`, the socket is
`$XDG_RUNTIME_DIR/obelisk-shell.sock`, the bus name `org.obelisk.Supervisor`, the environment prefix
`OBELISK_`, and the shader entry points `obelisk_effect`, `obelisk_opacity`, `obelisk_from` and
`obelisk_to`. A three-case replacement was enough and could not overreach, because "oblisk" is not a
substring of any other word: every occurrence in the tree was this project's own name.

Now, because the Lua namespace and the shader uniforms are config-facing contracts with nobody yet on
the other side: nothing is pushed to origin, no machine has an installed copy, and 0.1.0 does not
move, since versioning starts at the first push.

The 195 earlier entries were swept too, against this file's rule that an entry is dated evidence and
is not edited to match what shipped later. That rule protects the substance of a decision, and no
entry here decided how to spell the project; leaving them citing a name that no longer exists would
preserve a typo and cost each reader a moment deciding whether "Oblisk" was something else.

Rejected: keeping "Oblisk" as a stylized name. Nothing chose it.

Rejected: an `oblisk` alias in the Lua namespace, to spare configs that do not exist.

## 0197. `obelisk call` runs a config's own verbs, because a keybind could only write state

`obelisk set`/`toggle` was the only frame a control client had. It reseeds a declared `state` and
marks the scene dirty, and nothing hangs a config callback off that write; rendering may not have
side effects. So a keybind could change what the shell *drew* and never what it *did*, and
`lib/screen_recording.lua`'s `start`/`stop` were reachable from a mouse and nothing else.

`action(name, fn)` declares a verb and `obelisk call <name> [args]` runs it, waits, and prints what
it returned. `name` is one opaque string: `rec.toggle` groups for a reader the way a module path
does, and nothing splits on the dot, so no delimiter rule can surprise a config. Arguments and the
return value cross as JSON, marshalled by `capability::invoke`'s conversion in both directions.

The shape is this codebase's own outbound verb, `obelisk.<cap>:invoke(action, ...)`, not the QML
predecessor's `IpcHandler`. `CommandParams` itself is not reused: its `generation_id` and
`expected_revision` describe the Renderer's view of a capability, and an external caller has neither.
A target is a namespace in the key and nothing more -- no `rec` object is created, because config
exports are not the Supervisor's capability roster.

Four frame variants and two payloads. The Supervisor assigns the id, never the client: every control
peer is `CONTROL_CLIENT_GENERATION`, so the id is the only thing that tells two waiting callers
apart, and believing a client's own would let one collect another's answer. It records which
generation was asked and refuses an answer from any other, which a swap mid-call can otherwise
produce. Pending calls are capped and dropped with their connection.

Registrations last one evaluation, cleared beside `on_change` handlers (ADR-0115) and for the same
reason: they are closures over locals that the next evaluation replaces. A failed evaluation leaves
them cleared rather than half-registered.

Returning nothing and returning `nil` are one answer, because Lua cannot tell them apart. A raise or
an unmarshallable return is a failure and the caller's non-zero exit; a returned `{ error = ... }`
table is a table, or the two could never be distinguished. A handler runs under the same CPU budget
as an `on_change` handler.

Rejected: a callback on external `state` writes. Two files instead of eight, but its only use is
side effects, and a side effect asked for from outside is a call -- a second way to do this, kept
forever once configs adopted it.

Rejected: a `state` holding a token a keybind flips to mean "do it". A command protocol inside a
signal whose value means nothing, and scripts would come to depend on the flip.

Rejected: a fifo read by a long-lived `process.run`, which worked already. A second control channel
beside the socket built for this, untyped, with a shell loop reaped and respawned on every save.

Not built: any boundary beyond the socket's. `$XDG_RUNTIME_DIR` is `0700`, so a caller is already
this user, and a process of this user can run what it likes without asking the shell. An action is
reachable by anything that can reach the socket, which is what `set`/`toggle` already were.

What a synchronous answer can say is bounded: `rec.toggle` returns `starting` or `cancelled`, naming
what the press did, not whether a capture later succeeded. It is read before the `slurp` exit
callback runs, so the state itself would report `starting` for a press that just cancelled.

An answer is capped well under the 16MiB frame limit. A frame that cannot be written is a dead
socket to the writer, so an oversized return would cost the Renderer's whole connection rather than
its own call.

Known, and not fixed: a config reload while a region selection is open loses the `slurp` handle,
because an in-place reload re-requires the module. The cancel survives, so the capture the user
cancelled cannot start; the overlay stays up until Escape, and `starting` clears when its callback
finally runs. Holding the handle somewhere an evaluation cannot reset means storing a userdata
outside `state`, which is machinery for a window that needs a config save mid-selection to open.

Verified against a live niri session: `obelisk call no.such.thing` exits 1 naming it, a first
`rec.toggle` prints `starting` with `slurp` on screen, and a second prints `cancelled` with the
child gone, repeatably in both directions.

## 0198. The instruction hook is installed for the life of the VM, not around each budget

Installing a Lua hook does not retrofit coroutines that already exist: a new thread inherits its
creator's hook, and a creator running while nothing was budgeted has none to pass on. `shell.lua`'s
own top level is exactly that moment, so

    spin = coroutine.wrap(function() while true do end end)

stored there and resumed from any getter ran with nothing to stop it. Reproduced: the call never
returned. That is the render thread, so it is Wayland. `StdLib::COROUTINE` is granted to configs.

The refcount that decided when to remove the hook is what opened those windows, so it is gone and
`install_hook` runs once in `signal::register`, before any config code. `HookHolders`,
`acquire_hook` and `release_hook` go with it, and `CpuBudget`/`LayoutPassBudget` now only push and
pop deadlines. This reverses the "install on 0->1, remove on 1->0" half of ADR-0022 decision 2,
which was right about reentrancy and wrong about coverage.

The cost is the callback every 1000 instructions with no budget live, where `expired_budget` finds
no deadline stack and returns on its first lookup.

Not fixed, and now precise: a config that catches the hook's error and keeps spinning still has no
bound, because the gates are cooperative. `obelisk call` (ADR-0197) made that reachable from any
process of this user rather than only from a click, which is what prompted looking.

## 0199. A shell with no terminal writes its log to a file, because the compositor that started it kept nothing

`spawn-at-startup "obelisk"` gives the shell `/dev/null` for stdout and stderr, so a session's
diagnostics were gone before anyone could ask for them. Checked on the running shell: both
`/proc/<pid>/fd/1` and `fd/2` pointed there. A service unit would have handed them to the journal,
but the shell is started from the compositor's config on purpose (`justfile`'s `install` recipe
says so).

Every diagnostic in both binaries is an `eprintln!`. So `log::capture` is two `dup2` calls onto
`$XDG_RUNTIME_DIR/obelisk-shell.log` rather than a logging crate, a level filter or a second
format: the Renderer inherits the descriptors through `spawn_group_leader`, and a panic message
lands in the file because nothing of ours sits between the process and the write. `obelisk log`
prints the file and `-f` follows it, after `quickshell log -f`.

Three things the shape settles:

- **Only `/dev/null` is taken over, per descriptor.** `capture` reads `/proc/self/fd/1` and `fd/2`
  and replaces only the ones naming it. Two earlier versions were wrong: an `is_terminal` test
  swallowed a redirect and a pipe, and testing stderr alone while replacing both left
  `obelisk >mine.log 2>/dev/null` writing an empty `mine.log`. Teeing rather than replacing needs a
  pump thread, and that thread dies with an aborting process still holding the panic message it was
  about to write.
- **Truncated per run, and no rotation.** Per-login state beside the control socket; the run worth
  reading is the current one. `ponytail:` a runaway `eprintln!` loop fills `$XDG_RUNTIME_DIR`, and
  the ceiling is sharper than a large file. `eprintln!` panics when the write fails and release is
  `panic = "abort"`, so the shell dies and everything else on that tmpfs loses the space. Taken
  anyway: bounding it needs a size check in the write path, and the only shapes that survive an
  aborting producer are the rejected pump thread or a separate collector process. That is a second
  process to bound a loop that is already a bug. It is a new failure though, not a worsened one:
  the same loop used to write to `/dev/null` for free.
- **An OFD lock says who is writing.** `--follow` stops when the writer's lock frees, and a crash
  frees it as readily as an exit. `F_OFD_SETLK`/`F_OFD_GETLK` rather than `flock` for the one
  property `flock` lacks: the reader can ask without taking. A probe that locks what it tests is
  itself a writer while it holds it, and a shell starting inside a reader's 200ms poll would find
  the log owned and spend its whole run on `/dev/null`. The same lock stops a second shell blanking
  a running one's log, which is why the open does not truncate and `set_len(0)` follows the lock.
  It frees when the last descriptor on that description closes, Renderer copies included, so a
  lingering child keeps a follow alive.

quickshell keeps a second, structured `log.qslog` beside the plain one so `-r` can re-filter a
finished run at read time. Not copied: there are no log levels here to filter by.

## 0200. The active route index is re-read from `info`, because PipeWire never pushes one that appears late

`obelisk` starts from `spawn-at-startup`, before the ALSA card has settled. `bind_device` bound
device 43 and called `subscribe_params(&[Route])` while the card still had no `Route`, and PipeWire
answered with nothing. The profile landing a moment later emits `info` with `PARAMS` changed and no
`param` event, so `device_routes` stayed empty for the session and `write_device_route` dropped
every hardware `set_volume`. Clicking the volume widget did nothing, bar and audio panel alike, on
every login since the feature landed.

Found in the log ADR-0199 added, on the first boot that had one:

    audio: sink 50 routes through device 43 port 7, whose active Route index has not been seen

Reproduced without rebooting, which is also the check:

    pactl set-card-profile 43 off      # no routes exist
    <start the shell>                  # binds, subscribes to nothing
    pactl set-card-profile 43 output:analog-stereo+input:analog-stereo

So `Route` is enumerated from the device's own `info` handler on every `PARAMS` change and
`subscribe_params` is gone. A bind always answers with one `info`, so startup still enumerates
once, and later ones cover what a subscription would not: a profile switch, a plugged headset, a
card that settles after login. The proxy is an `Rc` for that handler to reach, held `Weak` there
because a strong one is a cycle through the listener the device owns.

No unit test: the behaviour is the daemon's, and tests here do not touch a shared daemon. The recipe
above is the check; `extract_route_target` already covers the parsing half.

`bind_device_node` subscribes to `Props` the same way and is left alone, because a node is created
with its `Props`. If a sink ever reads zero volume at login, suspect this first.

## 0201. `fuzzy` is fzf's scorer in the engine, with the finder left in config

1. One global, `fuzzy(haystack, needle) -> score?, start?`. Not a capability: it is read inside
`computed`s, which must be pure and synchronous (ADR-0021).

2. The scorer only. `createFinder`, `find` and `sortResults` are iterate/sort/tiebreak/cap, which
`launcher.lua`'s `filter` already was. `start` is returned because the mirror tiebreaks on it; match
positions for highlighting are not, because the mirror computes none either.

3. Ported from the mirror's `Services/Utils/Fzf.qml` -- BSD-3-Clause, copyright 2021 Ajit -- with
every constant unchanged, rather than taking a matcher crate. `LauncherService.route` decides the
web row on `maxAppScore < Math.max(32, q.length * 25)`; a different score scale turns that copied
threshold into a number to re-tune by feel.

4. Rust rather than config Lua: O(needle x haystack) per candidate on every keystroke is ~30k inner
steps over 300 entries, 1-3ms interpreted against tens of microseconds compiled, out of 5ms.

5. Non-ASCII keeps the mirror's separate greedy scorer, ceiling and all. Its numbers do not line up
with the DP's, so a list mixing alphabets orders the two groups by slightly different rules.

Rejected: porting the 350-line JS into `dev-config`, which spends the graph budget to own more code
than the engine version. Also rejected: keeping the five hand-rolled tiers, which agreed with fzf
everywhere except initials -- "vsc" scored "Visual Studio Code" in the same bucket as every other
name holding v, s and c in order, then preferred the shortest.

## 0202. `obelisk.system.monotonic` is published beside `time`, because a countdown was reading a clock the user can move

1. Publish monotonic seconds beside `time`, from an `Instant` taken when the `system` capability is
   first started. The epoch is arbitrary; only a difference is defined. Sample the elapsed clock each
   tick rather than counting ticks, and gate the push on the whole state rather than the wall second:
   gating on `time` stalls `monotonic` for as long as a repeating clock correction resamples one
   second, and a countdown armed from a stalled reading fires the moment it unsticks.
2. Use it for idle timing, the power countdown, and the media position's elapsed term.
3. Keep wall time for persisted timestamps and capability-owned stamps. Weather freshness, the
   updates "last checked" line and notification times are anchored to stamps that outlive the
   session, and monotonic would break them at the first restart.
4. Exclude suspend with `CLOCK_MONOTONIC`; expose `CLOCK_BOOTTIME` separately if anything needs it.

Amendment: the retry deadlines and the recording clock moved too. Weather and currency each hold
two deadlines in one handler, a session-owned retry and a freshness check against a stamp on disk, so
those now read `monotonic` and `time` respectively rather than sharing one. Recording stamps its own
start instead of reading `recorder.started_at`, because every term in its elapsed calculation is a
duration and a capability's stamp could not join them: a monotonic reading only compares against
another from the same origin, and each capability would own a different one.

Still on wall time: the update panel's "took N min" line. Its end is `install_finished_at`, the
capability's stamp, so moving it would mean config observing the end and stamping it again -- another
state and an edge detector for a line that is wrong only if the clock is set mid-install.

Deferred: a timer API owning every deadline. Displayed elapsed durations still need a clock to
subtract.

## 0203. `timer(ms, fn)` is a list of its own, because a callback cannot be scheduled the way `delay` is

1. One-shot only, returning a handle with `cancel()`. A repeating flavour would have to choose
   between fixed-delay and fixed-rate and then answer what a missed deadline means; only the config
   knows, and re-arming expresses either.
2. Its own sorted list, not the `WakeDeadline` slot `delay`/`pulse` share. That slot works because
   those are pull-based: a due wake dirties the scene and the pass re-reads every clock signal it
   reaches, so nothing needs identity. A callback has to run whether or not any node reads anything,
   and ADR-0124 never resolves a hidden subtree, so a pull-based timer behind one would never fire.
3. Cancelling stays effective inside a due batch, including a timer cancelling itself. Taking every
   callback up front, as `notify_change` takes its handler list, would run one the previous callback
   had just cancelled. The batch is drained out of the armed list in one move and each callback
   taken from it by index, so dispatch is linear rather than a search-and-shift per timer.
4. A timer armed by a callback waits for a later turn.
5. Dispatch precedes the turn's re-resolve. It resolves the applied tree, so a reload still waiting
   on `ApplyPendingReload` paints its new bindings when that lands, not from here.
6. Cleared with `action`'s registrations, before an evaluation and after a failed one (ADR-0115): a
   callback held past a reload closes over the previous evaluation's locals. Ids keep counting across
   a clear, so a handle from before it cancels nothing armed after it.
7. `[1, 86400000]` ms, not `delay`/`pulse`'s 60-second ceiling, which could not express
   `lib/idle.lua`'s two-hour suspend stage.

8. A registration is staged until its evaluation's output is applied, then promoted; discarded if
   that output is refused or superseded. Without it a topology change leaves the outgoing process
   running the incoming config's timers beside the candidate's, and both fire. Timers alone get this
   and `action`/`on_change` keep ADR-0115's overlap, because only a timer fires without an external
   trigger: an action waits for `obelisk call`, and a handler waits for a push both generations get
   anyway. Deadlines are absolute, so staging costs a promoted timer no accuracy, and a callback
   arming a timer is not an evaluation, so it goes live at once.

Accepted: the budget is per callback, as `action` and `on_change` are, so a batch spends one per
timer.

Rejected: a heap with cancellation bookkeeping; the expected workload does not justify it.

## 0204. A `Background` surface claims input only where a handler sits, because paint stands in for occlusion and nothing is behind the bottom layer

ADR-0038 decision 5 derives a surface's input region from what it draws: a visible node claims its
box when it paints, or when it is a `button` carrying a pointer handler. ADR-0195 restated the reason
as "what can be clicked has one right answer and so needs no config input". The wallpaper is the
counterexample.

`modules/global/wallpaper.lua` paints an opaque ground and a full-surface `image`, and carries no
handler and no `hover`, so the walk handed the compositor `0,0 3440x1440`. Measured on Hyprland
0.56.2: toggling a special workspace off over an empty workspace left the hidden scratchpad's window
focused, so the bar's active-window widget kept naming it and `killactive` still killed it.
`CInputManager::mouseMoveUnified` walks Overlay, Top, windows, then Bottom and Background; with no
window on the workspace it reached the wallpaper, and a found surface that is not keyboard-focusable
is no reason to clear window focus -- `refocusLastWindow` re-asserts the last window outright.
Twenty toggles: stale focus every time with the shell up, none with it stopped, none with the
wallpaper's region emptied.

Paint is not an aesthetic here, it is a stand-in for occlusion: a drawn card must not leak a click to
the window behind it. Nothing is behind `Background`, so there the stand-in claims a whole output for
a surface that cannot use it, and takes the desktop's focus-through with it.

1. **The carve-out is by layer, not by surface and not by config.** `Background` drops the paint half
   of the solid test and keeps the handler half. Every other layer is unchanged, `Bottom` included,
   which has something under it.
2. **A role with no `layer` keeps the proxy.** A window, popup or lock parses no layer, so the walk
   reads `parse_layer` and treats anything but `Ok(Background)` as occluding.
3. **A clickable wallpaper stays expressible.** A `button` with `on_click` claims its box on
   `Background` exactly as it does anywhere else.

Rejected: dropping the paint proxy on every layer, so that only interactivity ever claims. It is the
more uniform rule, and removing the paint/input coupling is the better long-term model, but as
written it is wrong twice over. `hover` is not a pointer handler, and `dev-config`'s battery,
bluetooth, network, updates, screen-recorder, idle-inhibitor, launcher and wallpaper indicators are
`rect`s carrying `hover` and no `on_click`: they receive pointer events today only because they
paint, so the rule would silently retire eight tooltips. Counting `hover` as interactivity repairs
that and does fix the wallpaper, which leaves one real trade -- the engine would stop guaranteeing
that a drawn overlay is opaque to input, and a future painted surface with neither handler nor hover
would leak clicks and focus-follows-mouse focus to whatever sits behind it. That failure is silent
and lands a stray click in another window, which is the shape of bug this ADR exists because of. Do
it as its own change with a one-time audit of every painted surface, not folded into a focus fix;
`tooltip` and `osd` are the two surfaces it would newly make click-through.

Rejected: a surface property such as `input = "None"`. The wallpaper would be its only writer, and it
moves a fact the engine can derive into something every future inert surface has to remember.

## 0205. Hyprland's `main` keyboard is the one being typed on, which closes ADR-0034.2's cycling gap

ADR-0034.2 decision 4 recorded that Hyprland "lacks reliable name-to-code correlation" and left
`active_layout_index` pinned to `0`, a read-back nothing could cycle from. Both halves are stale
against 0.56.2, verified live rather than from docs:

1. `j/devices`'s `keyboards` entries carry `active_layout_index`. Read it; no name-to-code table is
   needed. It stays `#[serde(default)]` so an older Hyprland reports the previous `0`.
2. The `main` field serializes `IKeyboard::m_active`, which `CInputManager::onKeyboardKey`
   reassigns on every key event. ADR-0034.2 decision 3's "select one keyboard" mechanism stands;
   what it got wrong is that the one keyboard is fixed. It tracks whoever typed last, which is why
   a physical `grp:alt_shift_toggle` moves exactly that device and the read follows it.
3. `switchxkblayout` accepts `main` as a device target, resolved against the same `m_active`. Aim at
   the word, not a tracked device name; a click before the first read is no longer dropped.
4. Read `j/devices` over `.socket.sock` rather than spawning `hyprctl`, per ADR-0118 decision 2, and
   read the write's reply. An out-of-range index answers `layout idx out of range of N` and was
   silent. `hyprland_request` and `hyprland_command` join `hyprland_socket_path` in `compositor`
   (ADR-0118 decision 5).
5. Signal the initial read, and take it whether or not the event socket connects. It applied
   silently before, so `layout_count` stayed `0` in the first snapshot and an indicator drawn only
   for two or more layouts stayed hidden until the first switch.

The resync blocks on the reader thread, so the ticket sequence that ordered concurrently spawned
`hyprctl` calls is deleted rather than ported. That thread also owns the connect: it blocks, and an
`async fn` on a two-worker runtime builds this.

Rejected: targeting `switchxkblayout all`. Layout is per-device, and with `main` tracking the active
keyboard there is nothing to reconcile; `all` would yank layout on keyboards the user did not touch.

Rejected: following the device named in `activelayout>>DEVICE,KEYMAP` instead of `main`. Measured on
an eight-keyboard session, every physical toggle named the `main` device, so the two agree and the
event payload buys only a second source of truth.

Known limitation: `m_active` moves for any keyboard-class device, and that session's eight include
media, system-control and power-button nodes. A key on one of those makes it `main`, so both the read
and the write follow a device nothing is typed on until the next real keystroke moves it back.
Narrowing the set means guessing which nodes type, against Hyprland's own answer. Left alone until a
session reports a wrong layout.

## 0206. Session verbs branch in config, and only the two the compositor owns

`power_menu` ran `niri msg action quit` and the idle stage ran `power-on/off-monitors`, so on
Hyprland logout and the whole blank stage did nothing.

1. Branch in Lua, in `lib/compositor.lua`, on `workspaces.compositor`. ADR-0119 decision 3 publishes
   that name so config can choose policy, and ADR-0056 decision 1 refused a compositor trait in the
   Supervisor. A session verb is not a capability, so neither grows for this.
2. Only `logout` and display power branch. Reboot, poweroff and suspend are logind's and identical
   under both compositors; giving them entries would imply a difference that does not exist.
3. Hyprland's spellings are `hl.dsp.exit()` and `hl.dsp.dpms({ action = "on"|"off" })`. 0.56 parses
   the command socket as Lua, so the pre-0.56 `dispatch exit` dies in that parser. `dpms` toggles
   when passed no table, so the field is always explicit.
4. `detach` returns whether anything ran, and the caller may not record the verb as done on `false`.
   `idle.blanked` is the `dpms` stage's `done` predicate. Setting it on a no-op arms lock and then
   suspend over a lit screen, and `set_displays_powered`'s equality guard then refuses every retry.
   Found in review, not in use.

ADR-0118 decision 2's "command sockets, not subprocesses" binds the Supervisor, which holds the
socket path and a connection budget. Config has neither and already shells out for `systemctl`, so
`hyprctl` and `niri` here are subprocesses on purpose.

Rejected: a `session` capability wrapping these in the Supervisor. It moves one compositor check
from Lua to Rust and costs a second place that decides compositor identity, plus a capability whose
whole job is running two commands.

## 0207. A Renderer file splits by concern when its tests can move with the code

ADR-0076's follow-up kept Renderer files whole. `wayland/input.rs` then held 1,557 production and
913 test lines for two jobs that share only `App`: pointer hit-testing, clicks, drags, wheel, hover
and cursor, and keyboard focus with the `secure_submit` buffer.

1. Split a file into a folder of concern files when its tests already call those concerns
   separately, so every test lands beside the code it covers (AGENTS.md: tests live beside the
   code). `wayland/input.rs` becomes `input/mod.rs` for the seat, `pointer.rs` and `keyboard.rs`.
2. Split commits move code only, plus imports and the visibility the extra depth forces. An item
   that was `pub(super)` one level up becomes `pub(in crate::wayland)`, keeping the reach it had.
3. `layout/scene.rs` stays one file. 171 of its tests reach it through `apply_at`, the whole
   prepare, solve and finish pass, so splitting the code would leave about 3,900 test lines in
   `scene/mod.rs` beside none of the code they test.

Rejected: splitting by size alone. A file whose tests drive only the whole pipeline gains file names
and loses the one place its tests and its code meet.

## 0208. No Rust test or comment depends on `dev-config`, because the crates are the framework and `dev-config` is one shell built on it

ADR-0155 removed six component tests but kept a `require` test and the Renderer still carried six
tests that loaded `dev-config/obelisk` and asserted its surface list, bar zones, history card and
lock screen. A restyle of that shell failed the engine's build.

1. Engine tests use inline fixtures. The seven loading tests are deleted: `check.rs` and
   `lua/mod.rs` already cover nested `require` with tempdir fixtures, `layout/paint.rs` covers the
   mask glyphs, and `layout/scene.rs` covers the taffy margin workaround.
2. Comments state engine reasons. A config module, theme token or surface id from `dev-config` is
   not a rationale for engine behaviour.

Rejected: keeping the loads as smoke tests. `just check` already type-checks `dev-config` against
`lua-meta`, and whether that shell's layout fits belongs to running it.

## 0209. A debug build boots `dev-config` over every configured directory but `-c`

Debug order: `-c`, `dev-config/obelisk`, `$OBELISK_CONFIG_DIR`, `$XDG_CONFIG_HOME/obelisk`, then
`$HOME/.config/obelisk`; release drops `dev-config`. `-c` sets its own `OBELISK_CONFIG_ARG`, since
sharing `$OBELISK_CONFIG_DIR` could not tell it from the session's variable. This is ADR-0208's one
exception, and release builds never see the path.

## 0210. A missing lua-language-server fails the gate instead of skipping it

`just types` and `tools/luafmt.py` exit non-zero with an install hint when no lua-language-server is
on PATH or in Zed's extensions, as `lua` already does without `luac`. A printed skip exited 0, so
`just check` and the pre-commit hook passed having checked no stub.

Reject keeping the skip for want of CI: with no CI, `just check` is the only gate.

## 0211. Paint draws cosmic-text's glyphs, and text alignment follows each line's reading direction

`text::atlas::TextPainter` draws the glyphs cosmic-text placed, through femtovg 0.27's public
`fill_glyph_run`, instead of handing femtovg the string to shape again with `fill_text`. femtovg
still rasterizes and packs its own atlas.

Two shapers disagreed on Arabic notifications. femtovg re-derived each drawn line's direction from
its first strong letter, dropped a letter from CaskaydiaCove words whose final letter is two glyphs
in one cluster (`خامس` drew as `امس`), and put a space on the wrong side of a direction change.
cosmic-text's layout of the same strings was correct.

1. `ShapeResult` carries a `ShapedLine` per line: its direction, baseline and glyphs, each naming
   the `fontdb::ID` it was shaped in. femtovg registers every face in the database under that id,
   mapping no new file; a face it refuses draws nothing. The painter's per-variant and
   per-named-family chains (ADR-0104, ADR-0144) are deleted.
2. Paint and `hit::link_under` shape each line alone through `ShapingHandle::shape_lines`. Lines
   end where cosmic-text's `LineIter` ends them (`\r` and `\n\r` too), so paint draws the rows
   measured.
3. `Start` and `End` follow the line's reading direction, as CSS `start`/`end` and Qt's unset
   alignment do: a right-to-left line starts at the right. `Center` is unchanged.
4. A wrapped line shaped alone can disagree with its paragraph's direction. `wrapped_to_fit`
   prefixes U+200F or U+200E when it does; cosmic-text gives the mark no advance.
5. Only paint and link hit-testing keep glyphs (`ShapingHandle::shape_glyphs`), so measurements and
   elide probes do not fill the shaping memo with them.
6. A glyph carries the weight cosmic-text shaped it at, and paint hands `fill_glyph_run` the face's
   normalized `wght` coordinates for it: a variable family's bold is one face at `wght` 700, and
   with no coordinates it drew regular outlines at bold advances.

The baseline is cosmic-text's, centred in the 1.2x line, where it was
femtovg's ascender from the box top, so text moves by half the leading, and a row holding a taller
fallback glyph moves its own baseline.

Rejected: a per-script font key in `fonts {}`. CaskaydiaCove covers Arabic, so Arabic draws in it,
as it does in Quickshell under the same family.

Rejected: carrying glyphs from `Scene::apply` in `PaintStyle::Text`. A `textfield`'s content is
built at display-list time, where the shaping worker is out of reach.

## 0212. NetworkManager proxies are hand-written

`capabilities/network/proxies.rs` declares only the NetworkManager members `obelisk.network` calls,
and the enum values it matches as `u32` constants from `nm-dbus-interface.h`.
`rusty_network_manager` is removed, reversing ADR-0029 item 2.

1. The crate depended on `zbus` with default features, and cargo unions features, so the Supervisor
   carried zbus's async-io executor, `blocking` pool and `async-process` beside tokio. Without it,
   the Supervisor's `default-features = false` holds: 137 crates to 120.
2. Its build script ran bindgen over a vendored `nm-dbus-interface.h`, and `num_enum` derived
   conversions for enums whose every use here matched a few values.
3. BlueZ, UPower, logind, MPRIS and StatusNotifier proxies were already hand-written (ADR-0030).
   The crate's `Connection.Active` proxy subscribed to `state_changed`, a member NetworkManager
   never emits, so that one was hand-written too.

`Connection.Active` stays in `connect.rs`. zbus names signal types after the D-Bus member, and its
`StateChanged` would redefine `Device`'s in `proxies.rs`.

Device tracking is unchanged: `DeviceAdded` and `DeviceRemoved` drive `refresh_devices`, which never
lived in the crate.

## 0213. Every role object dies through one teardown

Amends ADR-0088 decision 1 and ADR-0195's blur-effect lifetime.

1. `App::drop_role_object` is the only path that destroys a panel, window, popup or lock object: blur
   release, child popups, EGL, role object, per-entry resets, then keyboard and pointer scrubs.
2. Roles keep only their own policy: the popup latch, lock-release ordering (ADR-0042), entry removal
   on output loss.
3. The blur effect's surface-id guard is deleted. Every `wl_surface` destroy now takes the effect with
   it, so an effect cannot name a dead surface.

Five per-role teardowns shared 1 of 10 steps. Hidden windows and popups kept `pointer_at` and hover
signals, and pinned their last images against ADR-0182.

## 0214. A button's default cursor and its input region ask one question

Amends ADR-0107 decision 2 and ADR-0109 decision 3.

1. `ResolvedNode::takes_pointer` is true for a `button` with `submit = true` or a callable `on_click`,
   `on_drag` or `on_wheel`. The input region and the default cursor both read it.
2. Drag- and wheel-only buttons show `pointer`. A config wanting `grab` sets `cursor`.
3. Press dispatch keeps its per-gesture checks: each needs its own handler, and a drag-only button
   inside a clickable one must not block the outer click.

A transparent `submit` button claimed only its painted label, so 3800 of its 4800 px² passed clicks
to the window behind, under an arrow cursor.

## 0215. A command decodes into a typed action variant

Amends ADR-0037 and ADR-0052 decision 1.

1. `parse_action` reads `(action, arguments)` as an externally tagged enum: the action names the
   variant, the arguments fill its fields by position. The wire is unchanged.
2. Checks beyond a type stay in serde: `non_empty`, `lua_list` (mlua sends an empty table as `{}`),
   absolute paths.
3. The stub generator writes one typed `invoke` overload per variant.

38 hand parsers coerced silently 3 times (0eec577). Rejected: named-argument tables, an IDL change.
