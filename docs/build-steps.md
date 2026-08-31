# Oblisk Advanced Scaffold & Scoped Engineering Roadmap (v2)
## Multi-Crate Rust Cargo Workspace Scaffolding and Research Playbook

This document specifies the exact, step-by-step scaffolding architecture and compiler playbooks to guide a coding agent from zero to a successfully compiled binary. It includes targeted **Research Milestones** with concrete technical criteria and direct links to active open-source GitHub repositories to solve complex platform boundaries.

---

## 1. Architectural Workspace Topology

The Oblisk codebase is laid out as a multi-crate cargo workspace, enforcing a hard boundary between the privileged, durable platform daemon (`supervisor`) and the unprivileged, hot-reloadable graphics UI renderer (`renderer`).

```text
oblisk-workspace/
├── Cargo.toml                      # Workspace meta-configuration
├── Cargo.lock
├── shared/                         # Serialization definitions & IPC protocols
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs                  # Guarded envelopes, JSON-RPC, snapshots
├── supervisor/                     # Durable background system daemon (zbus/pipewire)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                 # Unix socket server, core event loop, and signal router
│       ├── dbus/                   # NetworkManager, BlueZ, MPRIS, and Polkit interfaces
│       ├── hardware/               # Udev, Netlink, and Sysfs polling threads
│       ├── process/                # Safe process group spawning & PGID reaping
│       └── reload/                 # Presentation-Before-Authority orchestrator
└── renderer/                       # Ephemeral UI Renderer process (Wayland/GLES3/Lua)
    ├── Cargo.toml
    └── src/
        ├── main.rs                 # CLI entry point, MLua VM initializer, frame thread
        ├── wayland/                # SCTK client wrappers & static surface mapping
        ├── layout/                 # One-pass constraint solver & subpixel snapping
        ├── render/                 # FemtoVG path drawing & GLES3 transition shaders
        └── lua/                    # MLua userdata proxy signal bindings
```

---

## 2. Scaffolding Playbook: Phase-by-Phase Compiler Milestones

### Phase 1: Workspace Scaffolding & Cargo Dependency Tree

Establish the compilation parameters in the workspace root. Use the latest stable Rust edition and toolchain available at implementation time, not a pinned historical one; edition 2024 in particular tightens `unsafe` block requirements in ways that directly serve this project's memory-safety goals in the Wayland/EGL FFI code. Pair that with highly aggressive production profile configurations to eliminate GC-adjacent stutters in hot paths (there's no GC to stutter, but the same profile settings still remove allocator and codegen overhead).

Every crate version below is a placeholder (`"latest"`), not a real pin. Run `cargo add <crate> --features ...` or check crates.io directly at implementation time to resolve actual versions. This matters more for some crates than others: `zbus` in particular has moved through multiple major versions (3 → 4 → 5) with real breaking API changes since this spec was drafted, so check its migration notes specifically rather than assuming the API described elsewhere in these docs still matches the current major version.

#### 1. Root `Cargo.toml`
```toml
[workspace]
members = ["shared", "supervisor", "renderer"]
resolver = "2"

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

#### 2. `shared/Cargo.toml`
```toml
[package]
name = "shared"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "latest", features = ["derive"] }
serde_json = "latest"
thiserror = "latest"
```

#### 3. `supervisor/Cargo.toml`
```toml
[package]
name = "supervisor"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../shared" }
tokio = { version = "latest", features = ["full"] }
zbus = { version = "latest", features = ["tokio"] }
tokio-stream = "latest"
futures-util = "latest"
nix = { version = "latest", features = ["process", "signal"] }
regex = "latest"
pipewire = "latest"
udev = "latest"
inotify = "latest"
wayland-client = "latest"
wayland-protocols = { version = "latest", features = ["client"] }
smithay-client-toolkit = "latest"
```

`pipewire`, `udev`, and `inotify` were missing from this list despite being required by the prose elsewhere in this doc and in `oblisk-supervisor-services-dbus.md`: `pipewire` for § 6's registry stream mixer (Phase 6 below), `udev` for § 1.1's battery netlink monitor, `inotify` for § 1.2's backlight watch and the config-directory watch driving reload (ADR-0001).

The Supervisor also gets its own Wayland connection now (ADR-0010): `ext_idle_notifier_v1` (§7) and lock-screen authority both need to survive a Renderer crash or reload, which means the process holding them needs to be the Supervisor, not the Renderer. `smithay-client-toolkit`'s `session_lock` module wraps `ext_session_lock_v1`; idle-notify has no SCTK wrapper and is hand-dispatched against raw `wayland-protocols`, the same shape as ADR-0009's `TextInputService` on the Renderer side.

#### 4. `renderer/Cargo.toml`
```toml
[package]
name = "renderer"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../shared" }
tokio = { version = "latest", features = ["rt", "net", "macros"] }
mlua = { version = "latest", features = ["lua54", "vendored"] }
wayland-client = "latest"
wayland-protocols-wlr = { version = "latest", features = ["client"] }
wayland-protocols = { version = "latest", features = ["client", "unstable"] }
smithay-client-toolkit = "latest"
khronos_egl = { version = "latest", features = ["static"] }
gl = "latest"
femtovg = "latest"
cosmic-text = "latest"
```

`wayland-protocols` now also carries the `unstable` feature: `wp-text-input-v3`'s client bindings (`textfield`, ADR-0009) live behind it, and `smithay-client-toolkit` was added per ADR-0008.

---

### Phase 2: Core IPC Marshalling and Serialization Layer

Implement the strict serialization models inside `shared/src/lib.rs`. This forms our binary communication interface contract, mapped exactly to `oblisk-idl-api-specs.md` (§ 1 & § 3).

*   **CommandEnvelope**: Wraps Lua write operations with generational tracking flags.
*   **StateSnapshot**: Emitted by the Supervisor on system changes to instantly hydrate active Lua signals.

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub jsonrpc: String,
    pub method: String,
    pub params: CommandParams,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandParams {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub arguments: Vec<serde_json::Value>,
    pub expected_revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub revision: u32,
    pub payload: serde_json::Value,
}
```

---

### Phase 3: Research Milestone — High-Performance Wayland EGL Surface Setup

Before rendering any pixels on screen, the `renderer` must initialize an OpenGL ES (GLES3) rendering context bound to the Wayland layer-shell compositor protocols. This has been a frequent source of thread collisions and memory leaks for coding agents.

```text
               [ Wayland Registry Event Dispatcher ]
                                 │
                     (Bind Core Wayland Objects)
                                 ▼
               [ wl_compositor, wl_subcompositor ]
               [ zwlr_layer_shell_v1             ]
                                 │
                      (Initialize EGL Context)
                                 ▼
               [ khronos_egl::Display / Config ]
               [ khronos_egl::Context (GLES3)  ]
                                 │
                        (Create EGL Surface)
                                 ▼
               [ eglCreateWindowSurface(wl_surface) ]
```

#### Research Task & Goals:
1.  **Registry and layer-shell binding**: use `smithay-client-toolkit`'s `shell::wlr_layer` module for `zwlr_layer_shell_v1`/`zwlr_layer_surface_v1` instead of hand-dispatching the protocol against raw `wayland-client` (ADR-0008). Reserve raw `wayland-protocols` for the one thing SCTK doesn't wrap: `wp-text-input-v3` for `textfield`.
2.  **EGL Context Allocation**: Initialize EGL with `khronos_egl`, following SCTK's own EGL setup as a reference: [SCTK EGL Module](https://github.com/Smithay/client-toolkit/tree/main/src/egl). Find the optimal EGL config supporting 8-bit ARGB color formats (`EGL_SURFACE_TYPE` with `EGL_WINDOW_BIT`, `EGL_RENDERABLE_TYPE` with `EGL_OPENGL_ES3_BIT`).
3.  **Surface Context Binding**: Match the Wayland physical native window (`wl_egl_window`) to the created EGL surface and make the GLES3 rendering context current on the thread. Refer to `noctalia`'s OpenGL ES Renderer initialization logic: [Noctalia Renderer Setup](https://github.com/noctalia-dev/noctalia/tree/main/src/renderer).
4.  **Layer Constraints**: Bring up three surfaces to prove the layer-shell path end to end:
    *   `main_bar`: anchored on top, marked exclusive.
    *   `overlay_canvas`: anchored to all four edges, non-exclusive, transparent. On boot, immediately commit an empty input region (`wl_compositor::create_region` with no added coordinates) to allow background applications to receive pointer clicks.
    *   `wallpaper_layer`: `Background` layer, non-exclusive, one per monitor (ADR-0007).

    Superseded by ADR-0038: these three are a bring-up scaffold, not the surface model. They are
    hardcoded in Rust as a `SurfaceRole` enum and created before any Lua runs, which means
    `shell.lua`'s own `surface` declarations are discarded. Phase 20 deletes the enum and drives
    surface creation from the evaluated topology, at which point these three become ordinary ids in
    the default config. Do not add a fourth role here; adding the third one is what exposed the
    problem.

---

### Phase 4: Research Milestone — Cosmic-Text and FemtoVG Rendering Engine

Text rendering on a Linux status bar must be shaped, wrapped, and cached with extreme efficiency to sustain a fluid 120Hz display refresh cycle.

```text
[ Lua String / Signal ] ──▶ [ cosmic_text::Buffer ] ──▶ [ Off-Thread Swash Glyphs ]
                                                                   │
                                                           (Atlas Packaging)
                                                                   ▼
[ Physical Frame Draw ] ◀── [ FemtoVG Rasterizer ] ◀── [ Glyphs Texture Atlas ]
```

#### Research Task & Goals:
1.  **Asynchronous Shaping**: Write an asynchronous wrapper around `cosmic-text`'s font shaping database (`cosmic_text::FontSystem`, `cosmic_text::Buffer`). Ensure layout widths and font-family fallbacks are processed off-thread to prevent frame drops when rendering dynamic media titles. Refer to cosmic-text implementations: [Cosmic Text Examples](https://github.com/pop-os/cosmic-text).
2.  **Texture Atlas Management**: Map glyphs to a dynamic, size-bounded GPU texture atlas cache (2048x2048) in FemtoVG. Review iced's text graphics engine pipeline: [Iced Text Renderer](https://github.com/iced-rs/iced/tree/master/graphics) and [Noctalia Text Pipeline](https://github.com/noctalia-dev/noctalia/blob/main/src/renderer/text_renderer.cpp).
3.  **Subpixel Snapping Math**: Implement layout snapping math. Calculate text lines and box borders using fractional coordinates, snap coordinates to physical boundaries before damage rectangles are projected to the viewport, and snap borders strictly to single physical pixels to prevent anti-aliasing blur [oblisk-layout-engine-geometry.md § 5].

---

### Phase 5: Research Milestone — Polkit D-Bus Authorization Agent Handshake

A PolicyKit authentication agent must securely receive D-Bus authorization queries and process password authentication off-thread without exposing sensitive keys to the Lua VM heap.

```text
[ privileged action ] ──▶ [ org.freedesktop.PolicyKit1 ]
                                       │
                         (dbus authentication request)
                                       ▼
                             [ Oblisk Supervisor ]
                                       │
                      (push challenge metadata over IPC)
                                       ▼
                             [ Lua VM Dialog UI ]
                                       │
                      (secure password entry typed in)
                                       ▼
[ pam authorization ] ◀── [ Oblisk Secure Buffer ] (typed password)
```

#### Research Task & Goals:
1.  **Agent Registration**: Map out the exact D-Bus signature for `org.freedesktop.PolicyKit1.Authority.RegisterAgent`. Handle standard interactive challenges, authenticating local users on the active session bus. Refer to standard C++/JS implementations: [LXQt Polkit Agent Core](https://github.com/lxqt/lxqt-policykit/tree/master/src) and [Aylur's GTK Shell Polkit Service](https://github.com/Aylur/ags/tree/main/src/service/polkit.ts).
2.  **The Secure Password Input Boundary**: Ensure the `on_change` and `on_submit` callbacks for your `textfield` primitive never capture plain-text characters in Lua VM space. All typed passwords must flow directly into native, secure Rust buffers that zeroize their memory allocations on drop (`secrecy` crate or raw pointer zeroing), preventing keylogger memory extraction attacks.

---

### Phase 6: Research Milestone — PipeWire Registry Stream Mixer

The supervisor must run a zero-polling registry listener that intercept volume level properties changes on the physical ALSA sinks and maps application-specific audio streams dynamically.

```text
[ pw_registry event ] ──▶ [ Intercept Node Added / Properties Changed ]
                                           │
                         (filter by Stream/Output/Audio)
                                           ▼
                                [ Oblisk Audio Apps ]
                                           │
                         (map Process PID -> App Name)
                                           ▼
                         [ Push updated lists to Lua ]
```

#### Research Task & Goals:
1.  **PipeWire Registry Mapping**: Set up a background event thread using `libpipewire` (or raw pipewire socket dispatching). Avoid high-frequency polling commands. Refer to native Rust examples: [Pipewire-rs Examples](https://github.com/rnumr/pipewire-rs) and [AGS Audio Service](https://github.com/Aylur/ags/tree/main/src/service/audio.ts).
2.  **App Mixer Tracking**: Monitor node added and node properties changed events. Identify stream nodes (type `Stream/Output/Audio`) and dynamically map the node's process identifier (`sec.pid` or `node.client-id`) to resolve process application names, providing a dynamic list of per-app volume sliders to your Lua widgets.

---

### Phase 7: Subprocess PGID Gating & Safe Reload Orchestration

The Supervisor manages the lifecycles of processes spawned by `process.run` with absolute visual and crash safety [oblisk-supervisor-services-dbus.md § 12].

#### Process PGID Gating:
Configure all child forks to spawn within an independent Unix process group (`setsid` or `setpgid` via the `nix` crate). Ensure the command runner maps to this PGID structure:

```rust
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn spawn_pgid_child(cmd: &str, args: &[String]) -> std::io::Result<std::process::Child> {
    unsafe {
        Command::new(cmd)
            .args(args)
            .pre_exec(|| {
                // Establish an independent process group
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                Ok(())
            })
            .spawn()
    }
}
```

#### Safe Reaping Routine:
When a Renderer crash occurs or a hot-reload is triggered, execute the safe cleanup routine:
1.  Locate the active process handle.
2.  Transmit `SIGTERM` to the entire process group: `nix::sys::signal::kill(-pgid, nix::sys::signal::SIGTERM)`.
3.  Spawn a non-blocking 100ms async wait timer. If the child process group has not fully exited, escalate to `SIGKILL` to clean up active screen recorders and input overlays cleanly.

---

### Phase 8: Hot-Reload Presentation Before Authority (PBA) Flow

In `supervisor/src/reload.rs`, orchestrate the overlapping process transitions without introducing a single blank or black frame on the user's display [oblisk-supervisor-services-dbus.md § 15].

```text
Supervisor                            Renderer Gen N                     Renderer Gen N+1 (Candidate)
    │                                       │                                         │
    │── (Spawns Gen N+1) ───────────────────┼────────────────────────────────────────▶│
    │                                       │                                         │ (Binds Wayland / null-buffers)
    │                                       │                                         │ (Stages assets in background)
    │                                       │                                         │
    │◀─ (Ready Event) ──────────────────────┼─────────────────────────────────────────│
    │                                       │                                         │
    │── (ActivateDraw Nonce) ───────────────┼────────────────────────────────────────▶│
    │                                       │                                         │ (Commits GLES3 frames)
    │                                       │                                         │ (Attaches wp_presentation_feedback)
    │                                       │                                         │
    │◀─ (Presented Callback Verified) ──────┼─────────────────────────────────────────│
    │                                       │                                         │
    │── (Clear Input Region) ──────────────▶│                                         │
    │                                       │                                         │
    │── (SIGTERM Group) ───────────────────▶│                                         │
    │                                       │                                         │
    │── (Promote to Active Focus) ──────────┼────────────────────────────────────────▶│
```

1.  **Overlapping Spawn**: Spawn the Candidate `N+1` while keeping the current active generation `N` rendering and receiving input.
2.  **State Hydration**: Immediately write the pre-cached values of NetworkManager, BlueZ, and PipeWire state snapshots down to the Candidate’s control socket, eliminating any startup state-query latency.
3.  **Null-Buffer Staging**: The Candidate completes Wayland Layer-Shell handshakes but commits null graphics buffers, remaining completely invisible.
4.  **Activate Draw**: The Supervisor writes an `ActivateDraw` nonce packet. The Candidate compiles the AST, draws its first layout frame on the GPU, and attaches a `wp_presentation_feedback` request to its commit.
5.  **Evidence Verification**: Once the compositor triggers the `presented` callback, confirming pixels have physically updated on all outputs, the Candidate writes back presentation evidence.
6.  **Swap & Reap**: The Supervisor commands Generation `N` to clear its input region. It marks Generation `N+1` as active, maps its input regions, and reaps the Generation `N` process group cleanly.

---

## 3. Playbook Testing & Validation Protocols

Command your agent to execute this automated shell script workflow to verify structural type checks, socket communication, and AST config parser health:

```bash
#!/usr/bin/env bash
set -euo pipefail

echo "==================================================================="
echo "Oblisk Automated Compiler Testing Playbook"
echo "==================================================================="

# 1. Structural Crate Compile and Workspace Bounds Check
echo "Step 1: Running Cargo Check..."
cargo check --workspace --release

# 2. Emulate Supervisor-Renderer IPC socket handshakes
echo "Step 2: Executing shared IPC serialization tests..."
cargo test -p shared --all-features

# 3. There is no config dry run, and there never was. See section 6, "Found while writing this":
#    the Renderer parses no command-line arguments, so `--validate` is ignored in full and this
#    line starts a live shell over the running session instead of checking a config.
#    Left in place, corrected, because the flag it names is still worth building.
#
#    cargo run -p renderer -- --validate ~/.config/oblisk/shell.lua   # DOES NOT VALIDATE

echo "Success: Scaffolding has compiled with 100% type-safety!"
```


## 4. Continuation Playbook: Phase 9 Onward

Phases 1-8 scaffolded the workspace and built isolated primitives in research-milestone phases.
None of them are wired together yet. `supervisor` and `renderer` share no transport, no Lua VM
runs anywhere, and the process-group and PBA-orchestration primitives Phase 7 and Phase 8 already
built (`process::spawn_group_leader`/`reap_process_group`, `reload::run_pba`) have no caller. The
phases below close that gap and reach what the original scope never covered: the scene graph, the
reload watcher, and the D-Bus/hardware backend controllers.

Terminology follows `CONTEXT.md`: loader, watcher, dependency snapshot, retained scene, lease,
rollback. Read it first if a term here is unfamiliar. Every phase reuses an already-shipped
primitive where one exists instead of re-deriving it, and every phase lists what it deliberately
leaves for the next one. Follow the same scope-ceiling discipline as ADR-0015, ADR-0017, ADR-0018,
and ADR-0019, rather than guessing ahead of a phase's own real caller.

### Phase 9: IPC Control-Socket Transport & Wire Framing

Build the real Unix-socket transport `CandidateLink` (ADR-0019) and the Supervisor's write-path
dispatch (`oblisk-idl-api-specs.md` § 3.1) both need. The Supervisor binds and listens at
`$XDG_RUNTIME_DIR/oblisk-shell.sock`, not `/tmp`, which is world-writable and unsuitable for a
socket that will eventually carry secure textfield submissions (ADR-0005). Frame every message
with a 4-byte big-endian length prefix ahead of a JSON payload, matching `CommandEnvelope`/
`StateSnapshot`'s existing shape (`shared/src/lib.rs`, Phase 2). Add the framing reader/writer and
its `thiserror`-based error enum to `shared`, finally giving that crate's already-declared, unused
`thiserror` dependency a real job. Use each side's native async `AsyncReadExt`/`AsyncWriteExt`
directly. Do not convert a `tokio::net::UnixStream` to a blocking `std::net` socket to reuse a
synchronous reader, and do not busy-poll with a short sleep. `read_exact`/`write_all` on the async
stream already suspends correctly.

The Supervisor's listener must accept more than one live connection at once. During a generation
swap, Generation `N` and Candidate `N+1` are both connected simultaneously (`CONTEXT.md`,
Concurrent Overlapping Lifetimes). Each connection identifies its generation with a handshake
message before any other traffic, so the Supervisor can address commands and pushes to the right
generation instead of assuming exactly one peer.

Deliberately deferred: the command-dispatch routing table itself (§ 3.2's ~30 write commands).
This phase builds transport and connection identity only, not handlers for any specific
capability. Also deferred: `process.run`'s line-streaming (Phase 15, a different transport
concern).

### Phase 10: Lua VM Bootstrap & the Loader

Research Milestone. Instantiate `mlua` in `renderer` for the first time and build the loader: the
Lua evaluation of `shell.lua` into a node tree, reused for both a candidate's first evaluation and
the authoritative generation's re-evaluation on an in-place reload (`CONTEXT.md`, Loader).

#### Research Task & Goals:
1. **Type-marshalling boundary** (`oblisk-idl-api-specs.md` § 1.1). The strict, non-coercive
   Rust-Lua type table. Implement it as the one conversion boundary every value crosses, not ad
   hoc per call site.
2. **`Signal` primitive** (§ 1.2). `signal:get()`, `signal:map(fn)`, and `computed(dependencies,
   fn)`. `computed`'s 5ms-per-evaluation CPU cap needs research against `mlua`'s interrupt/hook
   API (`Lua::set_interrupt` in recent `mlua` versions) to find a mechanism that can actually
   abort a runaway Lua closure, not just measure after the fact.
3. **Node constructors**: `rect`/`row`/`column`/`text`/`icon`/`button`/`list`/`textfield` (§ 5.2)
   as Lua-callable sugar producing tagged tables carrying a `kind` field. This is the loader's
   output for Phase 12's retained-scene reconciliation to consume, not the final in-memory node
   itself.
4. **Topology extraction**. The loader's evaluation must expose the top-level `surface` nodes'
   layer/anchor/monitor set as a distinct, cheap-to-diff output, separate from the full node tree.
   This is what Phase 13's watcher compares across reloads to decide swap vs. in-place, and it
   must be obtainable without running Phase 12's full retained-scene transaction.

Deliberately deferred: the full retained-scene reconciliation (Phase 12); write-command dispatch
back through Phase 9's socket (needs a specific capability consumer, not loader work); `textfield`
`secure_submit` wiring to `SecureBuffer` (Phase 15, needs both the node and the socket).

### Phase 11: Minimal End-to-End Slice

Prove Phase 9 and Phase 10 are wired correctly before building anything on top of either. The
Supervisor pushes one real `StateSnapshot`, reusing the already-shipped PipeWire mixer output from
`audio::mixer` and giving it a real destination instead of Phase 6's `eprintln!` ceiling, over the
real socket. The Renderer's loader evaluates a one-line `shell.lua` reading that value back as a
`Signal`. The Supervisor is the listener, the Renderer connects as client. Match Phase 9's own
design here; don't invert it. Acceptance test: a real audio-mixer volume change, made on the
system, visibly reaches a `Signal:get()` call inside the Renderer process.

### Phase 12: Retained Scene & the One-Pass Layout Engine

Implement `renderer/src/layout/mod.rs` against `oblisk-layout-engine-geometry.md` § 3-5 and
`CONTEXT.md`'s retained-scene entries. Not a tree rebuilt from scratch each evaluation, but a
persistent structure the loader's output is reconciled into.

1. **Constraint Pass** (§ 3.1, top-down). Available bounds minus padding/margin, clamped by
   `Pixels`/`Percent`/`Content`/`Fill`.
2. **Size Resolution Pass** (§ 3.2, bottom-up). `Content`-sized text measured via `cosmic-text`
   (Phase 4's shaping pipeline already does this off-thread); `Row`/`Column` intrinsic-size
   formulas.
3. **Position & Stretch Resolution Pass** (§ 3.3, top-down). Spare-space distribution by
   `align_h`/`align_v`, not left-alignment-only.
4. **Retained-scene transaction** (`CONTEXT.md`). Each reload cycle's batch apply: match fresh
   nodes to existing retained nodes by index (§ 4, Keyed Reconciliation), write the changes, and
   tear down removed subtrees child-first so a parent never frees a resource a child still
   references.
5. **Lease**. A removed node's GPU resource survives past its removal from the tree until whatever
   still needs it (a wallpaper crossfade, Phase 16) finishes consuming it. Build the mechanism now
   even though its only real consumer lands later; retrofitting deferred cleanup onto an
   already-shipped child-first teardown is more invasive than building it in from the start.
6. **Overlay input-region bounding boxes** (§ 5). Bounding-box union over `overlay_canvas`'s
   visible children, projected logical-to-physical with floor/ceiling snapping, pushed via
   `wl_surface::set_input_region`. Reuse `text/snap.rs`'s existing `snap_to_physical`/
   `snap_border_to_physical` (Phase 4); the second has no caller yet and was built for exactly
   this. Per ADR-0038 this is a per-surface operation, not an `overlay_canvas` special case: it is
   a no-op for a tightly-sized bar and applies to any surface larger than its visible content. The
   live push waits for Phase 20 (ADR-0023 item 5).

Deliberately deferred: `list`'s virtual-repeater fast-reconciliation beyond basic indexed diffing,
if index-based matching turns out insufficient for reordering without full rebuild. Flag rather
than solve speculatively.

### Phase 13: Watcher & In-Place Reload

Build the Supervisor-side `inotify` watcher on `~/.config/oblisk/` (`CONTEXT.md`, Watcher).
`inotify` has been an unused dependency since scaffolding, staged for exactly this. On a debounced
file-change event, the watcher asks the currently authoritative generation's loader (Phase 10) to
re-evaluate and report its new surface topology (§ 15.1). Unchanged topology: send `capability:
"renderer", action: "reset_registrations"` (ADR-0006) and apply the fresh evaluation in place
(ADR-0001). One Lua evaluation total, no process spawn, no Wayland rebinding. Changed topology:
hand off to Phase 14's `run_pba` for a full generation swap.

Rollback (`CONTEXT.md`): the pre-reload retained scene stays applied until the new evaluation
fully succeeds. A failed in-place evaluation surfaces through `oblisk.rescue` (`is_rescue`/
`error_log`, IDL § 2.10) instead of applying a broken tree or leaving the shell blank.

### Phase 14: Wire the PBA Orchestrator, Per-Output Evidence & Renderer Presentation Feedback

`reload::run_pba` and its `CandidateLink` trait (Phase 8, ADR-0019) are fully built and tested
against a fake. This phase gives them their first real implementation and their first real caller.

1. **Real `CandidateLink`** over Phase 9's socket. `push_state_snapshot`, `recv_ready_signal`,
   `send_activate_draw`, `recv_presentation_evidence` as real framed messages, not a fake.
2. **Per-output evidence** (closes ADR-0019 item 5). `CONTEXT.md`'s `Authoritative generation (per
   output)` already settles the shape: authority transfers per output as each output's evidence
   arrives, not all-or-nothing. `recv_presentation_evidence` takes an output identifier; the
   Supervisor promotes each output independently as its evidence lands. Reaping Generation `N`'s
   process is still all-or-nothing, a process can't be partially reaped, so it happens once every
   output Generation `N` owned has individually transferred to `N+1`, matching ADR-0003's
   per-`(generation, output)` model.
3. **Renderer-side null-buffer commit and `wp_presentation_feedback`** (§ 15.2-15.3, closes
   ADR-0019 item 3). Acknowledge `zwlr_layer_surface_v1`'s `configure` without a visible commit,
   then on `ActivateDraw`, draw the first real frame and attach a presentation-feedback request.
   Requires `wayland-protocols` (not just `wayland-protocols-wlr`) with its presentation-time
   client feature enabled. Confirm the exact feature name against the crate's current published
   API before depending on it.
4. **Swap's input-deselection and promotion messages** (§ 15.4, closes ADR-0019 item 6). The two
   remaining wire messages `run_pba` doesn't send today: clearing Generation `N`'s input region
   per promoted output, and signaling `N+1` to claim focus.
5. **`main.rs` wiring**. `mod reload;` already exists in `supervisor/src/main.rs` with no caller
   (Phase 8). Phase 13's watcher becomes that caller here.
6. **A Candidate that cannot build a scene must not signal ready.** Found live against
   `dev-config`, not by a test. `run_startup_evaluation` sets rescue state and returns on an apply
   failure, and `wayland::run` then binds, presents and signals ready regardless, so the Supervisor
   promoted a Candidate whose config had been rejected and SIGTERM'd a working Generation 0. The log
   reads in order: `startup shell.lua evaluated but failed to apply to the scene`,
   `main_bar activated ... presentation feedback requested`, `superseded generation 0 exited
   cleanly`, `PromoteGeneration(main_bar) received`.

   The evidence gate cannot catch this on its own, and that is the part worth understanding before
   fixing it. Evidence is `wp_presentation_feedback` on a surface whose pixels come from
   `draw_main_bar_proof_text`, which reads nothing from the scene, so presentation proves the GPU
   works rather than that the config does. Phase 19 item 6 makes the pixels come from the scene,
   which narrows the hole without closing it: an empty scene is a legitimate config, and a Candidate
   that presents nothing is indistinguishable from one that was asked to present nothing.

   The Renderer is what has to refuse. `OBLISK_PBA_CANDIDATE` is already read in `wayland::run`
   three statements away, and `run_startup_evaluation` currently returns `()`. A Candidate whose
   startup evaluation or apply fails should exit non-zero without signalling ready, and let
   `run_pba`'s ready deadline or the child's exit drive the rollback that already exists.

   Generation 0 keeps today's behavior, and ADR-0024 item 4's reasoning is why: a blank shell that a
   config edit can recover beats no shell, and on first boot there is no prior Generation to roll
   back to. That ADR's doc comment reasons only about first boot and never asks whether this process
   is a Candidate. Amend it with a banner when this lands.

### Phase 15: `process.run`, Stream Piping & Secure Input

1. **`process.run`'s Lua binding** (closes ADR-0018 items 1-2). Call the already-built
   `process::spawn_group_leader` directly; do not reimplement process-group spawning. Pipe
   stdout/stderr line-buffered and non-blocking into Lua callbacks via
   `tokio::io::AsyncBufReadExt`, not a dedicated blocking thread with a synchronous `BufReader`.
2. **`textfield` `secure_submit`** (closes ADR-0015 item 2). Keystrokes from `wp-text-input-v3`
   append directly into `shared::SecureBuffer` (fully built and tested since Phase 5, zero
   production callers until now), never through Lua. Cross Phase 9's socket as a distinguished
   envelope variant, and call `.zeroize()` immediately after the send completes, per ADR-0005 and
   the `SecureBuffer` module doc comment's own warning not to rely on `Drop` alone.
3. **Real PAM conversation** (closes ADR-0015 item 1). Replaces
   `dbus::polkit::AuthenticationAgent::begin_authentication`'s channel-forward. The PAM crate
   choice is unresearched, ADR-0015 says so explicitly. Spike it first, and write an ADR for the
   pick before wiring it in; this is exactly the hard-to-reverse, surprising-without-context kind
   of decision `domain-modeling` reserves an ADR for.

### Phase 16: D-Bus & Hardware Backend Controllers

Notifications (§ 1, ADR-0033 — expanded scope and one sub-item split out below), Tray (§ 2), MPRIS
(§ 3), NetworkManager (§ 4), BlueZ (§ 5), idle (§ 7, ADR-0032 — split into two named sub-items
below), telemetry (§ 11), and power/thermals (§ 13) all follow the pattern already proven twice in
this codebase, `dbus::polkit` (D-Bus proxy/agent registration) and `audio::mixer` (event-driven
listener thread), and are TDD-able against real fixtures per `oblisk-tdd-test-harness.md` (p2p
D-Bus connections, real sysfs roots), no mocks needed. Use `#[zbus::interface]`, not
`#[dbus_interface]`; ADR-0013 already documents why the latter doesn't exist in the zbus version
this workspace actually depends on. None of these block each other; sequence them by product
priority once Phase 11's transport makes any of them worth building. Building a backend before
that just repeats ADR-0015/0017's `eprintln!`-dead-end pattern a third time.

Idle (§ 7) is one controller sharing generation-scoped cleanup, but ADR-0032 splits it into two
named sub-items because its two halves don't share a transport — don't assume inhibit inherits
notify's Wayland-specific setup work:
- **Idle notify**. `ext_idle_notifier_v1` on the Supervisor's own dedicated Wayland connection (a
  sibling to lock authority's own dedicated connection, ADR-0010). Needs the `wayland-protocols`
  `"staging"` Cargo feature gate.
- **Idle inhibit**. `org.freedesktop.login1.Manager.Inhibit` on the already-open system D-Bus
  connection NetworkManager/BlueZ/polkit already share. Needs neither a new connection nor the
  `"staging"` feature gate.

Notifications (§ 1) grew past what the services doc specs — ADR-0033 adds an allowlist-parsed
`body-markup`/`body-hyperlinks`/`body-images` span grammar, Lua-configured per-urgency sound, and a
real do-not-disturb backend, none of which either spec doc names. Build the Supervisor-side
controller (D-Bus server, sanitizer/allowlist parser, SHM icon spooling, sound trigger, DND state)
as one Phase 16 slice like every other controller here — but the matching renderer-side span
*rendering* (bold/italic weight, clickable hyperlink regions, inline image layout) is its own
later slice, not part of this one:
- **Notifications body-span rendering**. First real consumer of ADR-0012's FemtoVG glyph-atlas
  work — no rich-text/span support exists in the renderer's text pipeline yet. Sequence after
  Phase 12 (retained scene) and whatever slice lands ADR-0012's glyph atlas; until then, Lua
  receives correct `NotificationSpan` data with no widget yet able to render it as anything but
  flattened text.

Two exceptions worth building on their own track instead of folding into the general D-Bus
pattern above:
- **Wallpaper transition engine** (§ 8). Needs Phase 12's lease mechanism for its double-buffered
  crossfade, not a D-Bus controller pattern. Sequence after Phase 12, independent of the rest of
  this phase.
- **Active-window tracking** (§ 9). Binds the foreign-toplevel protocol client-side in
  `renderer/src/wayland/`, not a Supervisor D-Bus backend. Coupled to that module's growth
  instead.

### Phase 17: XDG Atomic State Manager

`~/.local/state/oblisk/state.json`, written via temp-file-plus-atomic-rename (§ 14.1: write to
`.tmp`, `sync_all`, `rename`). Pure local file I/O in `supervisor` with no dependency on the
socket, Lua, or any D-Bus controller. Can be built any time, including in parallel with Phase 9 or
10, as a low-risk warm-up if one is wanted.

---

## 5. Renderer Playbook: Phases 18 Onward

Phases 9 through 17 built the Supervisor out to ten capabilities pushing real system state into
live Lua signals. None of that state can become a pixel. The Renderer draws one hardcoded proof
string (`wayland/mod.rs`'s `draw_main_bar_proof_text`, labelled a Phase 4 integration proof in its
own comment), and the resolved `Scene` the layout engine maintains is never referenced by the
Wayland module at all. ADR-0023 item 9 recorded this deliberately: Phase 12 was the layout module,
not the paint pipeline.

Two structural findings from the 2026-08-28 renderer review set the order of what follows.

The Renderer is two OS threads that cannot reach each other's state (ADR-0039). The Lua VM and the
retained scene are on the socket thread; the EGL context, the surfaces, and the FemtoVG canvas are
on the Wayland thread. Painting, dynamic surfaces, and input dispatch are all blocked on the same
seam, so it is removed first and alone.

The surface set is hardcoded in Rust and disconnected from `shell.lua` (ADR-0038). Three roles are
created before any Lua is evaluated, so a config's own `surface` declarations are discarded. This
is what stands between Oblisk and the Quickshell-shaped goal of a config that builds any shell
component rather than a bar with an overlay.

A third finding arrived with the 2026-08-28 scope call for Quickshell-equivalent freedom: floating
windows, popups, session lock, and per-screen variants. ADR-0040 turns the surface set into four
Wayland roles (`panel`, `window`, `popup`, `lock`), ADR-0041 replaces `Variants` with a Lua loop over
a new `oblisk.screens` signal, ADR-0042 moves the session lock into the Renderer, and ADR-0043 sets a
memory budget with the two decisions that make it reachable.

A fourth finding came out of the 2026-08-28 review of the Lua boundary, and it is the oldest of the
four. No path exists from a capability's state to the screen. A `StateSnapshot` push writes its
`LiveSignalHandle` and stops there, `layout/node.rs` rejects a `Signal` handle in every property it
parses, and re-evaluation runs only on a config edit. Change the volume and nothing moves. ADR-0023
item 2 deferred this and named Phase 13 as the place to pick it up; Phase 13 shipped without it and
no later phase claimed it. ADR-0044 settles it, and it has to land before the frame gating below,
which currently names no mechanism for deciding that the scene changed.

The phases below close all four. Each reuses what already exists rather than re-deriving it, and
each names what it leaves for the next, following the same scope-ceiling discipline as ADR-0021 and
ADR-0023. Phases 18 through 21 are strictly ordered, since each unblocks the next; 22 through 24 are
independent of each other once 21 lands, except that Phase 24 is worth building early. Phase 25 is
independent of all of them and blocks nothing, but every § 3.2 write command stays unreachable from
Lua until it lands.

Phases 26 and 27 come from a fifth review, of Quickshell's own QML-to-C++ layer on 2026-08-28. They
close the two gaps that changed a design rather than adding a feature: a config is a directory of
Lua files and not one file (ADR-0047), the config VM drops the stdlib calls that would stall the
Wayland thread ADR-0039 put it on (ADR-0048), and a startup failure needs an error path that does not
run through the config that just failed (ADR-0046). The same review produced ADR-0045, which lands
inside Phase 19 because it changes reconciliation. Both phases are independent of everything above.

### Phase 18: Renderer Thread Consolidation

Implement ADR-0039. Move `Loader`, the live-signal map, the rescue state, and `layout::Scene` into
`wayland::run`; demote `socket::spawn_client` to framed I/O that forwards inbound
`SupervisorFrame`s over a channel and writes outbound envelopes it receives over another. `mlua::Lua`
is `!Send`, so this is a construction move, not a hand-off.

Collapse what the move makes redundant: three of `main.rs`'s four channels become direct calls,
and the duplicate `ShapingHandle` and its second `FontSystem::new()` startup cost go away
(ADR-0023 item 8).

`PLACEHOLDER_OUTPUT_SIZE` does not go with them, and ADR-0039 decision 4 is wrong about why it
could. Being on one thread makes the configured sizes reachable, not attributable. `Scene` keys
surfaces by the `id` a config writes, and `wayland::mod` hardcodes `TrackedSurface::surface_id`
from `SurfaceRole::label()`. Today those id spaces do not intersect: the only config in the repo
declares `"bar"` and the three Rust-created surfaces are `"main_bar"`, `"overlay_canvas"`, and
`"wallpaper_layer@{output}"`. Nothing to look up, and a fallback for the misses would be a
mapping policy invented here for ADR-0038 to delete. It moves to Phase 20 item 4, which deletes
`SurfaceRole` and is what makes the two id spaces one.

A pure refactor with no behavior change: same three hardcoded surfaces, same proof string, same PBA
handshake, fewer threads. That is the acceptance criterion. Resist folding any of Phase 19 into it;
a refactor that also adds a paint pass cannot be verified as behavior-preserving.

Deliberately deferred: everything the move unblocks. Painting, surface creation, input, and the
`overlay_input_regions` push all wait for their own phases, even though each becomes a local call
the moment this lands.

### Phase 19: Signal Reactivity and the Paint Pass

The first phase that puts Lua-driven content on screen, and the first where that content changes on
its own. Items 1 through 5 implement ADR-0044 and ADR-0045 and are verifiable with no EGL at all:
push a snapshot, assert the resolved `ResolvedNode` tree changed. Items 5 onward turn a resolved
tree into pixels. Build them in that order, since item 9's gating condition is item 2's output.

1. **Resolve `Signal` properties instead of rejecting them** (ADR-0044 decision 1). Where a parser
   finds a `Signal` userdata, call `get()` and parse the result under the rules it already applies
   to a literal. Resolve exactly once: a signal resolving to another signal is an error, not a
   second read, because chasing it to a fixed point is an unbounded loop on a cyclic construction.
   Take `#[allow(dead_code)]` off `lua/marshal.rs` and run a resolved value through
   `check_number`/`check_integer`/`check_string`: this is the Lua-authored value crossing into Rust
   that those functions were written for, and until now nothing called them.

   `reject_signal` does not go away entirely. The four `SurfaceTopology` fields (`id`, `layer`,
   `anchor`, `monitor`) keep rejecting, because topology is computed at evaluation time for
   `handle_reevaluate` to diff against `applied_topology`, and a field that changes after that
   comparison would let a surface move layer or monitor with no generation swap (ADR-0001). The
   helper survives for those, renamed to say so. See ADR-0044's amendment banner.

   Resolving runs Lua, since a `computed` signal calls its closure, so the parsers need a `&Lua`
   threaded down through `Scene::apply`'s call chain. That is what routes the call through
   ADR-0021's 5ms hook rather than around it.
2. **The dirty flag** (ADR-0044 decision 2). `LiveSignalHandle::set` marks the scene dirty. Hold the
   last evaluation's `LoadOutput` and re-run `Scene::apply` against it when the flag is set, without
   running `shell.lua`. Mark it `ponytail:`, naming the ceiling: one flag for the whole scene, so a
   high-frequency capability re-resolves surfaces that read nothing from it.

   Four things this item's own review found, all of which belong in it. A signal resolving to `nil`
   must mean the property is absent, or a config binding a bare capability signal cannot boot at
   all: startup evaluation runs before the first `StateSnapshot` is drained, so every rostered
   signal reads `nil` and layout rejects the tree (see ADR-0044's amendment). The flag must not be
   consumed when there is nothing to re-resolve against, or a push arriving before the first
   successful apply is cleared and lost, which after a failed startup means a permanently blank
   shell that no later push can recover. `Scene::retiring` now grows at push cadence rather than per
   config edit, and `Scene::release` still has no production caller (ADR-0023 item 7), so a
   successful apply has to drain it. And the `rescue` signal shares the flag, so a clean startup and
   a `TopologyChanged` verdict both leave the scene marked dirty, the second of which re-applies a
   generation whose scene is supposed to stay untouched.

   Order the poll loop drain, then re-resolve, then draw. `ActivateDraw` arriving in the same drain
   as a snapshot would otherwise paint the pre-push layout, and nothing would draw the corrected one.
3. **Recursion depth cap.** `resolve_and_reconcile` recurses through `children_of` with no counter,
   so `local r = rect {}; r.children = { r }` overflows the stack and aborts the process past the
   guard page, where `oblisk.rescue` cannot catch it. § 1.1 caps strings at 64KB and integers at
   2^53 and says nothing about table shape. Cap depth at a constant, return a `LayoutError`, and let
   the existing rescue path report it. Item 2 makes this reachable far more often, since a cyclic
   tree now re-resolves on every push rather than once per config edit.

   Item 1's review confirmed two more entrances to the same abort, and neither is caught by
   anything that exists. A computed signal in `children` that returns a fresh table per read
   (`deep = computed({}, function() return { rect { children = deep } } end)`) builds an infinitely
   deep tree: item 1's "resolved to another `Signal`" guard never fires, because every result is a
   `Table`, and the recursion is pure Rust, so Lua's own `LUAI_MAXCCALLS` never accumulates. It
   aborts at an 8 MiB stack. A self-referential or mutually recursive computed
   (`a = computed({}, function() return b:get() end)` and back) does the same through
   `Signal::get_value`, and merely assigning the handle to a property now triggers it where it used
   to need an explicit `:get()` in `shell.lua`.

   ADR-0021's 5ms cap cannot stop either, and this is a flaw in the cap rather than a gap in its
   coverage: every nesting level calls `push_deadline`, and the instruction hook reads
   `stack.last()`, which is always the innermost and always fresh. Monotonically deepening recursion
   provably never trips it. Reading the *outermost* deadline instead would make the budget apply to
   the whole nest, which is what it was meant to bound. Decide that with the depth cap, since a
   depth counter alone leaves the cap still unable to bound a wide-but-shallow evaluation. See
   ADR-0021's amendment banner.

   The slice's own review then found the depth cap missed the commoner shape entirely, so four more
   things belong here. `Signal::get_value` resolved a computed's dependencies *before* pushing a
   deadline, so a dependency chain nested Rust frames with the stack pinned at depth 1: a 200 link
   `map` chain reached depth 200 with a maximum stack of 1, and 5000 links aborted with neither cap
   firing. The deadline has to cover dependency resolution, which is also what makes one deadline
   govern a whole evaluation. The hook's error is an ordinary Lua error that `pcall` catches (a
   measured body ran 37.6 ms and returned a partial value), so the budget needs a second gate at the
   Rust boundary after the call returns, which a config cannot catch. `Lua::set_hook` installs per
   Lua thread, so a computed body working inside a coroutine was never hooked at all: use
   `Lua::set_global_hook`. And the two caps must agree on whether the limit is inclusive, with each
   message stating the limit actually enforced.

   Size the constants against the compounded worst case, not each recursion alone. Measured on a
   2 MiB debug test thread: 5,216 bytes per tree level, 14,864 per body-nesting signal level, 1,840
   per dependency-chain level, additive rather than multiplicative because a signal nest pops before
   the tree descends. The original 128 and 32 peak at 1.14 MB, a 1.8x margin rather than the 3x and
   4x their comments claimed.
4. **Reconcile by `id`, not by position alone** (ADR-0045). `Scene`'s § 4 matching pairs a parent's
   children by index, so inserting a node above a sibling shifts every node below it onto the wrong
   retained counterpart. Add an optional `id` base property on every node kind, pair identified
   children first within one parent, then fall back to today's positional rule for the rest. Reject
   duplicate ids among siblings as a `LayoutError`. This matters here rather than later because
   item 2 turns reconciliation from a per-edit event into a per-push one.

   "Fall back for the rest" means the rest of the *unidentified* children, on both sides. An `id`
   has to mean "this is the same node, and only the same node" in both directions, or declaring one
   is weaker than declaring none: an identified fresh child whose id is new must not adopt a
   positional leftover, and an anonymous fresh child must not inherit a retained node that declared
   an id. See ADR-0045's amendment banner, which exists because the first implementation read the
   ADR's looser wording the other way and measured `[a,b,c]` against `[b,c,d]` retiring nothing.

   Pair through a per-parent map rather than a nested scan. Sibling count is config-controlled and
   this now runs on the Wayland dispatch thread at push cadence: a nested scan measured 32ms for
   1000 reversed siblings and 380ms for 4000. Reject a non-UTF-8 `id` rather than converting it
   lossily, since an id is an equality key and `U+FFFD` collapses distinct ids together.

   ADR-0045's `list` and `key` half is not part of this item. `list` is registered as a Lua
   constructor in `NODE_KINDS` but rejected by `ensure_supported_kind`, so it never reaches
   reconciliation; `key` has nothing to attach to until `list` is a real node kind. That is item 12.
5. **Resolve each property once per pass.** Item 1's review measured one `Scene::apply` over
   `surface > row > rect` and found `margin` resolved four times (the parent loop, both
   `intrinsic_content_size` folds, and `position_children`), `align_v` and `spacing` twice each,
   `visible` and `width` once. They are independent `Signal::get_value` calls, so a closure that is
   not a pure function of unchanged state answers differently within a single pass.

   That is a real geometry bug, not just waste. With `m = computed({}, function() n = n + 1; return
   { left = n } end)`, a row measured `width == 12` from the second read and positioned its child at
   `x == 4` from the fourth, so a 10-wide child spans 4..14 inside a 12-wide parent. `scene.rs`'s own
   comment states the invariant this breaks: the sizing pass and the positioning pass "must agree, or
   a child would be sized to fit but then overlap or leave a gap once positioned". `os.clock()`,
   `math.random`, or any accumulator upvalue reaches it.

   It also multiplies cost. ADR-0021's cap is per `get_value` call, so four resolves buy four
   independent 5ms budgets: 100 margined nodes whose closures sit near the cap is a 2 second freeze
   of the Wayland dispatch thread, measured at 47ms for 20 such nodes today. And a resolved table's
   `__index` runs once per read entirely outside the cap, since `call_with_cpu_cap` removes the hook
   when `get_value` returns while `parse_edge_insets` reads four keys afterwards through
   metamethod-aware `Table::get`. A `__index` of `while true do end` hangs unkillably.

   Resolve every property of a node once and have the sizing and positioning passes read those
   values rather than the raw `mlua::Value`s. Not literally at the top of the node's own reconcile:
   a parent reads its child's `margin` to compute the budget it recurses with, so the resolve has to
   happen in the parent's loop iteration for that child, with the surface root resolved by
   `Scene::apply` itself. Same once-per-node guarantee, one frame further up. This is not the
   memoization ADR-0044 decision 3 rejects: that is about caching *across* pushes, and this caches
   nothing beyond the pass it happens in. One pass, one answer per property, which is what makes the
   resolved tree a snapshot rather than four disagreeing reads.

   Two things this item does not deliver, against how the paragraphs above read. The `__index` hole
   stays open, and it is worse than "a signal read twice": a plain Lua table with an `__index`
   reproduces the whole original bug with **no signal involved at all**. `parse_edge_insets` is still
   called four times per child per pass on the same resolved table, 16 metamethod invocations, and a
   measured `margin` metatable produced a row measured 18 wide with its child positioned spanning
   16..26, which is the exact assertion this item's own headline test makes. Those reads also run
   with no instruction hook installed, because `CpuBudget` drops its hook when `Signal::get_value`
   returns: an `__index` spinning 200 million iterations made one `Scene::apply` take 26.10 seconds
   and return `Ok(())`, where the same loop inside a `computed` was refused in 5.12ms. Closing it
   means parsing geometry once into the retained node too, and bounding a whole layout pass rather
   than each getter call. And this does not remove `Scene::apply`'s rollback snapshot: resolution
   stays interleaved with the walk, so a failure at depth still leaves partial mutation to undo.

   Accept one cost knowingly. Resolving the whole property map means every signal-bound property
   resolves every pass, including the paint-only ones no parser reads yet (`background`, `color`,
   `radius`), each buying its own 5ms budget. That is the price of the resolved tree being a
   complete snapshot, which is what lets item 6 read a colour off it without resolving anything
   itself. It also means a getter that raises fails the apply even for a property nothing currently
   reads, which is correct: § 1.2 says any property may hold a `Signal`, so "nothing reads it" is a
   fact about today's parser set, not about the config.
6. **Per-node drawing.** `rect` (background, `radius`, per-edge `border_color`/`border_width`) and
   `text` (reuse `TextPainter`, already a FemtoVG `Canvas<OpenGl>` with `resize` per frame, and
   `text/shaping.rs`'s existing off-thread shaping). `row`/`column`/`button` are containers with no
   paint of their own beyond their `rect` properties. Draw in tree order so the stacking model
   ADR-0023 item 4 already implements resolves overlaps the way layout resolved them.

   Give `content` and `icon`'s `size` defaults here, so absence stops being an error for them. Item
   2 made a signal resolving to `nil` mean absent, which is what lets a config bind a capability
   before its first push, but `text { content = oblisk.mpris.title }` still fails at boot because
   `content` is required. A `text` bound to a not-yet-pushed signal is a normal boot state, and once
   `nil` maps to absent the parser cannot tell it apart from an omitted key, so the fix is a default
   rather than a new distinction. It belongs here rather than in item 2 because painting an empty
   `text` is what makes an empty default meaningful. The cost, accepted: a misspelled `content` key
   renders an empty node instead of being rejected.

   Built in three commits rather than one, recorded here because the middle one leaves visible
   debt. First the two defaults above, which need no renderer at all. Then the paint property
   parsers (`background`, `radius`, `border_color`, `border_width`, `foreground`), which are pure
   functions over an already-resolved property map. Then the drawing pass and the harness, which is
   the only part that needs a GL context. The parsers land with no production caller and therefore
   with an `#[allow(dead_code)]` apiece, seven of them, more than the rest of the crate accumulated
   across Phases 12 through 19 combined. That is the price of the split, and the third commit pays
   most of it back, not all: it deletes six of the seven attributes by giving every parser a real
   caller in the new `layout::paint` module, but adds one of its own, on `paint::paint_tree` itself.
   `RendererClient` (`renderer/src/socket.rs`) keys a `Scene` by the `id` a config writes (`"bar"`);
   `wayland::mod` keys a `wl_surface` by `SurfaceRole::label()` (`"main_bar"`, `"overlay_canvas"`,
   `"wallpaper_layer@{output}"`). Those two id spaces don't overlap, so there is no surface whose
   retained tree a lookup could find to hand `paint_tree` -- `socket.rs`'s `PLACEHOLDER_OUTPUT_SIZE`
   `ponytail:` comment spells out the same gap. Any mapping invented here would be policy Phase 20
   item 4 deletes outright, once it removes `SurfaceRole` and unifies the two id spaces -- that is
   what gives `paint_tree` its first production caller and lets this last attribute go. Net debt
   goes from seven attributes to one, real and visible rather than hidden, which is why it is
   written down here.

   The harness has to come with the drawing pass and not after it. `EGL_MESA_platform_surfaceless`
   plus a pbuffer surface is confirmed working on Mesa 26.2: a GLES 3.2 context, and `glReadPixels`
   returning an exact `#FF0000FF` after a red clear, both on Iris and, with
   `LIBGL_ALWAYS_SOFTWARE=1`, on llvmpipe. So the gate is "EGL init failed, skip" rather than "this
   only runs on a developer's desktop", and there is no reason to write the drawing pass first and
   assert its pixels later.
7. **Snapping.** Reuse `text/snap.rs`'s `snap_to_physical` and `snap_border_to_physical`. The second
   has had no caller since Phase 4 and this is what it was written for: a border snapped to whole
   physical pixels instead of straddling two (`oblisk-layout-engine-geometry.md` § 5).

   **Amendment, on landing. `snap_border_to_physical` was deleted rather than wired up, because
   its contract is wrong.** It rounds a coordinate to the nearest physical pixel *center*, a
   half-integer, which is correct only when the stroke is an odd number of physical pixels wide.
   Measured directly against femtovg 0.26.0 on this machine's Mesa/Iris, stroking a horizontal
   line and reading back a pixel column:

   | stroke width | centerline | rows lit |
   |---|---|---|
   | 1 | 10.5 | row 10 at 255. Crisp. |
   | 1 | 10.0 | rows 9 and 10 at 128. Blurred across two. |
   | 4 | 10.5 | rows 8 to 12 at 127, 255, 255, 255, 127. Blurred across five. |
   | 4 | 10.0 | rows 8 to 11 all 255. Crisp, exactly four. |

   An even-width stroke wants an integer centerline, an odd-width stroke a half-integer, so a
   function that always returns a half-integer makes a 4px border worse than leaving it alone. It
   had no caller, so nothing had to be unbuilt.

   `snap_border_band(start, thickness, scale) -> (f32, f32)` replaces it: round the band's two
   edges to the nearest physical pixel independently, and let the centerline and width fall out.
   Parity then takes care of itself at every width. A positive thickness whose edges round
   together is forced to one physical pixel, so a hairline the config asked for cannot vanish
   between two pixels. It returns logical units, since `layout::paint` builds every path in
   logical coordinates and `TextPainter::resize` hardcodes a device pixel ratio of 1.0, so a
   caller handed physical units would convert every one back.

   Both branches of `paint_border` use it. The per-edge fill branch snaps each edge's thin axis
   (an `EdgeAxis` argument says which, since inferring it from the rect's own width and height is
   ambiguous whenever a node's height equals its border width). The uniform-width-with-radius
   stroke branch snaps the node's box span on each axis and the stroke thickness, then keeps the
   existing half-width inset, computed from the snapped thickness.

   Two things deliberately left alone. `fill_rect` does not snap: backgrounds are a separate
   question with their own trap, since two adjacent snapped rects either overlap or leave a seam
   depending on the rounding rule, and this item names borders. And item 17's scissor clip keeps
   using `snap_to_physical`, which rounds outward, not `snap_border_band`, which rounds to
   nearest. The two sit next to each other and must not be unified: a clip has to grow outward so
   it never shaves a pixel a box legitimately filled, a border has to round to nearest so it
   never comes out a pixel wider than asked for. `text/snap.rs`'s module doc comment now says
   this, since picking the wrong one is the mistake the pairing invites.

   Three pixel tests cover it, all proved by removing the snap and rerunning. A 1px border at
   `padding.top = 10.3` reads `(201, 178, 178, 255)` on its row unsnapped instead of full white.
   A 4px filled edge at 31.3 (close to the dev config's real `notification_area` height of 31.6)
   loses a fully-lit row the same way. The stroke branch needs its own fixture at a fractional
   position, since the pre-existing radius test sits at an integer padding where the stroke
   already lands on whole pixels and passes with no snapping at all.
8. **One canvas, many surfaces.** All surfaces share one EGL context, so one `TextPainter` serves
   all of them: make the surface's EGL surface current, `resize` the canvas to that surface, draw,
   swap. Verify FemtoVG tolerates the surface switch under a shared context before assuming it; if
   it does not, one canvas per surface is the fallback, not a redesign.

   > Built inside Phase 20 item 4, and it could not have landed anywhere else: deleting
   > `SurfaceRole` deletes `draw_main_bar_proof_text`'s only condition, so `paint_tree` had to
   > become the draw path in the same commit that removed the enum.
   >
   > The shared-context question is verified rather than assumed, as this item asks.
   > `one_canvas_draws_correctly_across_two_surfaces_sharing_one_context` builds two pbuffers on one
   > EGL context and drives one `TextPainter` across both, asserting three separate things: the
   > second surface draws at all (the canvas survived `eglMakeCurrent`), it draws at its own size
   > (the `resize` took effect), and the first surface's framebuffer is untouched by the second's
   > draw. It runs for real on this machine's Mesa rather than skipping. The reason it holds is that
   > under EGL a context owns its GL objects while a surface is only the framebuffer, so the
   > per-surface work is `Canvas::set_size` and nothing else. The fallback was not needed.
9. **Frame-callback scheduling.** Request `wl_surface::frame()` and redraw only when both a frame
   callback has arrived and item 2's re-resolve produced a different result for that surface, rather
   than on the current 15ms poll timeout in `wayland::run`'s loop. ashell's `src/application.rs` does
   exactly this with a `frame_pending` flag and reports an idle cost of zero, which is the target:
   the loop blocks when nothing is happening. Write the gate so a second reason to wake can be added
   next to the first rather than replacing it: an animation repaints whether or not a signal changed,
   and that is the one thing here that would be a redesign rather than an addition if the condition
   is hardcoded to "the scene changed".
10. **Declared fonts, not discovered ones** (ADR-0043). Do not call `load_system_fonts()`. Load the
   families the config names plus its declared fallback chain, through `fontdb`'s
   `load_font_file`/`load_fonts_dir`. This is the single largest lever on the memory budget, and it
   also removes most of the roughly one second `FontSystem::new()` currently costs, since that time
   is mostly cold-cache I/O over fonts the shell never draws with.

   It is also a correctness item, which only became visible once item 6 put real text on a real
   screen. Measurement and paint resolve a font independently and are not resolving the same one.
   `text::shaping::shape` measures with cosmic-text under `Attrs::new()`, which asks for
   `Family::SansSerif` and then runs cosmic-text's own per-glyph fallback. `default_font_bytes`,
   which is what FemtoVG actually rasterizes with, runs its own `fontdb` query for `SansSerif` and,
   when that misses, falls back to `db.faces().next()`: literally the first face in scan order. Its
   own `ponytail:` comment already called that fallback a placeholder for a real policy.

   Measured on this dev machine: the `SansSerif` query misses, so paint drew every glyph in
   **Adwaita Mono** while layout had measured in a proportional face. A `text` node's box came out
   roughly 30% narrower than the glyphs drawn into it, so the bar's media cell overlapped the two
   cells to its right. One font, resolved once and shared by both paths, is what fixes this; a
   config-declared family is what makes that resolution deterministic instead of dependent on font
   scan order.

   Built in two commits. The first is the resolver and the shared chain, which is what closes the
   correctness half; the second is the Lua `fonts = {...}` declaration ADR-0043 decision 2 writes
   out, still to come. The split is because the declaration has an unsettled question the resolver
   does not: a config returns a table of surfaces today, so there is nowhere for a top-level
   `fonts` table to go without changing that return shape, and neither this phase nor Phase 26 owns
   that decision. Until it lands the chain is a constant, `["sans-serif", "Noto Sans CJK JP",
   "Noto Color Emoji"]`, which is the shape ADR-0043 names as what the default config ships.

   The resolver is fontconfig, through one `fc-match` subprocess per chain entry. This is worth
   recording because ADR-0043's "load the declared families through `load_font_file`" understates
   the problem: `fontdb` has no family-name to file index, and `load_system_fonts()` *is* that
   index, so a declared family name cannot be resolved by fontdb alone. fontconfig already holds
   that index and already encodes the system's own font policy, which is a better answer than the
   `db.faces().next()` scan order this replaces.

   fontconfig substitutes rather than failing, so a miss has to be detected by comparing the
   returned family against the request rather than by exit status. `fc-match "ZZ No Such Family"`
   exits 0 and returns Noto Sans. The generic aliases (`sans-serif`, `serif`, `monospace`,
   `cursive`, `fantasy`) are exempt, since substitution is the whole point for those.

   One known limitation, found on this dev machine and left unbuilt. A family that *is* installed
   but that a user's own fontconfig rules strongly substitute away reads as a miss and gets
   dropped, so declaring it yields nothing rather than the wrong font. The machine had a
   `~/.config/fontconfig/fonts.conf` prepending an Arabic naskh face with `binding="strong"` for
   anything fontconfig classified as generic sans-serif, which caught `Noto Sans`, and the file was
   removed rather than worked around. The fix if this ever matters is `fc-match -s`, which prints
   the full ranked list: the substitution is the head, and the requested family sits below it in
   fontconfig's own weight and style ranking, so walking the list to the first family that matches
   the request defeats the substitution while keeping the ranking. `fc-list` is the wrong tool for
   it, since it does not rank (`fc-list "Noto Sans" file` returned 72 faces headed by
   `NotoSans-SemiCondensedExtraBold`).

   Two things this item does not deliver. `femtovg::add_font_mem` takes no face index and always
   loads face 0, so a weight or style living only inside a `.ttc` is unreachable: `Inter.ttc` holds
   36 fonts and only the first is addressable. Harmless today because nothing selects a weight or
   style anywhere in the crate, and `add_shared_font_with_index` is the fix when something does.
   And the divergence test cannot see the original bug any more, which is worth stating rather than
   leaving to be rediscovered: once the chain resolves to a single Latin-covering face, every
   resolution path converges on it, so reverting `shape()` to `Family::SansSerif` leaves the test
   green. It still catches paint and measurement loading different font *sets*, measured at 61.6%
   divergence when the painter was handed the chain minus its first entry. A separate test holds
   one `FontSystem` fixed and varies only the family argument (241.08 against 302.4, and both
   241.08 when the argument is ignored) to cover what the first one no longer can.
11. **Atlas eviction** (ADR-0043). femtovg allocates 512x512 RGBA8 atlas pages, one mebibyte each,
   grows the list without bound, and frees them only on an explicit `clear()`. Clear at a
   page-count threshold on an idle frame and let it rebuild. Mark it `ponytail:`, naming the
   ceiling: a whole-cache drop rather than an LRU, because femtovg exposes no per-glyph eviction.

   **Amendment: not built, because none of its three premises hold.** Checked against femtovg
   0.26.0's own source rather than its docs.

   *There is no public clear.* `GlyphAtlas::clear` is `pub(crate)` (`src/text.rs:928`) and
   `Canvas`'s `glyph_atlas` field is private (`src/lib.rs:312`). The only `clear` a public path
   reaches is on `ephemeral_glyph_atlas`, the per-flush color-glyph atlas, in `flush()`.
   `Canvas::reset()` resets draw state (transform, scissor, paint), not the atlas. ADR-0043
   decision 3 says pages are freed "when something explicitly calls `clear()`"; nothing outside
   the crate can. ADR-0043 still reads as though the call is available, and wants an amendment
   banner pointing here.

   *There is no page count either*, except behind the `debug_inspector` cargo feature, which is
   what gates `debug_inspector_get_font_textures` (`src/lib.rs:2275`). It is an empty feature that
   pulls in no dependency, so enabling it is cheap, but shipping a release build that turns on a
   debug-gated inspector to read one number is the wrong shape for a threshold check.

   *There is no idle frame.* Item 9's frame-callback scheduling is what would define one, and it
   is unbuilt and blocked behind Phase 20 item 4.

   The one reachable mechanism is dropping the whole `Canvas` and rebuilding `TextPainter`:
   `Drop for Canvas` calls `images.clear`, which frees the atlas textures with everything else.
   That is a heavier operation than the ADR costed. It re-uploads every font and drops every
   non-glyph image too, so "a full rebuild on an idle frame is invisible" is a claim about a
   targeted atlas clear, not about this.

   Not built, because the problem is currently unreachable. `paint_tree` has no production caller
   (Phase 20 item 4), so the only glyphs production rasterizes are the six characters of
   `draw_main_bar_proof_text`'s "Oblisk" at one size, which is one atlas page forever. Building a
   whole-canvas teardown, a debug-feature-gated page count and an invented idea of idleness, to
   guard growth that cannot happen yet, is three speculative mechanisms for zero present benefit.

   Revisit when `paint_tree` has a production caller and a config draws varied text, and take one
   of: enable `debug_inspector` and rebuild the painter at a threshold; or upstream a public
   `Canvas::clear_glyph_atlas` to femtovg, which is a small patch against code that already has
   the method. Prefer the second. The first is a workaround for a missing four-line accessor.
12. **The `list` node, and `key`** (ADR-0045, `oblisk-idl-api-specs.md` § 5.2). `list` is registered
   as a Lua constructor in `lua/nodes.rs`'s `NODE_KINDS` but rejected by
   `layout::scene::ensure_supported_kind`, so a config that uses it gets `UnsupportedNodeKind` and
   never reaches reconciliation. Build the node kind: children generated from a `source` collection,
   with `key` mapping a source element to a string used the way item 4 uses `id`, and a duplicate
   `key` rejected as a `LayoutError` the same way a duplicate sibling `id` is.

   Last in the phase because it needs both halves of what precedes it. Item 4's identified pairing
   is the mechanism `key` reuses, and a `list` whose items reorder is only observably correct once
   items 6 through 11 can draw them. Until this lands, a config that wants a dynamic child list
   writes a computed `children` signal, which reconciles positionally and therefore loses identity
   on every insertion.
13. **Cap what an error message interpolates.** Every type-mismatch arm in `layout/node.rs` builds
   its detail with `format!("expected a number, got {value:?}")`. mlua's `Debug` for `LuaString`
   formats the whole byte string escaped, and `marshal::check_string`'s 64KB cap never runs on this
   path because it lives in `checked_string`, which a rejected value never reaches. So
   `rect { radius = string.rep("x", 20 * 1024 * 1024) }` allocates and formats 20 MB on the Wayland
   dispatch thread, then hands it to `rescue`'s `error_log` (§ 2.10) for a human to scroll past.

   Found by item 6's review, which counted four instances and declined to fix them, because the
   shape predates the slice that copied it: `parse_spacing`, `parse_font_size` and `parse_icon_size`
   all shipped with it. It is roughly fifteen call sites and one helper that truncates a value's
   `Debug` form to something a log line can hold, and it wants its own tests rather than a
   mechanical sweep folded into a paint commit. Not urgent: the hostile config is the user's own, so
   this is diagnostics quality rather than a security boundary. It is written down because a fourth
   copy of a bad pattern is how it becomes the convention.

   Item 6's own third commit then changed the cadence this is paid at, and for a while that turned
   it from untidy into a real problem: `Scene::apply` did not parse the paint properties, so
   `layout::paint` was the first thing that ever validated a `background` or a `radius`, and it did
   that while drawing. A malformed value was logged and treated as absent rather than failing the
   apply, so one `background = 5` in one node formatted and printed on every frame.

   **Fixed.** `node::paint_style` parses every paint property once, while `Scene::apply` resolves
   the node, and a failure rolls back and reaches `rescue` exactly as a bad `align_v` does. The
   per-frame log is gone with the five log-and-default draw builders it lived in. It went alone,
   against the note above: the geometry half it was waiting to be paired with does not exist, since
   those parsers already run at apply time under the right failure rule. See docs/adr/0068.

   The 20 MB interpolation itself is untouched. It is now paid once per failed apply rather than
   once per frame, which is the cadence the original judgement assumed.
14. **A container's content size must include its own padding.** Measured live: a content-sized
   `column` holding one 15.6-tall `text` reports 15.6 whether its padding is 8 or 50 on every edge.
   `scene.rs` parses `padding` to inset the box it lays children out in, but no arm of
   `intrinsic_content_size` adds it back, so a content-sized container is exactly `padding` short in
   both axes and its children are positioned past the edge of the box meant to contain them. Every
   child's `margin` is already folded into all four arms, which is what makes the omission read as
   an oversight rather than a rule.

   Harmless for a `Fill` or fixed-size container, where padding correctly shrinks the child budget
   without changing the parent. It bites exactly the case a popup or a notification card is: sized
   to its contents, with padding as the whole point. Item 6 is what makes it visible, since a
   background painted at the reported size will visibly stop short of its own text.

   **Done in `c3bcb1d`**, in `resolve_and_reconcile` rather than in `intrinsic_content_size`, and
   deliberately so: `intrinsic_content_size` answers "how much room do the contents need", which is
   what the row/column sums and the stacking union are about, while a node's own padding belongs to
   the node. Adding it through `own_width_known.unwrap_or(...)` is also what confines it to an axis
   the config did not state, since on a stated axis padding has already done its work insetting the
   child budget and adding it again would count it twice.

   **A leftover this exposed, on leaves rather than containers.** That `unwrap_or` runs for every
   kind, so a `text` or `icon` with its own `padding` grows too. Measured: `text { content = "Ob",
   font_size = 14 }` resolves to 19.544x16.8, and the same node with `padding = 10` on every edge
   resolves to 39.544x36.8, exactly the padding. But `TextPainter::draw_line` draws at the rect's
   origin, and a leaf has no children for `position_children` to inset, so the glyphs stay in the
   top-left corner and the padding becomes dead space on the right and bottom.

   Both halves want doing together, and neither alone is the fix. Dropping the leaf from the size
   calculation makes `padding` on a `text` silently do nothing, which is not better than doing the
   wrong thing visibly. The honest version is `paint_text` offsetting the draw by the node's own
   padding while the size keeps including it, which is the same shape as the container case:
   `position_children` insets children by padding, so paint should inset a leaf's content by it
   too. Sequence it with item 17's wrap follow-up, since both change what `paint_text` hands
   `draw_line`.
15. **One spelling for the geometry properties.** `border_width = 1` is accepted and `padding = 10`
   is not, though both reach `parse_edge_insets`: only `border_width` has the scalar shorthand in
   front of it (item 6's second commit added it there and nowhere else). A config author who learns
   the shorthand on one property finds it missing on the two that use it most. Give `margin` and
   `padding` the same scalar form, which also means moving the range check `parse_border_width`
   applies per edge to a decision that covers all three rather than one.

   While there: `height = "Content"` is rejected by a message that lists a number, `"Fill"` and
   `"NN%"` and never says content sizing is spelled by omitting the property. That is the one size
   mode with no spelling, so it is the one a config author will guess at.
16. **A JSON `null` in a snapshot must reach Lua as `nil`.** `Loader::to_lua_value` goes through
   mlua's serde bridge, which maps `Value::Null` to a lightuserdata sentinel rather than `nil`. Live
   against a real tray, Telegram's item arrived with `icon_path = userdata: (nil)`, which is truthy:
   `if item.icon_path then` takes the branch that assumes a path and then concatenates a userdata.
   Every optional field of every capability payload has this shape, so the first config to read one
   gets it wrong, and gets it wrong silently.

   `serialize_none_to_null(false)` and its unit-variant sibling are the switch. Mapping to `nil`
   also erases the key from the table, which is the semantics a config wants and the one every
   `x or default` idiom in Lua already assumes.
17. **Clip a node's paint to its own box.** `TextPainter::draw_line` calls `fill_text` with no
   scissor, so a `text` whose content is wider than the rect layout gave it paints straight over
   whatever sits to its right. Seen on a real bar: the media cell's title ran through the two cells
   after it, and both were legible through each other.

   Item 10 is the reason the overflow was that large here, but fixing the font does not close this.
   A `text` bound to a capability is exactly the case where content length is not the config's to
   control: an MPRIS title is whatever the player reports, and no font choice makes an arbitrary
   string fit a fixed cell. `femtovg::Canvas` has `scissor`/`reset_scissor`, so this is a clip
   around each node's draw rather than new machinery.

   Clipping is the floor, not the finished behavior. § 3.2 gives `text` a wrap at the available
   width, which layout already measures with (`text_wrap_width`), so the honest fix is for paint to
   render the same wrapped lines layout measured rather than one unwrapped line that gets cut off.
   Ellipsis is a further step and wants a spec decision first, since § 5.2 names no overflow
   property today.

   **Amendment, on landing.** `paint_node` wraps both a node's own draw and its recursion into
   children in `Canvas::save` / `intersect_scissor` / `restore`, clipped to
   `snap_to_physical(rect, scale)`. `intersect_scissor` rather than `scissor` is what makes
   nesting compose: `save`/`restore` push and pop the whole femtovg `State`, scissor included, so
   a child intersects against every ancestor's clip and can only shrink the region further. The
   clip is snapped the same way `draw_line` snaps its glyph origin, so the clip edge and the
   glyph's physical placement agree; `snap_to_physical` grows outward (floor the top-left, ceil
   the bottom-right), so it never shaves a pixel a box filled at a fractional coordinate.
   `text/atlas.rs` is untouched: `draw_line`'s other caller, `wayland::mod`'s
   `draw_main_bar_proof_text`, has nothing to do with the tree walk, and the walk is the thing
   that knows about node boxes.

   Two things this does not do, both now recorded in the code as `ponytail:` comments. The clip is
   rectangular, so a node with a `radius` clips an overflowing child to square corners while its
   own background underneath is rounded. femtovg has `intersect_rounded_scissor`, but its own doc
   comment gives exact rounded corners only when it is the first active scissor or the previous
   clip contains it, which is false for anything nested; shipping corners that are exact at the
   root and silently square below it is worse than square everywhere.

   The wrap follow-up above is more work than "paint renders the wrapped lines" implies, and this
   slice found why. `layout::scene::intrinsic_content_size` does measure a `Content`-sized text
   box against the wrap width, passing it to `ShapingHandle::shape` as `max_width`, but
   `ShapeResult` returns only a bounding `width`/`height`, never the line breaks that produced
   them. Paint has nothing to render even if it wanted to. The follow-up therefore starts at
   `ShapeResult` carrying its lines across the shaping worker's channel, not at `paint_text`.

   One risk checked and found absent. A tight clip could shave the tail of a `Content`-sized
   `text`, since layout sizes that box from cosmic-text while femtovg paints with its own
   advances, and item 10's divergence test only pins the two within 2%. Measured on the current
   chain (single-face Noto Sans, no fallback triggered) with an unclipped scratch render scanning
   past the measured edge: the two measurements agreed to within 0.0001px on a 53-character
   string at 32px, and the last lit pixel sat 3 to 4 physical pixels inside the box in every case
   tried. It stays the correct outcome if some future face renders wider than it measures, since
   the alternative is the overrun landing on the neighbour.

   Dormant in production until Phase 20 item 4. `paint_tree` still has no caller anywhere outside
   its own test module, for the id-space reason its doc comment records, so the clip is exercised
   by the headless EGL tests and nothing else. Both new tests were proved by removing the
   `save`/`intersect_scissor`/`restore` block and rerunning: the text escaped to `(64, 64, 255)`
   at (59, 7) against a blue surface, and the oversized child painted its own green at (50, 50)
   where the surface's magenta belongs.

Deliberately deferred, and this one is a decision rather than an omission: **no damage tracking.**
Redraw the whole surface. `oblisk-layout-engine-geometry.md` § 5 projects damage rectangles, but
ashell ships a working bar presenting the full viewport every frame with no `wl_surface::damage`
call anywhere, and reports no cost from it. Partial damage is a real optimization with a real
bookkeeping burden; build it when a profile says the full redraw is the problem, not before.

Also deferred: `icon`, which needs a spec conflict settled first. `oblisk-idl-api-specs.md` § 5.2
item 5 gives `icon` a `name` property holding a theme name (`"audio-volume-high"`), implying the
Renderer resolves it. `oblisk-supervisor-services-dbus.md` § 9.2 puts an off-thread XDG desktop and
icon-theme resolver in the Supervisor, with an LRU cache, exposed to Lua as
`system:find_icon(app_id, name, fallback)` returning an absolute path. Building both means two icon
resolvers.

Settle it toward the Supervisor's, which is already specified, already required to be off-thread,
and already has the cache; the tray and notification controllers also already spool decoded icons
to `/dev/shm` by path (`dbus/shm_icons.rs`), so a path-taking `icon` node has two producers waiting
and a name-taking one has none. Drawing an image from a path is the easy half either way. Note that
neither resolver exists yet, so nothing has to be unbuilt. ashell's dependency on the
`freedesktop-icons` crate is the reference for whichever side ends up owning it.

> **Settled in Phase 29, the other way.** docs/adr/0054 puts the resolver in the Renderer, not the
> Supervisor. The recommendation above missed that § 3.2's own row calls `system:find_icon` a
> synchronous lookup returning a path, and the control socket carries one-way commands and one-way
> snapshots with no request/response shape to make that true over. `freedesktop-icons` was the right
> reference, on the wrong side of the process boundary.

Also deferred: Lua-authored state (ADR-0044 decision 5). Live signals are read-only to Lua, so a
config cannot hold reactive state of its own and "is this dropdown open" has nowhere to live.
`state(name, initial)` returns a writable `Signal` that marks dirty through item 2's flag, and the
name is what makes it survive an in-place reload. Phase 22 is the first thing that cannot work
without it.

> **Built in Phase 21, one phase earlier than this predicted.** Phase 21 is the first thing that
> cannot work without it: without a writable signal, nothing a `button`'s `on_click` does marks the
> scene dirty, so no handler is observable. See Phase 21's own note.

Testing: `oblisk-tdd-test-harness.md` § 3.1's headless EGL harness is written for exactly this,
using `EGL_PLATFORM_SURFACELESS_MESA` to render off-screen and assert pixel values (a `rect` with
background `#FF0000` writes red). It has never been built. Note two constraints before starting:
`renderer` is a binary-only crate with no `src/lib.rs`, so this lives in an inline `#[cfg(test)]`
module rather than the `renderer/tests/test_headless_renderer.rs` path the harness doc names (the
same conclusion ADR-0021 reached for its own tests), and a surfaceless context still needs a working
driver, so the test needs gating rather than being assumed to run in every environment.

### Phase 20: Lua-Declared Surfaces

Implement ADR-0038. Delete `SurfaceRole` and the three `create_*` calls; drive surface creation,
reconfiguration, and destruction from the evaluated topology instead.

1. **Build the surface set from the evaluation, at generation startup.** `Scene` keys top-level
   surfaces by `id` in a `HashMap<String, RetainedNode>` (ADR-0023) and
   `layout::node::SurfaceTopology` already parses `id`/`layer`/`anchor`/`monitor`. Create one layer
   surface per declared `(surface, output)` pair after the first evaluation, replacing the three
   `create_*` calls that run before it today.

   Do not add in-place surface creation or destruction. Adding or removing a surface, or changing
   its layer/anchor/monitor/namespace, is a topology change and stays the generation swap ADR-0001
   routes it to; the candidate builds its own set the same way. Within a live generation only two
   things move: `visible` maps and unmaps a surface, and the fields layer-shell permits changing on
   a live surface (`margin`, exclusive zone, `keyboard_interactivity`, size) apply in place.

   That rule is about `panel`, which is all this phase builds. `popup` and `window` cannot follow it,
   because `xdg_popup` needs its grab before mapping and its positioner is consumed at `get_popup`
   time; for those two roles `visible` creates and destroys the Wayland object (ADR-0049). Phase 22
   owns that, and the distinction is noted here so this item is not later read as forbidding it.

   > Built, and three things the plan did not anticipate, all three found against a live niri
   > session with `WAYLAND_DEBUG=1` rather than reasoned out.
   >
   > **A commit with no buffer attached is how a client *re-maps*.** The XML says so directly, and
   > the consequence runs backwards through the whole file: an unmapped surface must never be
   > committed for any other reason. `apply_exclusive_zone` used to commit for itself, which would
   > have silently re-mapped every surface the config had just hidden. It now stages only, and the
   > commit that carries it is whichever one the caller was going to make anyway -- `swap_buffers`
   > on a mapped surface, the null-buffer commit on a staging Candidate, or nothing at all on an
   > unmapped one. That is also why "stage the whole update, commit once" is a correctness rule
   > here and not a tidiness preference.
   >
   > **Mapping a panel that was declared `visible = false` gets no configure back.** The protocol's
   > re-map procedure ("commit without any buffer attached, waiting for a configure event") applies
   > to a surface whose state was *reset* by an unmap. A panel that started invisible was never
   > mapped: it made its initial commit, was configured, and was acked, and simply never attached a
   > buffer. The compositor has nothing new to say, so waiting for a second configure waits
   > forever. The two cases share one request sequence and end in different states, told apart by
   > whether the surface has ever been bound. Measured both ways; the reset case does get its
   > configure.
   >
   > **An in-place reload never reached the screen on its own.** `handle_apply_pending` applied the
   > new scene and stopped there, and `wayland::run`'s poll loop only repaints when
   > `re_resolve_if_dirty` reports a change, so an edit sat in memory until some unrelated
   > capability push happened to mark the flag. A live session hides this (a push lands every few
   > seconds); a static config would not have. Predates this item and only surfaced because
   > `visible` made a reload's effect binary rather than a few pixels. Fixed by marking the same
   > ADR-0044 decision 2 flag, at the cost of one redundant `Scene::apply` per file save.
2. **One Wayland surface per targeted output.** Generalize the `"{id}@{output}"` surface-id
   convention `supervisor/src/reload.rs` already carries through the PBA handshake for wallpaper.
   `OutputHandler` is already implemented, so monitor hotplug adds and removes surface instances for
   any surface targeting `"All"`.

   > Built. Three things the plan did not anticipate, two of which were latent bugs the hotplug
   > path would have walked straight into.
   >
   > **`zwlr_layer_surface_v1::closed` set `self.exit`.** The compositor sends that event when the
   > output a surface is on is destroyed, so unplugging one external display would have killed a
   > shell still painting on the laptop panel -- the exact opposite of decision 3's "in place, with
   > no generation swap". It destroys that one surface now. Defensible while one hardcoded bar was
   > the only surface; wrong the moment a config declares N of them across M monitors.
   >
   > **Dropping a `TrackedSurface` frees neither the EGL surface nor, in the right order, the
   > Wayland ones.** `khronos_egl::Surface` is a plain copyable handle with no `Drop`, so
   > `eglDestroySurface` has to be called by hand or every unplugged monitor leaks one; and
   > `TrackedSurface` declares `layer` before `bound`, so a plain drop would destroy the
   > `wl_surface` out from under the `wl_egl_window` still pointing at it. `destroy_surface_by_id`
   > does all three explicitly, outermost first. Reading `egl_surface` also took its
   > `#[allow(dead_code)]` off: the workspace total drops from 12 to 11.
   >
   > **`smithay_client_toolkit` calls `output_destroyed` *before* removing the output from its own
   > `OutputState`.** A plain read of `outputs()` from inside that callback still lists the monitor
   > that just went away, so it is excluded by identity through an explicit `departing` argument.
   >
   > No live coverage: this machine has one output. The add/remove/retain decision is a pure
   > function (`layout::instance::reconcile_instances`) and is tested directly, including the
   > all-outputs-gone case; everything downstream of it -- surface creation on a new monitor,
   > teardown on a departing one -- has never run against a compositor.
3. **Three new `surface` properties**: `namespace` (the layer-shell namespace compositor rules match
   on, hardcoded per role today), `keyboard_interactivity` (`"None"`/`"OnDemand"`/`"Exclusive"`,
   without which a launcher cannot take typing), and `margin` (anchor offsets, which padding cannot
   express because padding is inside the surface). All three are in `oblisk-idl-api-specs.md` § 6.1
   as of ADR-0038.
4. **Move the default surfaces into Lua.** `main_bar`, `overlay_canvas`, and `wallpaper_layer` stop
   being Rust constants and become declarations in the shipped default config. `dev-config/oblisk/shell.lua`
   currently declares `surface { id = "bar", layer = "Overlay" }` and is ignored; the acceptance
   test is that editing that `layer` changes what the compositor stacks. Rename the constructor
   `surface` to `panel` here (ADR-0040): "surface" becomes the umbrella term for all four roles, and
   this phase is the last point where that rename is a one-line change.

   Delete `PLACEHOLDER_OUTPUT_SIZE` here too, and resolve each surface against its own
   `TrackedSurface::configured_size` (ADR-0023 item 6, ADR-0039 decision 4). This is where it
   belongs rather than in Phase 18: deleting `SurfaceRole` is what collapses the Lua `id` space and
   the Wayland `surface_id` space into one, and until they are one there is no surface a size
   lookup could hit.

   > Built, with four things this text did not anticipate.
   >
   > **A size of `0` is a protocol error unless both edges of that axis are anchored.** The XML is
   > explicit: "You must set your anchor to opposite edges in the dimensions you omit; not doing so
   > is a protocol error." Mapping `"Fill"` to `0` puts that under config control for the first
   > time, so `panel { anchor = { top = true }, height = "Fill" }` would kill the Wayland connection
   > and take the whole shell down with nothing on screen to say why. A pure `ambiguous_zero_axis`
   > check refuses that one surface and logs the axis instead. Both dev-config panels tripped it as
   > written.
   >
   > **The exclusive zone is derived on configure, not at creation.** § 6.1 gives `exclusive` as a
   > boolean but the protocol wants an integer. Deriving it at creation means guessing the size the
   > compositor will pick; deriving it from the configured size is one rule that is always right,
   > and layer-shell permits changing the zone on a live surface. A corner anchor gets `0`, since it
   > satisfies neither "exactly one vertical edge" nor its mirror.
   >
   > **The first apply resolves against the output's logical size.** § 15.2 forces evaluation before
   > binding, so nothing is configured yet, but `OutputInfo::logical_size` is already known from the
   > two roundtrips `run()` already does. That resolve is validation and is never painted: a
   > candidate null-buffers first, and non-candidate mode's first draw is on first configure, after
   > the real size has arrived.
   >
   > **`applied_topology` changed meaning, and it is now the more accurate one.** It was "the
   > topology that was successfully applied to the scene"; it is now "the topology this generation's
   > surfaces were built from", recorded on a successful evaluation. That is the question the diff
   > is actually asking once surfaces come from the evaluation. Under the old rule, a startup whose
   > apply failed would apply a later topology-changing edit *in place*, into surfaces the config no
   > longer describes.
   >
   > Phase 19 item 8 landed here too, and could not have landed anywhere else: deleting
   > `SurfaceRole` deletes `draw_main_bar_proof_text`'s only condition, so `paint_tree` had to
   > become the draw path in the same commit. Its shared-context assumption is verified rather than
   > assumed, by a headless test driving one `TextPainter` across two EGL surfaces.
5. **Push input regions.** `layout::overlay_input_regions` has been correct and tested since Phase
   12 with no caller (ADR-0023 item 5). Wire it per surface, not just for one overlay: it is a no-op
   for a tightly-sized bar and load-bearing for any surface larger than its visible content.

   > Built, and the prediction held on a live session: the bar's region came out `(0, 0, 1920, 32)`,
   > which is what the protocol default already is, while the notification area's came out
   > `(0, 0, 41, 32)` inside a 380x60 surface -- the rest of that surface passes clicks through. No
   > special case distinguishes them, which is the point. `overlay_input_regions` keeps its name
   > and loses its `#[allow(dead_code)]`; the workspace total drops from 13 to 12.
6. **`oblisk.screens`** (ADR-0041). Expose `OutputState`'s outputs to Lua as a reactive signal, so a
   config can loop over screens to declare per-monitor panels; this is what replaces Quickshell's
   `Variants`, since Lua already has `for`. It is the first Lua signal sourced in the Renderer rather
   than pushed by the Supervisor, and it stays out of `shared::CAPABILITIES`. An output change
   re-enters the existing reload path (re-evaluate, diff topology, report to the Supervisor), adding
   a trigger rather than a second reload mechanism.

   > Built, as a bare `screens` global rather than `oblisk.screens`: no `oblisk` namespace table
   > exists in this VM and every signal that does is a bare global, which
   > `register_rescue_signal`'s doc comment already records as a divergence from ADR-0022. One
   > signal is not a reason to build a namespace table; it moves with all of them when the full
   > `oblisk.*` tree is built.
   >
   > **The trigger could not be a `ReevaluateReport`.** `supervisor/src/main.rs`'s
   > `is_current_reload` drops any report whose sequence is not the one it most recently sent, so
   > a Renderer that fabricated a sequence would have its own report discarded as stale. The new
   > frame is `RendererFrame::RequestReload`, which asks the Supervisor to *start* a cycle; it
   > lands on the same `begin_reload` the watcher's own trigger now calls. The sequence stays
   > Supervisor-owned and the whole existing path is untouched, which is what decision 4 asks for.
   >
   > **The seed has to beat the evaluation, and the initial output burst beats both.** A config's
   > top-level `for _, screen in ipairs(screens:get())` runs during `run_startup_evaluation`, so
   > seeding afterwards would declare no per-monitor panels at all. The two roundtrips `run`
   > already does are what make the list available that early -- but they also *dispatch*
   > `OutputHandler`, so the handler fires before there is any evaluation to expand or surface to
   > reconcile. It updates the signal from there (which is the seed) and returns; a
   > `startup_complete` flag gates the rest. Without it every boot spent one whole redundant
   > evaluate/report/apply round trip on a `RequestReload` sent before the config had been read.
   >
   > **A retained instance must keep the size the compositor gave it**, not the output's logical
   > size a re-expansion re-seeds it with. A surface whose size did not change gets no further
   > `configure`, so handing back the re-expanded value would resolve a bar at full screen height
   > permanently. That is why `reconcile_instances` carries entries over from the current set
   > rather than returning `expand_instances`'s output directly.
   >
   > Verified live on one output: the dev config, copied and given a cell that loops over
   > `screens`, renders `eDP-1 1920x1200 @60.001Hz x1` -- the connector name, the logical size, the
   > millihertz-to-Hz division keeping its fraction, and the scale. The output-*change* half has no
   > live coverage on a single-display machine.

Deliberately deferred: xdg-shell toplevels and xdg-popup, both with reasoning recorded in ADR-0038.
ashell ships without popups and hand-rolls dropdown menus as ordinary layer surfaces with computed
anchors, which is the evidence that the layer-shell path is sufficient rather than merely tolerable.

### Phase 21: Input Routing

The last seam between a drawn tree and an interactive one. `button`'s `on_click` has been stored as
an opaque Lua value since ADR-0021 item 2 and has never been called.

1. **Pointer events to nodes.** Bind SCTK's pointer handling on the seat already bound for
   `wp-text-input-v3`, hit-test the resolved tree in reverse paint order, and invoke the matched
   `button`'s `on_click`. After Phase 18 the Lua closure is a direct call on the same thread, not a
   channel round trip.
2. **Per-surface keyboard focus.** Honor Phase 20's `keyboard_interactivity` and route key events to
   the focused surface.
3. **Real `textfield` focus attribution.** `wayland/mod.rs`'s `PLACEHOLDER_SECURE_SUBMIT_CAPABILITY`
   and `PLACEHOLDER_SECURE_SUBMIT_ACTION` constants exist because no per-`textfield` focus tracking
   does. Once a focused node is known, a completed `secure_submit` reads that node's own
   `{ capability, action }` table (§ 5.2 item 8) instead of reporting `"unknown"`.

   > **Built** (docs/adr/0050). Items 1, 2 and 3 across three commits, plus `state(name, initial)`,
   > which this phase turned out to need a phase earlier than the note below Phase 19 predicted.
   >
   > **Hit-testing returns the path, not the topmost node.** The only tree anyone writes is a
   > `button` whose child is a `text`, so "deepest node under the pointer wins" finds the `text`,
   > which has no handler, and no button ever fires. `layout::hit::hit_path` returns the whole
   > chain and each caller scans it from the deep end: `on_click` for the innermost `button`, focus
   > attribution for the innermost `textfield`. One traversal, two questions, which is why item 3's
   > `hit_under` returns both answers from one call.
   >
   > **`ResolvedNode::rect` is parent-relative**, which ADR-0050 got wrong before it was amended.
   > The point-to-node comparison needs no conversion, but the button's *absolute* rect -- what
   > `on_click` is handed, and what Phase 22's positioner wants -- only exists as the sum along the
   > path. `absolute_rect` is that sum, and it is a second reason the return type is the chain.
   >
   > **Containment gates descent, and that turns out to match paint exactly.** A node whose rect
   > misses the point is not entered and neither are its children, so a node's hittable region is
   > its intersection with every ancestor's rect. Phase 19 item 17's `intersect_scissor` chain
   > makes its painted region the same intersection. The two walks agree without either carrying a
   > clip rect, because they compute the same thing by different means.
   >
   > **A click is a press and a release on the same node.** Firing on press is shorter and removes
   > drag-off-to-cancel, which every toolkit a user has touched has. Identity is the
   > `(instance_id, rect)` pair, because `ResolvedNode` carries none that survives `to_resolved`;
   > a re-resolve that moves the button between press and release cancels the click, which is the
   > answer a real identity would give anyway.
   >
   > **`on_click` receives the button's rect.** ADR-0040 and ADR-0049 both say the anchor rect
   > comes from "the rect `on_click` returns", which read literally is impossible: the engine does
   > not know which `popup` a click was meant to open. The rect travels out to Lua as
   > `{ x, y, width, height }` and the config hands it to the popup. ADR-0050 decision 3 settles it.
   >
   > **`state(name, initial)` had to come first.** Live signals are read-only to Lua, so nothing an
   > `on_click` could do marked the scene dirty and no handler was observable at all. Item 1 shipped
   > an unconditional dirty mark after every handler and labelled it a stopgap; the next commit
   > built ADR-0044 decision 5 and deleted it. `SignalKind::State` is a fourth variant rather than a
   > reuse of `Live` because a `set` accepting `Live` would let a config overwrite the SSID the
   > Supervisor just pushed.
   >
   > **A submit with no focused destination now sends nothing.** The deleted placeholders addressed
   > a password to `"unknown"/"unknown"`, which no capability routes. `zwp_text_input_v3::Leave`
   > zeroizes too, which is the sharper half: no submit is ever coming for bytes left in
   > `secure_buffer` when a session ends, so leaving them means the next field's first submit
   > carries the previous field's characters to the next field's capability.
   >
   > **Item 2 binds `wl_keyboard` for `enter`/`leave` alone.** There is no `on_key` in § 5.2 and
   > ADR-0050 declines to invent one, so the four key callbacks are empty and say so. What focus
   > buys is the clearing rule: losing it clears the focused field and any armed click.
   >
   > Verified live on niri: three injected clicks incrementing a `state` counter with the rect
   > logged at `997,4 86x24`, matching where the button is drawn; drag-off-to-cancel in both
   > directions; an in-place reload triggered by a colour edit leaving the counter's value intact,
   > which is decision 5's reload rule; keyboard enter/leave with the right instance id against a
   > panel temporarily set to `OnDemand`; a malformed `secure_submit` taking focus with no
   > destination without killing the shell. Clicks go through a uinput absolute-axis device, since
   > xdotool cannot reach a native layer surface and libinput's acceleration defeats a relative one.
   >
   > **No live coverage: the end-to-end submit, either branch, and the `Leave` zeroize.** A
   > `textfield` paints nothing and a real `wp-text-input-v3` submit needs an IME, so the unit tests
   > are the whole evidence there.

Click-outside-to-dismiss for a `panel` stays unsolved: layer surfaces have no compositor-agnostic
grab, and Quickshell's `HyprlandFocusGrab` works through a Hyprland-specific extension. Popups do
not have this problem, because `xdg_popup.grab` is protocol-native (ADR-0040); that is a reason to
reach for a `popup` rather than a second `panel` when something needs to dismiss itself.

### Phase 22: The `window` and `popup` Roles

Implement ADR-0040's remaining two roles now that panels paint and take input. Both are
`smithay_client_toolkit::shell::xdg` work.

1. **`window`** (`xdg_toplevel`) via SCTK's `XdgShell::create_window`, `Window`, and
   `WindowHandler`. `WindowConfigure` carries the size, the decoration mode, and the state bitflags
   (`is_maximized`, `is_fullscreen`, `is_activated`, the `tiled_*` set) that layer-shell has no
   analogue for. The initial-commit discipline is identical to layer-shell's, so PBA's null-buffer
   staging needs no new branch. Ack through the wrapping `xdg_surface`, not the role object.
2. **`popup`** (`xdg_popup`) via `Popup`, `PopupHandler`, and `XdgPositioner`. Create with
   `Popup::from_surface(None, ...)` and then root it: `LayerSurface::get_popup` for a panel parent,
   `xdg_surface.get_popup` for a window parent, always before the popup's initial commit. Feed the
   positioner from the rect Phase 21's `on_click` returns.
3. **The grab**, which SCTK does not wrap. Call `popup.xdg_popup().grab(seat, serial)` through the
   raw-object escape hatch, the same pattern ADR-0009 established for `wp-text-input-v3`. Own the
   bookkeeping SCTK will not: grab only in response to a real input event, only before mapping, and
   destroy nested popups in reverse creation order. Treat `popup_done` arriving immediately as a
   denied grab, which the spec explicitly permits, not as an error.
4. **Decorations**: bind `zxdg_decoration_manager_v1` if present, request server-side, accept what
   the compositor grants. Do not build a client-side titlebar frame.
5. **`visible` creates and destroys, for these two roles only** (ADR-0049). A `popup` or `window`
   node's Wayland object exists only while shown, unlike a `panel`'s, which lives as long as the
   generation. Nothing new drives it: `on_click` writes named state, the write marks the scene
   dirty, and the re-resolve that reads `visible` as true is still running inside input dispatch, so
   the serial `grab` needs and the anchor rect are both in hand at creation time. Destroy nested
   popups in reverse creation order when a parent's `visible` goes false.

   This is also where the memory budget gets its cheapest win. Twenty declared popups that are never
   opened cost twenty retained nodes and zero surfaces, buffers, or EGL surfaces, which is what
   Quickshell buys with a `LazyLoader` primitive and Oblisk gets from the protocol constraint.

Deliberately deferred: `Popup::reposition` and the `Reactive` configure kind. Item 5 covers a
dropdown that opens under different buttons by creating a fresh popup per open; only an anchor that
moves while a popup is already open needs `reposition`, and nothing needs that yet.

> **Built** (docs/adr/0049, docs/adr/0051). Items 1 and 4 with the window half of item 5 in
> `2c80cfc`, items 2 and 3 with the popup half in `4019815`. `reposition` and the `Reactive`
> configure kind stayed unbuilt as written above.
>
> **Two things ADR-0049 left open had to be settled before item 2 would compile**, and ADR-0051
> records both. A `panel` parent is not one surface, since `monitor = "All"` expands it per output
> and `get_popup` takes exactly one parent, so a popup anchors to the instance the arming click
> landed on: a dropdown belongs to the click, not to the output set. And a compositor dismissal
> latches, because `popup_done` leaves the resolved tree still saying `visible = true` and a config
> with no `on_dismiss` would otherwise reopen a popup for the same click-outside to close, forever.
>
> **An unsized `window` root painted nothing.** § 6.2 gives a `window` no `width` or `height`, so
> the root fell to `parse_size_mode`'s `Content` default, and a `Content`-sized parent hands its
> children a zero budget: `child = column { width = "Fill" }` resolved to 0x0 and the window painted
> a fully transparent buffer. `Scene::apply_one_instance` now forces an unsized `window` root to
> `available` per axis, overriding the default only.
>
> **PBA's null-buffer staging needed no new branch, as item 1 predicted, but the readiness gate
> did.** `null_buffered` is only set from inside a configure, and a `window` declared
> `visible = false` has no `xdg_toplevel`, so no configure was ever coming and a Candidate whose
> every surface was such a window died on `ready_timeout`. The gate now asks whether each surface
> has staged *or does not exist*.
>
> **The grab cannot validate on wlroots, and that is not fixable here.**
> `wlr_seat_validate_pointer_grab_serial` demands a still-held button and the press serial, while
> docs/adr/0050 decision 2 defines a click as a press and a release, so `on_click` runs at button
> count 0. niri accepts the grab because smithay does not run that check. Item 3's "treat
> `popup_done` arriving immediately as a denied grab" is therefore load-bearing on sway and
> Hyprland rather than the corner case it reads as here. ADR-0051's second amendment records
> `on_press` as the upgrade path.
>
> **No live coverage: opening the dropdown.** The unit tests cover the latch state machine and the
> positioner derivation, and a real session has booted the popup's declaration, but nothing has yet
> clicked the button twice on screen to watch it close and reopen.

### Phase 23: The `lock` Role and Session Lock

Implement ADR-0042. The Renderer takes `ext_session_lock_v1` through SCTK's `SessionLockState` /
`SessionLock` / `SessionLockHandler`, creates one `ext_session_lock_surface_v1` per output, and
paints the config's `lock` node tree into them. Move SCTK's `session_lock` usage out of the
Supervisor, which keeps idle-notify and the decision to lock.

1. **Lock and surface lifecycle.** One surface per output, a new one for each output as it is
   advertised, and destroy on output removal. A second surface on one output is a `duplicate_output`
   error, and destroying a surface on a still-active output makes the compositor fall back to a
   solid color. Reuse ADR-0041's output tracking rather than adding a second source.
2. **`finished` is two events.** In response to `lock` it means the lock was denied, usually because
   another lock client holds it. Later, it means the compositor tore the lock down itself. Surface
   both through `oblisk.rescue`; never swallow either.
3. **Authentication composes what exists.** `textfield` with `secure_submit` fills
   `shared::SecureBuffer` (ADR-0005, ADR-0027), the buffer crosses the control socket, the
   Supervisor's re-exec'd PAM worker runs the conversation (ADR-0028), and on success the Renderer
   calls `unlock_and_destroy`. No new secure path.
4. **Never unlock except on successful authentication.** SCTK's `Drop` deliberately does not unlock,
   and nothing here may add a convenience path that does.
5. **Block generation swaps while locked.** Only one client may hold a lock, so a candidate cannot
   acquire one while the authoritative generation holds it. The Supervisor queues a topology-changing
   reload until unlock. In-place reloads still apply.

> **Built** (docs/adr/0052, and an amendment to docs/adr/0027). All five items, plus two decisions
> the phase could not avoid making and one it had to reverse.
>
> Item 3's "composes what exists" was the false premise. What existed did not work: `SecureBuffer`
> was written only from `zwp_text_input_v3::commit_string`, which a compositor sends only when an
> input method is bound, and `KeyboardHandler::press_key` was an empty stub. A password could not be
> typed, so a lock could not be left, and `ext-session-lock-v1` keeps a session locked on purpose
> when its client dies. A `secure_submit` field now reads `wl_keyboard` directly and the text-input
> binding is deleted, since `secure_submit` turned out to be its only consumer. docs/adr/0027 carries
> the amendment.
>
> Reversed from Phase 22: `lua::require_surface` rejected a `lock` at the root, which left § 6.4's
> "declaring it says what the lock screen looks like" with nowhere to write the declaration. It
> conflated where a declaration lives with when its Wayland object exists, two things docs/adr/0049
> had already separated for `window` and `popup`.
>
> Pulled forward from Phase 25: item 1's generic `CommandEnvelope` method, because a capability with
> no caller is dead code, and item 3 for exactly one name, because the roster seeds bare globals
> after `NODE_KINDS` and a bare `lock` signal would have overwritten § 6.4's constructor. The other
> ten stay bare until Phase 25 moves them together.
>
> Five review passes, each of which found at least one way to strand the session or bypass
> authentication, all fixed: a lock granted with no field to type into; a `TopologyChanged` in the
> window before `locked` reaping the lock holder; a stale `LockReport` from a reaped generation; two
> `lock` declarations sending two `get_lock_surface` for one output; an in-place reload deleting the
> password field mid-lock; a PAM outcome not bound to the lock it authenticated against, which
> released a *different* lock nobody had authenticated to; and a login password surviving a focus
> change to be submitted to another capability's action.
>
> **No live coverage: taking the lock.** Every gate here is a unit test plus a boot with the lock
> declared and never triggered. Nobody has typed a password into it. The first real trigger needs a
> VT escape hatch ready, because the failure mode is losing the session.
>
> Known and deliberate: one retained scene tree leaks per unplugged monitor (inert, the veto reads
> live instances); two PAM workers can overlap across a teardown and reacquisition, bounded by
> `PAM_EXCHANGE_TIMEOUT` and unable to apply more than one answer; a `lock()` in the one-hop window
> while a `Finished` is in flight is dropped rather than acquiring; and a `textfield` on a surface
> that never takes keyboard focus is now untypable rather than silently capturing another surface's
> keys.

### Phase 24: Memory Measurement Harness

Implement ADR-0043 decision 1, which the fonts and atlas work in Phase 19 is otherwise unverifiable
against. Read `/proc/[pid]/smaps_rollup` for the Supervisor and each live Renderer, sum PSS, record
per-Renderer USS, and sample the two-Renderer PBA handoff window as its own number. Report GPU memory
from DRM fdinfo's `drm-*-memory` fields rather than assuming `smaps` captures it.

No dependency on the paint pass; buildable at any point, and more useful before Phase 19 than after,
since it gives the font decision a before-and-after number.

> **Built.** `supervisor/src/memory.rs` reads `smaps_rollup` and `fdinfo`, `main.rs` samples from
> two call sites, and docs/adr/0043 carries the first reading. Three numbers, one log line.
>
> Two of this phase's own instructions were wrong about the machine they describe. `drm-*-memory` is
> not a field i915 exposes; the current `drm-usage-stats` naming is `drm-resident-<region>`, with
> `drm-memory-<region>` surviving only as an older amdgpu-era spelling the parser keeps as a
> fallback. And a DRM client holds many fds that each repeat the same byte counts under one
> `drm-client-id`, so the obvious per-fd sum triple-counts: reading niri, three fds each reported an
> identical 279968 KiB. The parser dedupes on `(drm-pdev, drm-client-id)` and reports the surviving
> client count so nobody has to trust that it did.
>
> Sampling is split rather than uniform (docs/adr/0043's sampling amendment). Steady state is behind
> `OBLISK_MEMORY_SAMPLE_SECS` because anyone can read the same file from outside a running shell.
> The handoff sample is unconditional because nobody outside the process can catch a window that
> exists only during a swap, and it is taken at one instant, after `run_pba` returns `Ok` and before
> the superseded generation is reaped, which is the widest point but not a tracked peak.
>
> **The reading, and the one it took a second experiment to get right.** 149.7 MiB total PSS on one
> monitor against a 50 MB budget, and 82.3 MiB of it is `libLLVM`, dragged in by Mesa's gallium
> megadriver on a machine whose Renderer reports `drm-driver: i915` and never asks for llvmpipe.
> Every mapped font together is 0.1 MiB.
>
> That font number nearly went into ADR-0043 as evidence against its own decision 2. It is the
> opposite: Phase 19 item 10 already landed, so the shipped path resolves a declared chain through
> `fc-match` and loads two files, and measuring it was measuring the fix. Pointing the harness at
> `system_fallback`, the last-resort path that still calls `load_system_fonts()`, gives the
> before-and-after this phase exists to produce: **2207.9 MiB of Renderer PSS against 137.0 MiB**,
> across this machine's 2648 faces. Decision 2 argued from a one-second startup cost. It is a 16x
> multiplier on the whole shell, and it also exceeds the 1.1 GB of fonts on disk, which means
> `fontdb` reads those files rather than mapping them as the ADR assumed.
>
> The handoff came in at 196.9 MiB, 1.32x steady state rather than 2x, with per-Renderer USS falling
> from 130.4 MiB to 42.6 MiB as the second process mapped the same clean Mesa pages. That is
> decision 1's whole argument for PSS over RSS, measured: an RSS sum would have said 274 MiB.
>
> Left for Phase 19 to weigh, not fixed here: `system_fallback` is shipped code that costs 2.2 GB on
> any machine without fontconfig, and its own comment calls that "slow".
>
> Not measured: femtovg's atlas growth (decision 3), a leak over days that a sample taken seconds
> after boot cannot see, and anything on a second monitor or a non-i915 driver. The Supervisor
> cannot divide by monitor count either, since `screens` is Renderer-sourced (ADR-0041), so the
> per-monitor budget is arithmetic the reader does.

### Phase 25: The Lua Write Path and the `oblisk` Namespace

Everything above is the read direction. Lua still cannot write. `Signal` userdata exposes `get` and
`map` and nothing else, so § 3.2's roughly thirty write commands and § 7.1's "intercepts all method
invocations on exported singletons" have no implementation and, until now, no phase. Phase 9
deferred the dispatch routing table and nothing claimed it back.

Both ends already exist. `shared::CommandEnvelope` is the wire type, the Supervisor routes per
module through ADR-0037's `dispatch(controller, envelope)`, and `lua/process.rs` builds a real
envelope from a real Lua call. `process.run` is the working template, not a special case.

1. **A generic capability method.** Give the `Signal` userdata (or a sibling handle registered under
   the same name) a method that builds a `CommandEnvelope` from `{capability, action, arguments}`
   and queues it on the outbound sender, exactly as `ProcessRegistry::send` does. One implementation
   covers all thirty commands; § 3.2's table is validation detail, not thirty code paths.
2. **Track the revision.** `apply_state_snapshot` reads `snapshot.capability` and `snapshot.payload`
   and drops `snapshot.revision`. § 7.3's guard rule compares `expected_revision` against the
   Supervisor's ledger, so a write needs the revision the read arrived with, and the Renderer
   currently keeps no such number. Store it alongside the `LiveSignalHandle` and stamp it. This is
   why `process.rs` hardcodes `expected_revision: 0`, which is correct only because `process` holds
   no state to be stale about.
3. **The `oblisk` namespace.** Globals are bare today (`audio`, `network`, `rescue`), and § 2
   specifies `oblisk.audio` throughout. `socket.rs` admits the divergence in a comment and calls it
   ADR-0022's ad-hoc precedent. Register one `oblisk` table and hang the roster off it. Every doc
   example in § 2 is wrong until this lands, which makes it the cheapest correctness win in the
   playbook.
4. **`oblisk.version`**, a `{ major, minor, patch }` table, registered on the same table item 3
   builds. Marginal cost here is zero and it is hostile to retrofit: a config written before any
   version exists has nothing to guard on, forever. Take Quickshell's idea and not its shape.
   `Quickshell.hasVersion(major, minor, features)` carries a feature-name list, which is the answer
   to a problem Oblisk does not have yet; a table a config can compare is the same guard without a
   registry of feature names to maintain.

Deliberately deferred: per-command argument validation from § 3.2's table. The envelope carries
`arguments` as JSON and each capability's `dispatch` already parses what it needs, so validating
twice means maintaining the schema twice. Reject at the module that owns the command.

> **Built.** Item 1 had already landed under ADR-0052 decision 1; items 2, 3 and 4 close the phase.
>
> Item 3 is the one with reach. Every roster name now hangs off one `oblisk` table, along with
> `rescue` (§ 2.10) and `screens` (§ 2.15), and no capability is left as a bare global. A test
> asserts both halves, because `set_global` never removes anything: a leftover bare seed would keep
> working, and every config written against it would keep working, until the day that name collided
> with a node constructor the way `lock` did.
>
> **What the rename nearly broke, and what caught it.** A capability is a `Capability` userdata now,
> not a bare `Signal`, and the engine decided "is this a signal?" by userdata type in three separate
> places. `computed({oblisk.audio}, f)` and `width = oblisk.sysinfo` would have stopped resolving,
> and a property the resolver skips is treated as a *literal*, so every live binding in every config
> would have frozen at frame one with nothing logged. That is the same failure mode the `%d` raise
> produced in Phase 28, reached by a different route. The three sites now go through one
> `signal::from_userdata`, with `is_signal` beside it for the callers that only need the question
> answered, and a test asserts the two agree on every type rather than trusting them to stay in step.
>
> Item 2 stores each capability's `StateSnapshot.revision` next to its value and stamps it onto
> every envelope. `CapabilityHandle::hydrate` writes both or neither, which is the point: a `set`
> that missed its revision bump would stamp the previous read onto a write reacting to the current
> one, which is exactly the race § 7.3 exists to drop. `0` is not a revision any push can produce
> (`bump_revision` starts at 1), so it means "never hydrated" and nothing else.
>
> **Nothing reads that number yet, and item 2 never asked anything to.** The Supervisor's
> `RendererFrame::Command` arm matches on `capability` and dispatches; it checks neither
> `expected_revision` nor `generation_id`. So § 7.3's guard rule ("if the `generation_id` is less
> than the current active generation, or if the `expected_revision` is stale, the Supervisor
> instantly drops the packet") is half-built: the Renderer now sends honest numbers and the
> Supervisor ignores both of them. That is the right half to build first, since a guard cannot be
> written against a field that is always `0`, but a write from a superseded generation is accepted
> today and § 7.3 says it must not be. It needs a phase.
>
> Item 4 is `oblisk.version`, three integers from Cargo's own `CARGO_PKG_VERSION_*`. It parses with
> an `expect` rather than falling back to zero: a version table that quietly reads `0.0.0` is worse
> than not booting, since a config would guard on it and take the wrong branch forever.
>
> **Still divergent from § 2, and now visibly so.** § 1.2 writes `content = oblisk.mpris.title`,
> one signal per field. This engine has one signal per capability holding a table, so `oblisk.mpris`
> is live and `oblisk.mpris.title` reads `nil` on a userdata with no such field, which a property
> treats as absent and renders as the documented default. The name is right now and the shape is
> not, which is a smaller gap than before this phase and a more confusing one: the example looks
> like it should work. Settle it by picking one, in the phase that next touches the read path.

### Phase 26: The Config Environment

Implement ADR-0047 and ADR-0048. Both change what a config *is* rather than what it can draw, and
both are small enough that splitting them buys nothing.

1. **`package.path` points at the config directory** (ADR-0047 decision 1), replacing the default
   rather than prepending to it, so `require "widgets.clock"` resolves inside the config and never
   picks up a same-named system module. C modules need no work: mlua's safe mode already replaces the
   C searchers and makes `package.loadlib` raise.
2. **Clear `package.loaded` before every re-evaluation** (ADR-0047 decision 2). ADR-0044 decision 4
   keeps the VM alive across an in-place reload and `require` caches by module name, so without this
   an edit to a required module re-runs `shell.lua` against the stale cached copy and changes
   nothing. It looks exactly like a reload that silently did not happen, which is why it is worth its
   own item.
3. **Watch the tree, hash the contents** (ADR-0047 decision 3). `supervisor/src/watcher.rs`'s single
   non-recursive `add()` becomes a recursive walk over the config directory. Keep a `path -> hash`
   map and drop any event whose `.lua` file hashes the same as last time. The existing debounce
   stays: it collapses one save's event burst, while the hash rejects saves that changed no bytes.
4. **Cut the stdlib** (ADR-0048). `Loader::new` calls `Lua::new_with` with an explicit `StdLib` set
   instead of `Lua::new`'s `ALL_SAFE`, dropping `IO` and `OS`, then re-registers `os.time`,
   `os.date`, `os.clock`, and `os.getenv`. After ADR-0039 every blocking stdlib call stalls Wayland
   dispatch, and ADR-0021's 5ms cap cannot catch it: the cap is an instruction-count hook, and a
   thread parked in a syscall executes no instructions.

Acceptance: a config split across `shell.lua` and `widgets/clock.lua` reloads when either file
changes, and `os.execute` is `nil`.

> **Built, all four items.** `dev-config/oblisk` is a directory of 32 files rather than one, which is
> this phase's acceptance shape against the config the repo ships rather than a fixture. `shell.lua`
> is 35 lines: its header comment and a list of six surfaces, each one a `require`.
>
> The layout mirrors the Quickshell config this shell is written to replace, because that config is
> the workload the gap ledger measures against and its 160 files have already answered how to
> organise this: `config/` for design tokens, `components/` for dumb reusable widgets, `lib/` for
> functions with no node in them, and `modules/` grouped by the surface they appear on
> (`modules/bar/indicators/`, `modules/bar/panels/`, `modules/global/`). The one deliberate break is
> that there is no `services/`. Quickshell needs 25 singleton `*Service.qml` files because each owns
> a D-Bus connection, a poll loop or a socket; here every one of those is a capability the Supervisor
> owns and pushes as a signal, so a module reads `oblisk.audio` rather than constructing an
> `AudioService`. The data layer is not missing from the tree, it is not the config's job.
>
> The split was checked against a structural signature of the node tree the config builds, dumped
> under stubbed engine globals before the first move: a pure refactor changes it by zero bytes across
> all 80 lines, and it did. Worth naming what that oracle does not catch, since it was nearly caught
> out once: it compares structure, not semantics, so rewriting `util.label(signal, read)` as a bare
> `signal:map(read)` passes it while dropping the `pcall` that keeps one malformed payload from
> failing the whole re-resolve.
>
> **§ 1 of the IDL described this phase in the present tense before it was true.** It has said "`io`
> is absent and `os` is cut to `time`, `date`, `clock`, and `getenv`" and "`package.path` resolves
> inside the config directory only" for as long as those ADRs have existed. Measured in the real VM
> before this phase landed: `io.open("/etc/hostname")` returned a working handle, `io.popen`,
> `os.execute` and `os.remove` were all present, and `package.path` was Lua's compiled-in default
> ending in `./?.lua`. The paragraph is accurate now. It is recorded here because a spec that states
> an intention as a fact is the failure mode this file's other banners exist to catch, and this one
> went unnoticed through four phases.
>
> **`require` had a worse failure than "does not work".** The default `package.path` ends in
> `./?.lua`, which resolves against the process's working directory, and nothing in the Supervisor
> sets one. A split config therefore worked when the stack was started from inside the config
> directory and failed from anywhere else, which is the shape that passes every test run by hand and
> breaks under a systemd unit.
>
> **Three defects in item 3's recursive walk, all found in review and fixed here.** Each was created
> by the walk itself, so none existed before this phase.
>
> The walk propagated `read_dir` errors, and `spawn_watcher`'s one caller passes them straight out
> through `?`, so a single unreadable subdirectory anywhere under the config directory would have
> stopped the Supervisor from starting. Before the walk existed, one non-recursive watch ignored that
> directory entirely. Startup now logs and skips it, agreeing with the `ISDIR` arm that already took
> the tolerant view for a directory appearing later.
>
> The walk used `read_dir`'s own `file_type`, which reads the directory entry and so calls a
> symlinked directory a symlink rather than a directory. `widgets -> ~/dotfiles/oblisk/widgets` is
> what a dotfiles repository produces, and `require` resolves through it because Lua opens the file
> and the kernel follows the link. The config would have loaded and never reloaded, which looks like
> the watcher working. Now `metadata` follows the link, with a visited set of canonical paths cutting
> the cycle that following links opens up.
>
> A rename is not a delete, and only the delete was handled. Moving a directory out of the tree sends
> one `MOVED_FROM` for the directory and no `DELETE` for anything inside it, so its watches stayed
> live on an inode that had left the config, and its files' hashes stayed keyed on paths that no
> longer existed. Recreating that path with the same bytes then matched a hash recorded against the
> old directory and the reload was suppressed for a file the watch had never seen. `rm -rf` does not
> hit this because it sends a `DELETE` per file; `git stash` and `git checkout` do.
>
> **A limit `package.path` cannot express, recorded rather than fixed.** It is a plain Lua string
> with no escape syntax, so a config directory containing `;` reads as two search entries and one
> containing `?` has every `?` replaced by the module name. Both are silent. Rejecting such a
> directory at startup is the only real fix and is not worth building for a path that is
> `$XDG_CONFIG_HOME/oblisk`.
>
> **Run live, and it found a defect no test could have.** `XDG_CONFIG_HOME=dev-config
> target/debug/supervisor` against a real niri session resolves all six surfaces and brings up the
> three visible panels with EGL contexts (`wallpaper` 1920x1200, `bar` 1920x34, `notification_area`
> 380x96), with `settings` and `click_menu` correctly not visible and `lock_screen` declared but
> never mapped.
>
> The first run did not. It reported `shell.lua failed to evaluate: error converting Lua string to
> table` and painted nothing. The cause is a Lua 5.4 change: `require` returns *two* values, the
> module and the loader data (its file path), where 5.3 returned one, and a call in the last position
> of a table constructor expands to all of its values. So `return { require(a), require(b) }`, the
> obvious entry point for a split config, is a *three* element list whose last element is a string.
> Nothing else would have caught it. `luac -p` sees valid syntax, the tree-signature oracle skipped
> the stray element as a non-table (fixed: it is a hard error there now), and no unit test had ever
> built a surface list through `require`. Running it was the only way.
>
> Two fixes came out of that. `shell.lua` binds each `require` to a local first, with a comment
> saying why, because the bug is invisible when reading the file. And `collect_surfaces` no longer
> lets mlua convert the elements: it type-checks each one and reports `surface 2 is a string, not a
> node` as an `InvalidTopLevelReturn` rather than an `Eval`, since the config evaluated fine and
> returned the wrong thing, with the `require` cause named because a config author cannot see it by
> reading their own file.
>
> Unit tests cover all four items and all three watcher defects, including a re-evaluation seeing an
> edited required module rather than the cached one, a `.lua` write in a subdirectory firing the
> watcher, an identical rewrite firing nothing, `require` resolving the shipped `config/theme.lua`
> and `components/pill.lua` (the second of which requires a module of its own, so it also pins
> transitive resolution), and a config that deletes `package` failing its next reload loudly.
>
> **The reload itself now runs.** Against a live session, editing `BG` in `config/theme.lua` painted
> the bar dark red and reverting it painted the bar back, with no restart between them: three scene
> applications of six surfaces each, one at startup and one per edit. That is the one check that
> exercises the watcher and the loader together, and both halves of ADR-0047 needed it. Neither
> `package.path` resolving `config.theme` inside the config directory nor the dropped
> `package.loaded` had ever run outside a unit test.
>
> The log alone does not prove it, which is worth recording because the obvious automated check is
> the weak one. Counting applied scenes cannot attribute them: the wallpaper went from `1200.0` to
> `1166.0` between the first batch and the second, which is the bar's 34px exclusive zone landing,
> and that reconfigure drives a re-resolve on its own. Three batches for two edits is equally
> consistent with one edit doing nothing and the compositor supplying the extra. Only the colour on
> the glass separates those, and only a human watching the screen saw it.
>
> One limit worth recording, met while trying to test deeper: a bare `Loader` cannot `require` an
> indicator, because an indicator reads `oblisk.audio` at require time and the `oblisk` table is
> built by `RendererClient`, not by `Loader`. Tests reach as deep as `components/`, which is every
> module that does not touch a capability.

### Phase 27: Out-of-Band Rescue

Implement ADR-0046. `oblisk.rescue` is a Lua signal the config reads and renders, which works only
while the config works. A startup evaluation failure leaves no tree to render through, and ADR-0024
item 4 records the result: the shell stays blank. The failure that most needs an error message is the
one that cannot produce one.

1. **Split the two failures.** A reload failure keeps a working scene under ADR-0024's rollback
   guarantee, so the running config renders its own banner through `oblisk.rescue`, unchanged. A
   startup failure with no prior scene is what gets the process.
2. **Re-exec, following ADR-0028's PAM worker.** The Supervisor re-runs its own binary with a flag
   and the error text. No second binary to install and no code path the failed config can influence.
3. **One `Overlay` layer surface per output**, drawing the error text, the file and line `mlua::Error`
   already carries, and the path it tried to load. Hardcoded Rust, no Lua VM, no capability
   connections.
4. **It is not a generation.** No generation id, no dependency snapshots, no PBA handshake, no
   authority over any output. The Supervisor reaps it the moment a real generation reaches
   presentation evidence.

No `inhibitReloadPopup` equivalent is needed. Quickshell has one because its popup also spawns on
reload failures, which is the case item 1 hands back to `oblisk.rescue`.

### Phase 28: The Capabilities § 2 Specified and No Phase Claimed

Implement docs/adr/0053. Five § 2 capabilities were specified and never built, and no phase in this
document owned any of them: `battery` (§ 2.2), `brightness` (§ 2.3), `workspaces` (§ 2.9), `system`
(§ 2.11) and `power` (§ 2.13), plus four fifths of `audio` (§ 2.4). Phase 16 built "the capability
roster" scoped by `oblisk-supervisor-services-dbus.md`'s section numbers rather than the IDL's, and
the IDL-only entries fell between the two documents.

1. **`battery`** (§ 2.2), **`system`** (§ 2.11) and **`audio`'s `volume`/`muted`** (§ 2.4). Done, see
   the note below.
2. **`brightness`** (§ 2.3). `/sys/class/backlight`, and the same `inotify` watch line 98 of this
   document already justifies the dependency with. Small, and deliberately not bundled above: it has
   no consumer pressing for it the way the other three did. Built, see the second note below, and
   line 98's `inotify` prediction is wrong.
3. **`workspaces`** (§ 2.9). Not small and not obviously ours. `niri-ipc` is already a dependency, so
   one compositor is reachable cheaply, but whether this capability speaks one compositor's IPC or an
   abstraction over several is a design question that needs an ADR before an implementation. Do not
   let a bar's need for a workspace strip decide it. Built, see the third note below, and the ADR is
   docs/adr/0056.
4. **`power`** (§ 2.13). Profiles through `power-profiles-daemon`, `on_battery` and `energy_rate`
   through UPower, which the keyboard capability already talks to. Built, see the fourth note below,
   and half of it is unverified for a reason worth reading.
5. **`audio`'s `sinks`/`sources`** (§ 2.4) and per-app `volume`/`muted`. The per-app half needs the
   same `SPA_PARAM_Props` subscription per stream node that the master already has; the arrays need
   the sink and source globals tracked as well as the streams. Built, see the fifth note below.

> **Built (items 1 and part of 5).** `battery` reads `/sys/class/power_supply` behind a real udev
> monitor on `AsyncFd`, filtered to the one system battery: the mains adapter, the USB-C PD source
> and any `scope=Device` peripheral battery are all excluded, and `Not charging` is matched exactly
> rather than by substring, which a `contains` check would invert into charging. That monitor is the
> first caller `udev` has ever had. It has been a declared dependency since scaffolding, justified by
> line 98's "§ 1.1's battery netlink monitor", which nothing then wrote. Its `send` feature had to be
> enabled, and `AsyncFd::readable_mut` used rather than `readable`, because the shared-reference
> guard needs `Sync` and only `send` is on.
>
> `system` pushes `time` once per wall-clock second, aligned to the boundary, and only when the epoch
> second it would report actually changed (docs/adr/0053 decision 2). `state` loads
> `$XDG_STATE_HOME/oblisk/state.json` once and degrades to an empty object on missing, unreadable,
> malformed or non-object JSON. Nothing writes that file yet, which the ADR names rather than hides.
>
> `audio` moved to § 2.4's object shape, which forced its app fields to § 2.4's names at the same
> time (`node_id` to `id`, `app_name` to `name`). Master volume comes from the default sink's
> `SPA_PARAM_Props` param, resolved through the `default.audio.sink` metadata key, and is the cube
> root of the max of `channelVolumes`: PipeWire stores those cubed, so the raw value reads 3% where
> `wpctl` and the user both say 30%. Verified against `wpctl get-volume`, not against the header.
> Per-app `volume`/`muted` are placeholders and are marked as such.
>
> The rewritten `dev-config/oblisk/shell.lua` is what all of this was for, and it found two things
> the capability work did not. Its previous version read `sysinfo.cpu_pct`/`ram_pct`; the real keys
> are `cpu_percent`/`ram_percent`, so it would have shown 0% forever the day Phase 25 woke sysinfo's
> pollers, with a dormant capability and a typo looking identical until then. And a three-zone bar
> cannot be built the way a flexbox one is: `resolve_non_content` gives a `Fill` child the parent's
> whole budget rather than the remainder, so two `Fill` spacers both take the full width instead of
> splitting it. The bar uses three fixed percentage zones each distributing its own spare space by
> its own `align_h`. There is no space-between in this engine, and nothing said so before now.

> **Built (item 2).** `brightness` reads `/sys/class/backlight`, picking one device by the `type`
> attribute the kernel's own `Documentation/ABI/stable/sysfs-class-backlight` exposes for exactly
> this (`firmware`, then `platform`, then `raw`, tie-broken by name), skipping any device whose
> `max_brightness` is not positive. It reports the `brightness` attribute rather than
> `actual_brightness`: the two differ while a driver fade is in flight or when the hardware rounds a
> request, and a config that calls `set(50)` and reads back needs to see `50`.
>
> **Line 98 of this document is wrong about the mechanism.** It justified the `inotify` dependency
> with "§ 1.2's backlight watch", and inotify does not fire on a sysfs attribute write. `keyboard`
> had already found this for the LED-state files; the same was confirmed here with `udevadm monitor
> --udev --subsystem-match=backlight`, which shows a `change` uevent on every brightness change.
> The watch is the `AsyncFd` udev monitor `battery` already uses, with the same 30s poll fallback.
>
> **The write goes through logind, because the Supervisor cannot write the file.**
> `/sys/class/backlight/*/brightness` is root-owned `0644` and the Supervisor runs as the user, so
> a direct write needs a udev rule shipped with the shell. `login1.Session.SetBrightness(subsystem,
> name, brightness)` on the `session/auto` path needs nothing installed and was confirmed to
> succeed as this user. logind refuses it from a session that is not the seat's active one, which
> is logind correctly protecting a display that session does not own, so that failure is logged and
> not worked around.
>
> **No backlight device means no push, ever.** § 2.3 specifies `percent: integer [0, 100]` and no
> absence sentinel, unlike `battery.present` and `sysinfo.temp_gpu`'s `-1`. A fabricated `0` would
> read as "the screen is off" rather than "there is no backlight", so the signal stays `nil` and
> ADR-0037's nil-until-hydrated contract carries it. This is the gap the previous note flagged in
> `audio`, which has no way to say "unknown" and hid a bind failure behind a plausible number.
>
> Verified end to end against the live session, not against the header: a config calling
> `oblisk.brightness:invoke("set", 40)` moved `intel_backlight` to 7680, which is 40% of this
> panel's 19200 exactly. That is also the first § 3.2 command with an argument in it, so it is what
> proves Phase 25's arguments array and revision stamp reach the Supervisor intact.

> **Built (item 3).** `workspaces` speaks niri and only niri (docs/adr/0056 decision 1). The
> alternative was extending `keyboard`'s `CompositorLink`, and it loses on a specific point rather
> than on taste: that trait's Hyprland implementor is unverified by ADR-0034's own admission, and
> workspaces would have forced a second, much larger unverified Hyprland implementation in the same
> commit. Hyprland models one active workspace per monitor plus a globally focused monitor; niri
> models a per-output `is_active` and a single global `is_focused`. Those do not map onto each other
> by renaming fields, and writing that mapping with no machine to run it on is how the last three
> silently-wrong answers got shipped. A session that is not niri never pushes, and the signal stays
> `nil`, which is `brightness`'s no-backlight posture unchanged.
>
> **§ 2.9 is wrong in three places, and only building it showed that.** `focused_workspace` sits
> inside the per-output structure, and focus is one workspace across every output, so it is reported
> only on the output that holds it and absent elsewhere rather than repeated onto monitors that do
> not have focus. `active_client.is_fullscreen` has no source at all: niri-ipc 26.4.0's `Window`
> struct has no such field and its event stream never reports one, so the key is omitted rather than
> answered `false`, which would be wrong for exactly the windows a fullscreen check exists to find.
> And § 2.9 as specified cannot be drawn: two opaque workspace ids per output, and nothing saying
> which workspaces exist, what they are called or what order they sit in. Each output entry carries
> a `workspaces` array now, which is the same call docs/adr/0053 decision 3 made for `audio`.
>
> `class` is niri's `app_id`. A Wayland toplevel has no `WM_CLASS`, so the IDL's own example values
> are app ids in practice, and this is a rename rather than a match.
>
> This is the second niri event-stream connection in the process, `keyboard`'s being the first. Both
> replay niri's full startup state to a reader that discards most of it. That is the smaller cost:
> sharing one stream couples `keyboard` and `workspaces` lifetimes, in a codebase where every
> controller owns its own connection. The third consumer is the point where that stops being true,
> and the ponytail in `workspaces/controller.rs` says so.
>
> Verified live on this session with screenshots, both branches. With the bar's `OnDemand` keyboard
> interactivity holding focus, niri reports no focused toplevel and the config drew "no window",
> which is the `active_client = nil` case being correct rather than broken. With a non-focusing bar,
> it drew `kitty (float)` against a live `niri msg -j focused-window` reporting exactly that. The
> strip drew `1 2 [3] 4 5 6 7 8 9 10 stash 12`, which is also the proof that a named workspace and
> an `idx`-only one both render. `workspaces:focus(id)` moved the live session from workspace 3 to 4
> and it was put back.
>
> The workspace strip is one `text` cell, not a `list` of buttons, because `list` still lays out
> vertically only. That ponytail has a second consumer now and neither of its two upgrade paths got
> cheaper: a `direction` property invents API § 5.2 does not have, and a true repeater needs
> docs/adr/0045 amended first.
>
> One hazard found in `niri_ipc::state` and not fixed: its reducer panics, rather than degrading, on
> a `WindowClosed` or `WindowLayoutsChanged` naming a window it has not seen. Those are niri's own
> invariants and this reader cannot violate them from outside, so the panic would kill the reader
> thread and stop workspace updates for the rest of the run with only a stderr backtrace. Named in
> the code, not worked around.

> **Built (item 4).** `power` is the first capability whose fields go absent one at a time.
> § 2.13 names four, and they come from two unrelated daemons: `active_profile`/`profiles` from
> power-profiles-daemon, `on_battery`/`energy_rate` from UPower. Either can be missing while the
> other works, so every field is optional and an unanswerable one is omitted rather than filled in.
> `brightness`'s all-or-nothing rule was right for a capability with one source and is the wrong
> shape for one with two: a desktop with no profile daemon would have lost its mains reading too.
>
> **Power-profiles-daemon is not installed on this machine, and that half is therefore unverified.**
> `busctl --system list` shows UPower and no `PowerProfiles` under either name. The proxy is built
> to the project's documented D-Bus API, including the 0.20 rename from `net.hadess.PowerProfiles`
> to `org.freedesktop.UPower.PowerProfiles` (both are tried, newest first), and confirmed against
> nothing. Same posture and same admission as `keyboard`'s Hyprland implementor. The degrade path
> *is* verified, because this machine is the degrade path: the run logged "no power-profiles-daemon
> reachable" and the bar drew the two UPower fields alone.
>
> **The UPower half is verified.** `ac 0.0W` on the live bar, against a `busctl` reporting
> `OnBattery=false` and `EnergyRate=0` on the composite `DisplayDevice`. `on_battery` comes from
> UPower rather than from the sysfs `battery` already watches because it is the system-wide answer
> across every power supply, and a laptop docked with two adapters is where picking one `Mains`
> device by hand gets it wrong.
>
> **The bar ran out of room, and that is now a documented ceiling rather than a fourth trim.**
> Adding `power` to the battery pill pushed the right zone past its 768px and clipped `lock` off
> the edge, the same failure `brightness` caused once already. Shaving two text modules did not buy
> enough back. `notifications` left the bar (the `notification_area` surface already draws the same
> newest notification, so it was the one duplicated readout) and `brightness` moved to the left
> zone. Neither side can grow: the sides are equal because that is what makes the middle a centre,
> and the 20% centre's slack cannot be borrowed by a 40% side. From here every module added costs
> another module its place, until this engine has a real space-between.

> **Built (item 5).** § 2.4 is now reported in full. The prediction in the item above
> was right about both halves and wrong about how much each cost.
>
> Per-app `volume`/`muted` is the same `SPA_PARAM_Props` subscription the master sink already had,
> pointed at a stream node, and that turned out to be a `.param` callback added to the listener
> those nodes already carried rather than a second listener. `pw-cli enum-params <id> Props` against
> a live playback stream confirmed a stream publishes `channelVolumes` and `mute` exactly as a sink
> does, cubed the same way, so `master::extract_sink_props` and `master_volume_from_props` were
> reused unchanged. Verified live: a stream set to 0.42 with `wpctl` read back as `0.42` in the bar
> while its two neighbours read `1.00`.
>
> `sinks` needed only a display name beside the `node.name` already tracked, because § 2.4's `name`
> is the "user-friendly description" and the metadata keys route by the other spelling. Both exist
> on every device this machine advertises: `node.description` is "Built-in Audio Analog Stereo",
> `node.name` is "alsa_output.pci-0000_00_1f.3.analog-stereo", and only one of them is meant for a
> person.
>
> `sources` cost less than the item predicted. § 2.4's source object is `id`, `name` and `active`,
> and all three are answerable from the `global` event's own props plus the `default.audio.source`
> metadata key, so a source is never bound at all: no proxy, no listener, no `Props` subscription.
> A sink is bound only because § 2.4 asks it for the master volume, which a source has no equivalent
> of.
>
> **No monitor filter, and that is a decision rather than an omission.** PulseAudio synthesizes a
> `.monitor` source per sink and every mixer UI filters them back out, so a filter was the expected
> shape here. A native PipeWire registry does not: `pw-dump` on this machine lists one `Audio/Source`
> beside one `Audio/Sink` with no monitor node between them. A filter written against a node kind
> this registry never emits would be dropping real devices on the guess that some are fake.
>
> `default.audio.source` carries the identical `{"name": ...}` shape as `default.audio.sink`, so the
> one parser serves both and got renamed to say so. `default.configured.audio.sink` is a different
> fact and is not read: on this machine it names a Bluetooth device that is not connected while
> `default.audio.sink` names the analog output actually in use.

> **Built beyond item 5: § 3.2's audio write actions.** Item 5 asks for the read side and this
> shipped the writes with it, which is an addition, not a reading of the item. The reason is that
> the read side alone produces a volume readout nobody can change and a device list nobody can
> switch to, and `main.rs` had no `audio` dispatch arm at all, so every § 3.2 audio row fell into
> the catch-all and was logged. Seven of the nine actions are built: `set_volume`, `set_muted`,
> `toggle_mute`, `set_default_sink`, `set_default_source`, `set_app_volume`, `set_app_muted`. All
> seven were exercised against the live session and put back.
>
> `play_sound` and `set_event_sounds_enabled` are not built. They need a sound player, an event
> sound theme and somewhere to persist the toggle, and none of the three exists anywhere in this
> codebase. `notifications`'s own do-not-disturb toggle already gates sound playback that nothing
> plays.
>
> **A write crosses into the PipeWire thread through `pipewire::channel`, not a controller.**
> Every proxy that thread holds is `!Send` and the thread sits inside a blocking `main_loop.run()`,
> so there is no handle for `main.rs` to call. The channel hands the loop an eventfd to poll beside
> its own sources, which is what that API exists for.
>
> **A hardware sink does not own its own volume, and writing its node's `Props` succeeds and does
> nothing.** This cost three failed attempts and each one looked like success. `pw-cli set-param 59
> Props '{ mute: true }'` against this machine's analog sink was accepted and had no effect, while
> the same write against a stream node worked immediately: the volume lives on the ALSA `Device`'s
> `Route` param, and the node's `channelVolumes` is a mirror that gets restored over anything
> written to it. So a sink with a device behind it is written through `Device::set_param(Route,
> ...)` with the volume in a nested `Props` object, and the node path is the fallback for a virtual
> sink that really is its own owner.
>
> Two more things had to be right before that write landed, and both were silent when wrong. The
> `Route` object's nested `Props` carries `SPA_PARAM_Route` as its param id, not `SPA_PARAM_Props`;
> a live `pw-cli enum-params <device> Route` prints it. And the route target
> (`device.id`/`card.profile.device`) is not in a sink's registry `global` event at all, only in its
> `info` props. Reading it at `global` time returns nothing, the write silently takes the node path,
> and the volume does not move. That is the same "the `global` event carries a subset" trap this
> module's own doc comment already recorded for a stream node's `application.process.id`, met again
> in a different field.
>
> **A `Props` write carries only the field being changed.** Sending the pair loses a write, observed
> live rather than reasoned about: nothing is updated optimistically, so `set_app_volume(id, 0.42)`
> followed immediately by `set_app_muted(id, true)` sent the second object with the volume from
> before the first and put it back to 1.0. A partial `Props` object is applied as a partial update,
> which `pw-cli set-param <stream> Props '{ mute: true }'` confirms.
>
> **`MixerState` got the extraction the review asked for.** Six maps keyed by the same device id
> collapsed into one entry struct per sink and per source, with the PipeWire proxies kept in their
> own maps so the entry stays plain data a test can build. The device tracking this write path needs
> would otherwise have made it nine.

### Phase 29: Icons, Images and the Wallpaper

Implement docs/adr/0054 and docs/adr/0055. Nothing in this codebase draws a pixel from a file. The
`icon` node parses its `size`, reserves that much layout space and paints nothing (`layout/paint.rs`
is literally `"icon" => {}`), there is no `image` node kind, and three capabilities already emit file
paths that no node can consume: tray (`icon_path` spooled to `/dev/shm` by `dbus/shm_icons.rs`),
notifications (the whole `Notify` icon precedence, same spool), and mpris (`album_art_path`). Like
Phase 28's five capabilities, no phase in this document ever owned this. Phase 19 deferred `icon` on
a spec conflict and named no phase to settle it; § 5.2 has no `image` at all; wallpaper had two ADRs
and a § 3.2 row and no phase.

1. **The image cache and the `image` node.** `image { source, fit }` plus § 5.1's base
   `width`/`height`. `source` is an absolute path. Decode through femtovg's existing `image`
   dependency for PNG and JPEG, through `resvg` for SVG (ADR-0054 decision 4), upload once and cache
   by (resolved path, integer pixel size). `image` has no intrinsic size: it takes the box it is
   given and measures `0x0` without one, unlike `icon`, because knowing a file's dimensions means
   decoding it during a layout pass that has no canvas to decode against.
2. **`fit`.** `cover` (default), `contain`, `stretch`. Clip to the node's rect; `cover` overflows the
   paint rect and crops (ADR-0055 decision 3).
3. **The icon resolver and the `icon` paint arm.** `freedesktop-icons`, called synchronously with the
   cache in front of it. `icon.name` that starts with `/` is a path, everything else is a theme name
   (ADR-0054 decision 2). This is what makes the tray draw icons instead of truncated app names.
4. **Wallpaper.** No capability, no controller, no dispatch arm. A `Background` panel with an `image`
   in it, its `source` bound to a `state()` signal, all four of which already exist (ADR-0055). The
   work here is proving it in `dev-config/oblisk/shell.lua`, not writing engine code.
5. **`system:find_icon`.** Not built. § 9.2's `app_id` to `.desktop` to `Icon=` half has no caller
   once `icon.name` resolves theme names itself, and `freedesktop-icons` does not do it (ADR-0054
   decision 5).

Testing: the headless EGL harness Phase 19 describes and nothing has yet built would assert this
directly, since "an `image` whose source is a 1x1 red PNG paints red" is the same shape as its own
worked example. Short of that, the resolver and the fit math are ordinary unit tests, and the
end-to-end check is the live session: the tray draws icons, and a `Background` panel shows a
wallpaper.

> **Built.** Verified on the live session, twice, with screenshots rather than by reading the log.
> A `Background` panel showed `dev-config/oblisk/wallpaper.svg` rasterized to the full 1920x1200
> output, `cover` cropping the 16:9 source into a 16:10 screen, and the volume pill drew
> `audio-volume-low` from `Tela-circle-dracula` beside its text, re-resolved from the signal on
> every push.
>
> **`freedesktop_icons::default_theme_gtk()` is unusable, and it fails silently.** It spawns
> `gsettings get org.gnome.desktop.interface icon-theme` per call, which is a process spawn on the
> thread that paints. Worse, it maps the setting through the theme's `index.theme` and returns the
> `Name=` field, while `with_theme` is keyed by *directory* name: on this machine it answers
> `"Tela circle dracula"` for a directory called `Tela-circle-dracula`, so every lookup returns
> `None` and every icon is simply missing with nothing logged anywhere. The theme is read out of
> `settings.ini` directly instead. This was found by running it, not by reading it, and it is the
> second time in three phases that a plausible-looking API produced a silently wrong answer rather
> than an error (the first was `audio`'s cubed `channelVolumes`).
>
> **`oblisk.config_dir` was added, and item 4 could not be proved without it.** ADR-0047 made the
> config a directory rather than a file, which makes it a place to ship a wallpaper, an icon or a
> sound, and nothing in Lua could name that place. It is a string beside `oblisk.version`, derived
> from the parent of the `shell.lua` actually loaded so it cannot disagree with it. Without it the
> shipped wallpaper needed an absolute path baked into a config in the repository.
>
> **The wallpaper ships as SVG, deliberately.** It is the one asset here a reviewer can read as
> text, and rasterizing it at the output's own width is what exercises `resvg` at a size no icon
> reaches. `rasterize_svg` needs no canvas, so the unit test renders that exact file and asserts
> the gradient survived, which is the half that would otherwise break silently: a tree that parses
> to nothing renders a transparent pixmap rather than an error.
>
> **The review found two silent bugs, and ADR-0031 had already named one of them.** A path-keyed
> cache serves an app's first tray icon forever, because `dbus/shm_icons.rs::write_png` overwrites
> the same spool path in place on every `NewIcon`. ADR-0031 chose that deliberately and deferred the
> consumer-side fix with the trigger spelled out: "Renderer-side texture cache-busting, only once
> the renderer's actual icon-loading mechanism exists and is shown to need it". This is that
> mechanism and it needed it on the first commit that could have exercised it. The key now carries
> the file's modification time and length, at the cost of one `stat` per image node per frame.
>
> The second: eviction deleted a texture that an already-recorded draw call in the same frame still
> named, because femtovg resolves an `ImageId` at `flush` rather than at `fill_path`. It answers a
> missing id with default paint parameters instead of an error, so the symptom is a silently blank
> image in any frame drawing more than 128 distinct ones. Eviction now queues and `paint_tree`
> frees the queue before it walks. Both bugs share a shape worth naming: neither crashes, neither
> logs, and both look exactly like a correct frame.
>
> **The tray draws.** Telegram registered a `StatusNotifierItem` reporting
> `icon_name: "org.telegram.desktop-symbolic"` and a null `icon_path`, and the bar drew the paper
> plane out of the active theme at 16px. That is ADR-0054 decision 2's `icon_name` branch end to
> end. The `icon_path` branch is still only a code reading: nothing on this session produced a
> pixmap-only item, so `dbus/shm_icons.rs`'s spooled PNG has never been painted.
>
> One older problem is now visible behind it. `intrinsic_content_size` has no horizontal `list`, so
> a second tray item would stack below the first and be clipped by a 34px bar. One item hides that
> completely.
>
> A second, unrelated one showed up in the same log and is worth writing down before it is
> rediscovered: the tray's first `StateSnapshot` was refused with "no connection registered for
> generation 0", because the Supervisor pushed it before the Renderer had registered. `tray` pushes
> only on change, unlike `battery` or `brightness` which poll and self-heal, so a capability that
> loses that race stays `nil` until the app happens to change its icon. It hydrated here on a later
> push. Not this phase's to fix, and not a thing to discover twice.


### Found in Phase 28, owned by Phase 29: a themed icon renders nearly invisible

Not this phase's to fix and not a thing to discover twice. The `icon` node resolves and rasterizes
correctly and then draws in almost the wrong colour.

A freedesktop icon theme paints with `fill:currentColor` and sets the actual colour in a `<style>`
block through a class (`.ColorScheme-Text { color:#565656; }`). resvg does not apply that rule, so
`currentColor` falls back to its default, and a 16px `audio-volume-low` from `Tela-circle-dracula`
rasterizes to pixels around RGB 30 out of 255. On this bar's `#1e1e2e` background that is a shape
nobody can see. Confirmed by rasterizing the file directly and reading the pixels, not by looking at
a screenshot: resolution returns the right path, the pixmap has 116 non-transparent pixels, and every
one of them is nearly black. Breeze's `org.telegram.desktop-symbolic` does the same thing with
`#232629`, so this is how icon themes are written, not one theme's quirk.

The shape of the fix is a decision rather than a patch, which is why it is written down instead of
bolted on here. `text` already takes a `foreground`, and an icon drawn from a monochrome theme wants
the same thing: either give resvg a `color` to resolve `currentColor` against, or tint the rasterized
pixmap. Both change what `icon` means (a themed glyph the config colours, rather than an image),
which is an amendment to docs/adr/0054 and not a one-line change. The full-colour icons this affects
nothing for (an app icon like `firefox.svg`) already draw correctly, which is why Phase 29 saw the
tray draw and read it as working.

### The missing animation model

The largest remaining gap, and it was found by pulling on a smaller one. `CONTEXT.md`'s Lease exists
to hold a removed node's GPU resource alive for "a wallpaper crossfade, an in-flight transition", and
it has had no caller since Phase 12 (ADR-0023 item 7). The reason is not that the API is missing. A
grep for transition, animation, or easing across `docs/` and `renderer/src/` returns nothing but
Hyprland's compositor-side `layerrule`. The feature the lease was built to serve was never specified.

Underneath that, Lua had no clock at all when this was written. Phase 28 built `system.time`, so a
config now has a 1 Hz heartbeat and can draw a time that moves. That is not an animation clock and
does not change the paragraph's conclusion: 1 Hz is three orders of magnitude short of a frame, and
ADR-0048 still keeps `os.time` as a read rather than adding a callback. A config cannot animate
anything today, whatever API the lease grows.

Quickshell has `EasingCurve` and `ElapsedTimer` in `core/` and inherits QML's `Behavior`,
`NumberAnimation`, and `Transition` on top. Matching that is a real body of work and it is not
scheduled here, because a shell without animation is functional and nothing is blocked.

One constraint does need respecting now, in Phase 19 item 9. Frame gating is written as "repaint when
the scene changed", which is one reason to wake. An animation is a second reason, orthogonal to the
first: a running animation must repaint whether or not a signal changed. Build the gate so a second
reason can be added rather than replacing the condition, and this stays an additive change instead of
a redesign.

The Lua-facing lease follows from that, not before it. Quickshell's `Retainable` exposes a refcounted
`lock()`/`unlock()` and a `dropped()` signal so the config can say "not yet" when its exit transition
is still running, and a `RetainableLock` wrapper because Quickshell found raw locking "overly
complicated and error prone". That is the right shape to copy on the day exit transitions exist.

### Judged and dropped

**Lazy surface creation.** Resolved by ADR-0049 rather than deferred. Creating `popup` and `window`
objects on show is forced by the protocol, and it delivers what Quickshell's `LazyLoader` delivers
without a `LazyLoader`. Asynchronous incubation during frame gaps, the other half of that type, has
no analogue here: Oblisk's per-open creation is one Wayland object, not an incubated QML tree.

**Config-triggered reload.** `Quickshell.reload(hard)` is callable from QML; Oblisk's reload is
Supervisor-only through `inotify`. It has no caller. ADR-0047's recursive watch covers edits, and
ADR-0048 removed file reading from Lua, which was the one remaining "something external changed"
trigger a config could have noticed and nothing else would. Build it if a caller appears.

### Phase 30: The Guard Rule

Implement the read half of IDL § 7.3. Phase 25 item 2 built the write half: every `CommandEnvelope`
now carries the `generation_id` that sent it and the `expected_revision` its config was reacting to.
The Supervisor reads neither. `main.rs`'s `RendererFrame::Command` arm matches on
`params.capability` and dispatches, so a write from a superseded generation is accepted today, which
§ 7.3 says must not happen.

Both inputs are already in scope at that arm. `authoritative.generation_id` is the ledger § 7.3 calls
for, and `revisions: HashMap<String, u32>` is the per-capability counter `snapshot::bump_revision`
maintains. This is a guard clause, not a subsystem.

1. **Drop a command from a superseded generation.** `params.generation_id < authoritative
   .generation_id` is the whole test. Not `!=`: under ADR-0024 a candidate generation evaluates its
   config *before* it is authoritative, so its id is legitimately greater, and `!=` would drop every
   write a reload makes. Which raises item 2.
2. **Decide whether a candidate may write at all.** A candidate's config calling `oblisk.audio:set`
   during evaluation moves real hardware, and ADR-0024's rollback guarantee cannot take that back.
   The alternatives are: let it through (a rolled-back config leaves the volume where it put it),
   drop it (a config cannot configure the machine it is booting on), or queue it until the swap
   commits. This is a trade-off with no obviously right answer and it is hard to reverse once
   configs depend on the behaviour. Write the ADR before writing the code.
3. **The revision guard needs a narrower rule than "stale".** Read literally, § 7.3 drops any command
   whose `expected_revision` is behind the Supervisor's, which breaks continuous controls. Dragging a
   volume slider sends `set` at revision 5, the Supervisor applies it, bumps to 6 and pushes; the
   next drag frame is already in flight stamped 5, and a literal reading drops it. The user sees the
   slider stick. § 7.3 was written against the generational swap race, not against a config that
   writes faster than a snapshot round trip.
4. **`expected_revision: 0` means "never hydrated", not "stale".** Phase 25 established that
   `bump_revision` starts at 1, so no push can produce `0`. `process` stamps `0` on purpose because
   it holds no state to be stale about. A guard that compares `0` against a live counter drops
   `process.run` entirely, so exempt capabilities with no entry in `revisions`.
5. **Log the drop.** § 7.3 says "instantly drops the packet" and says nothing about telling anyone.
   A silently dropped `set` is the same debugging shape as Phase 29's missing icons and Phase 25's
   frozen bindings: the config looks right and nothing happens. One `eprintln!` naming the
   capability, the action, and which of the two guards fired.

Deliberately not built: a reply frame telling the Renderer its command was dropped. § 7.1 has no
such frame, and adding one makes every write a round trip. The log line is for whoever is debugging
the config, not for the config.

---

## 6. The gap ledger: measured against a shell that already ships

Every phase above was written from `docs/oblisk-idl-api-specs.md`. A spec cannot list what it forgot,
so it cannot answer "is this enough to be somebody's daily shell". This section works the other way
round. It takes a Quickshell config that is already somebody's daily shell (`anasgets111/dotfiles`,
at `quickshell/.config/quickshell`) as a reference workload, and diffs it against what this workspace
compiles today.

A ledger, not a phase. Nothing here is scheduled, several entries need an ADR before a line of code,
and two are decisions about what Oblisk is for. Entries cite code, not docs, because the docs are
what missed this.

### What the reference workload is

Nineteen bar modules across three zones, eight Wayland surfaces, about forty data sources. It drives
PipeWire, UPower, BlueZ, MPRIS, NetworkManager, SystemTray, Polkit and PAM through native bindings.
It speaks niri's event stream on one socket and its request channel on another. It polls backlight
and keyboard-backlight sysfs at 100ms, shells out to more than twenty binaries, fetches weather over
HTTPS, and renders a live 256-bar audio spectrum through a GLSL fragment shader in the middle of the
bar.

### The verdict, by layer

| Layer | State | Detail |
| :--- | :--- | :--- |
| Capabilities (read) | near complete | every native Quickshell service has an Oblisk counterpart |
| Capabilities (write) | near complete | ahead in one place: that config reads the power profile, Oblisk sets it |
| Surface roles | complete | `panel` on four layers, `window`, `popup`, `lock`, all live-tested |
| Pointer input | one event of four | `on_click` and nothing else |
| Paint vocabulary | four operations | fill, radius, per-edge border, blit |
| Animation | absent | Lua has no clock faster than 1 Hz |
| Text and layout | sufficient | shaping, clipping, alignment and keyed reconciliation are built |

The data layer is fine. That was the surprise. Input and paint are where this falls over.

### The pointer reads one of four events

> **Item 1 is built.** `on_click` now fires for left, right and middle and takes the button's name
> as a second argument, which amends ADR-0050 decision 2. The six right-click modules below are
> unblocked; the hover and scroll rows are not. The paragraph and table are left as they were
> measured, because they are what the ranking was built from.
>
> Verified by unit test, not on a live session, which is a weaker claim than Phase 29's and is worth
> stating rather than leaving to inference. Six tests cover the three seams: the evdev-to-name map
> including the codes it refuses, the press/release button match, the release that must not clear
> another button's press, and the callback receiving `(rect, button)` with a one-parameter handler
> still running unchanged. What no test reaches is `pointer_frame` itself, which needs a compositor
> to deliver a real `BTN_RIGHT`. Until someone right-clicks the dev config's brightness pill and
> watches the number go down, that half is written and unconfirmed.

`renderer/src/wayland/mod.rs`'s frame handler matches `Press`, `Release` and `Leave`. Its `_ => {}`
arm drops `Enter`, `Motion` and `Axis`, with a comment saying why: "nothing in § 5.2 reads hover or
scroll yet". `clickable_button` looks up one property, `on_click`, and `fire_on_click` calls it with
a rect and no button index. That is the whole pointer model.

Eleven of nineteen bar modules lose their interaction to it, most losing more than one thing. The
last row is not a bar module; it is the only thing in the reference shell that needs a drag.

| Module | What it needs | What it gets |
| :--- | :--- | :--- |
| Volume | hover-expand, drag slider, wheel, middle-click mute, right-click panel | one click |
| SysTray | left activate, right menu, scroll forwarded to the item | one click; lays out horizontally now |
| ScreenRecorder | left region record, middle focused-output record, right options | one click |
| PowerMenu | click arms a countdown, right-click cancels it | no cancel |
| ArchChecker | left polls, right opens the panel | one of the two |
| IdleInhibitor | left toggles, right opens settings | one of the two |
| WallpaperButton | left opens the picker, right randomizes every monitor | one of the two |
| BatteryIndicator | hover tooltip: power source, platform profile, CPU governor | built, minus the governor |
| DateTimeDisplay | hover tooltip: mini calendar and weather detail | unblocked |
| MediaIndicator | hover opens the media panel | unblocked |
| WorkspaceStrip | hover highlights the slot under the pointer | unblocked |
| DisplaySettings | drag monitors on a canvas, snapping to neighbouring edges | no drag |

Three things are missing. They cost very different amounts, so keep them apart.

1. **A button index on `on_click`.** The cheapest item in this ledger. `wl_pointer::button` already
   carries the code, and `fire_on_click` already builds a table argument for the rect, which can
   carry a second field. One § 5.2 row, nothing else. Right-click alone repairs six modules.
2. **`on_hover`.** **Built, as a signal rather than a callback (docs/adr/0062).** `hover(name)` is a
   boolean the engine writes from `Enter`/`Motion`/`Leave`, `hover_rect(name)` is where the node
   was, and a node claims the region with a `hover` property. A tooltip is a `popup` with
   `grab = false` binding `visible` and `anchor_rect` to the pair, which is two lines and no state
   machine. The four rows above that wanted hover are unblocked; `dev-config` uses it for the
   battery tooltip, which is also where the power detail moved out of the pill.

   The design question the ranking named -- what a hover *is* in a tree that re-resolves on every
   push -- is answered by the condition/event split in decision 1, and the answer to "may the engine
   emit a signal" is yes, which is the direction that ADR sets. Verified by unit test at two seams
   and against the shipped config, not on a live session: `pointer_frame` still needs a compositor,
   the same gap item 1 records.
3. **`on_scroll`.** `Axis` arrives at the same drop site. The open question is what a scrollable
   container means, not what the callback looks like. Clipping already exists: `paint_tree` pushes an
   `intersect_scissor` per node, so a subtree is already cut to its parent's box. The missing half is
   a scroll offset applied during layout. Small change to `layout::scene`, large one to the spec,
   since § 5.2 has no container that owns a viewport. Until it lands, no panel holding a list is
   usable: network access points, bluetooth devices, notification history, launcher results, SMS
   threads.

### Paint has four operations

`layout::paint` fills a rounded rect, strokes up to four border edges, blits an image or icon, and
draws a text run. Every one of `rect`, `row`, `column`, `button`, `panel`, `window`, `popup` and
`lock` resolves to `paint_box`. The reference shell's look is built almost entirely out of what is
not in that list.

**Gradients and drop shadows are nearly free. Take them first.** femtovg 0.26, already the only
drawing dependency, ships `Paint::linear_gradient`, `Paint::radial_gradient`, `Paint::box_gradient`
and their `_stops` variants, plus a Canvas-2D shadow model on `Canvas` itself (`set_shadow_color`,
`set_shadow_blur`, `set_shadow_offset`). The work is § 5.2 rows, parsers in `layout::node`, and calls
in `fill_rect`. A `background` accepting a table of stops, and a `shadow` alongside `border_width`,
land the notification cards' `RectangularShadow` and every gradient pill in one slice. femtovg caps
the blur kernel at +/-24 physical pixels, so a `shadowBlur` above 16 renders tighter than the Canvas
2D spec says. Do not promise a large shadow.

**Backdrop blur is a decision, not a feature.** The reference shell's most distinctive trait is
`BackgroundEffect.blurRegion`, one continuous blur shared by the bar and whichever dropdown is open,
so it does not seam at the bar's edge. Wayland gives a client no way to read what is behind its own
surface, so nobody implements this by sampling the compositor. Two ways, and both need an ADR before
either gets built.

- *Compositor-side.* Hyprland's `layerrule = blur, <namespace>` blurs the surface for us. Zero
  renderer code, one documented namespace. niri has no equivalent, so it does nothing on the
  compositor this project targets.
- *Client-side, against the wallpaper we already own.* The config knows the wallpaper path, the
  Renderer already decodes and caches it (Phase 29), and femtovg has `Canvas::filter_image` with
  `ImageFilter::GaussianBlur`. Blur that image once, blit the offset crop under the panel, and the
  result matches the reference shell exactly, because that shell is not sampling a live backdrop
  either. It fails visibly over any window that is not the wallpaper.

Only the second works on niri, and it is a lie that looks right. That belongs in an ADR, not a commit
message.

**No shaders and no immediate-mode canvas, and that is the right call for now.** The reference config
uses raw GLSL twice: six wallpaper transitions, and the 256-bar Cava spectrum. It uses `Canvas` twice
more: the concave notch at the bar's corners (`RoundCorner.qml`, a cubic-bezier arc approximation)
and the monitor-arrangement grid. Exposing either to Lua puts the GPU in reach of config code, which
is a far larger decision than any node property.

The notch is the one real loss, because it carries the look and nothing else reaches it. femtovg's
`Path` already has `arc`, `arc_to` and `bezier_to`, and `Canvas` already has `fill_path` and
`stroke_path`, all of which `layout::paint` uses today for nothing more exotic than a rounded
rectangle. A `shape` node taking a path costs far less than a shader and closes the notch, the arcs
and the circular progress rings together.

### Animation: see "The missing animation model" above

Written up in section 5. Repeated here only for what the reference workload adds. That section
concluded from a grep that nobody ever specified an animation model. This config shows what one is
worth in practice: four named durations reused everywhere (`animationFast` 100ms, `animationDuration`
147ms, `animationSlow` 250ms, `animationVerySlow` 400ms), two shared `Behavior` components
(`ColorTransition`, `NumberTransition`), and effects that carry meaning rather than decoration. The
lock card shakes horizontally on a failed authentication. The notification card slides off to the
right when dismissed, so the gesture and the result match. The OSD queue suppresses lower-priority
entries instead of stacking them.

One correction to that section. It says a shell without animation is functional and nothing is
blocked. True of the engine, false of this workload: PowerMenu, WorkspaceStrip, SpecialWorkspaces and
Volume all use expansion as their primary affordance, and each is blocked twice over. It needs the
hover to trigger the expansion before it needs the easing to run it.

### Data no capability carries

The capability roster covers every native service this shell uses. It does not cover what the config
reaches for outside them.

| Missing | Used for | Nearest path today | Verdict |
| :--- | :--- | :--- | :--- |
| A JSON decoder in the Lua environment | `nvtop -s`, `lsblk --json`, `busctl --json=short`, weather, currency | **Built** as `json.decode` (ADR-0057); was none, and `process.run`'s `out_cb` handed Lua a string Lua could not read | highest value in this table, and it unblocked every subprocess row below it |
| An HTTP client | weather (open-meteo), IP geolocation, currency conversion | `process.run` `curl`, then the decoder above | fine as a subprocess; a capability would be scope creep |
| Desktop entry lookup | app-id to icon and display name, and the whole launcher | ADR-0054 decision 5 dropped `system:find_icon`, and this is the caller it said had none | that ADR was right about the mechanism, wrong that nothing would want the data; amendment, not reversal |
| Per-workspace window lists | `WorkspaceStrip` draws each workspace's app icon | § 2.9 carries one global `active_client` and per-workspace `{ id, idx, name }` | ADR-0056 chose that payload, so this is not an oversight; niri's `Window.workspace_id` is right there, making it an additive field |
| Special workspaces | the `SpecialWorkspaces` pill | nothing in § 2.9 models them | niri-specific; ADR-0056's one-compositor decision makes it cheap to build and awkward to name |
| Source (microphone) mute | `PrivacyIndicator`'s click target | § 3.2 has `set_default_source` and no `set_source_muted` | a spec hole, not a design choice; the mixer already writes node props |
| KDE Connect | SMS, ring, mount, remote commands | none; Lua cannot speak D-Bus, and this needs a live signal stream, not one-shot calls | out of scope by a wide margin, and the only entry arguing for a general D-Bus escape hatch |
| Monitor configuration | the display-settings arrangement editor | § 2.15 `screens` reads; nothing writes | writing output config is compositor-specific, and ADR-0056's reasoning applies unchanged |

Take the first row, whose original entry here argued that a pure-Lua decoder of about a hundred lines
belonged in the config rather than the engine, and stayed YAGNI-correct until three configs had each
written their own. That was wrong, and wrong for a reason worth recording: it was written without
checking what the engine already had. `renderer/src/lua/mod.rs` has converted `serde_json::Value` to
Lua since Phase 19 item 16, because every capability payload arrives that way, and both `serde_json`
and mlua's `serde` feature were already compiled into the `renderer`. A decoder written in Lua would
also have disagreed with that converter about `null`, which is the part no config author could have
debugged. ADR-0057 records the decision and why jq, a query crate, and the pure-Lua decoder all lose
to one function on the mapping that was already there.

### What the reference workload proves is already right

A gap ledger reads worse than the situation is.

- **The capability boundary held.** Sixteen capabilities, and the only native Quickshell services
  with no Oblisk counterpart are KDE Connect and desktop entries. The write path is ahead in one
  place: that config reads the power profile through `powerprofilesctl get` and cannot set it, while
  `power:set_profile` is built.
- **Brightness is better here, on both halves.** The reference config polls
  `/sys/class/backlight/*/brightness` every 100ms to read, and forks `brightnessctl` to write. Oblisk
  reads through a udev `backlight` subsystem watch, so the change wakes it instead of it asking ten
  times a second, and writes through `org.freedesktop.login1.Session.SetBrightness`, so no fork and
  no setuid helper on the write path either.
- **The surface roles are complete.** All eight of the reference shell's surfaces map onto built
  Oblisk roles, including the two that are usually hard: a real `ext-session-lock-v1` lock, and a
  registered Polkit agent with PAM behind a `secure_submit`.
- **Nerd Font glyphs sidestep the `currentColor` bug.** The themed-icon defect recorded above under
  Phase 29 does not touch this workload. The reference shell draws its chrome with Nerd Font
  private-use glyphs as `text`, which takes a `foreground` and works today. Only the tray and app
  icons come from a real icon theme, and those are full-colour SVGs that already render correctly.
  The bug is real and still worth fixing. It is not on the path to this shell.

### The ranking

By modules unblocked per unit of work, which is not the same as by size.

1. **A button index on `on_click`.** Six modules, one field, no new concepts. Nothing else should go
   first. **Built**, as a second argument carrying a name (`"left"`, `"right"`, `"middle"`) rather
   than a field or a code. ADR-0050's second amendment records the three calls that took: a second
   argument keeps every one-parameter handler working, a string matches what `fit`, `layer` and
   `align_h` already do at this boundary, and an unhandled evdev code still does nothing rather than
   arriving as `"other"` and running a handler written for the left button.
2. **A JSON decoder reachable from Lua.** Turns `process.run` from fire-and-forget into a data
   source. Every subprocess row above depends on it. **Built**, as `json.decode(text)` returning the
   value or `nil` plus a message (ADR-0057). It is one function over the JSON-to-Lua converter the
   engine already had, so the engine grew no new dependency and no second `null` mapping.

   Verified by nine unit tests, one of them decoding a real captured `niri msg -j focused-window`
   line with its nested `null`s and its non-ASCII title, plus a `luac -p` syntax check on the config
   below. Not live-verified: no test drives the full path, which is `on_click` to `process.run` to
   `out_cb` per line to `json.decode` to `state:set` to a repaint, because the middle of it needs the
   Supervisor to spawn a real child. `dev-config/oblisk/shell.lua`'s `refresh_window_title` is that
   path written out, reporting the focused window title, which is data no capability carries because
   ADR-0056 keeps window lists out of `workspaces`. Clicking it is the check.

   That config carries a request counter, and it is not decoration. A review traced the failure: left
   click, then a reset, then the first click's reply landing late and overwriting the reset, with a
   rapid double click showing whichever answer finished last rather than the newer one. Confirmed by
   loading the real `shell.lua` under stubbed engine globals and driving both orderings through the
   button's own `on_click`, which fails on the first scenario with the counter removed and passes
   with it. Nothing in `process.run` cancels a request the config has moved on from, so every
   subprocess-backed module needs that guard, and a demo without one would teach the wrong shape.
3. **Gradient and shadow on `rect`.** Already in the dependency. Parsers and § 5.2 rows only.
4. **`on_hover`.** ~~Two tooltips, a hover-to-open panel, a hover highlight, and every
   expand-on-hover affordance.~~ **Built (docs/adr/0062).** The engine may emit a signal, and this
   is the first one it emits.
5. **`on_scroll` plus a scroll offset.** Clipping is built, so this is layout and spec work, not
   paint work. Blocks every list-bearing panel until it lands.
6. **The animation model.** Largest item, scoped in section 5, and gated behind item 5 in practice;
   item 4 is no longer in front of it. An expanding pill has its hover now and snaps without the
   easing.
7. **Backdrop blur.** One ADR, two bad options, and only the client-side one works on niri.
8. **A `shape` node taking a path.** Closes the notch, the arcs and circular progress together, and
   costs far less than exposing shaders.
9. **Shaders.** The Cava spectrum and the wallpaper transitions. Lowest value, and the only item that
   puts the GPU in reach of config code.

### Found while writing this: `--validate` does not exist

Both validation protocols in this document, section 3 item 3 and section 7 item 3, have said `cargo
run -p renderer -- --validate <path>` since they were written, and both call it a dry run of the
config compiler. The Renderer parses no command-line arguments. `renderer/src/main.rs` never reads
`env::args`, there is no argument parser in its dependency tree, and the flag is ignored in full.
That command validates nothing. It starts a live Renderer, which connects to the compositor, opens
the DRM render node, and stays up until something kills it.

Worse than a no-op. The command is documented as the safe way to check a config, and it launches a
second shell over the running session. Found by running it. It sat there for twelve minutes holding
`/dev/dri/renderD128` and printing nothing. Both call sites are corrected in place rather than
deleted, because the flag they name is worth building. A config that fails to load should say so at a
shell prompt, not by taking over the screen.

Half of that is no longer true, by accident. docs/adr/0059 decision 1 makes the Renderer exit when
the Supervisor's control socket is gone, and a Renderer launched from a shell prompt has no
Supervisor at all, so the command above now exits `70` in a fraction of a second instead of holding
the render node until someone finds it. The flag still does not exist and the command still
validates nothing. What it no longer does is take over the session while failing to.

---

## 7. Continuation Testing & Validation Protocols

```bash
#!/usr/bin/env bash
set -euo pipefail

# 1. Structural compile check across the now-larger workspace
cargo check --workspace --release

# 2. Full test suite, including the new transport/loader/layout/watcher seams
cargo test --workspace

# 3. There is no dry run. `--validate` was never built (section 6, "Found while writing this").
#    The Renderer reads no arguments, so the line below starts a live shell over the running
#    session instead of checking anything. Left here, corrected, rather than deleted, because
#    the flag it names is still worth building.
#
#    cargo run -p renderer -- --validate ~/.config/oblisk/shell.lua   # DOES NOT VALIDATE
```
