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

# 3. Dry-run the config compiler to ensure Lua AST evaluates properly
echo "Step 3: Validating user layout configuration schema..."
cargo run -p renderer -- --validate ~/.config/oblisk/shell.lua

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

   Item 6's own third commit then changed the cadence this is paid at, which is what turns it from
   untidy into a real problem. `Scene::apply` never parses the paint properties, so `layout::paint`
   is the first thing that ever validates a `background` or a `radius`, and it does that while
   drawing. A malformed value is logged and treated as absent rather than failing the apply, since
   there is no rollback available mid-frame with a GL context bound. So once `paint_tree` has a
   production caller, one `background = 5` in one node formats and prints on every frame, on the
   Wayland dispatch thread, at whatever rate item 9's frame callbacks fire.

   Rate-limiting the log is the wrong fix and would hide the real one. Paint properties should be
   parsed once at apply time, where a failure already has somewhere to go: a `LayoutError` that
   rolls back and reaches `rescue`, exactly as a bad `align_v` does today. That is the same "parse
   geometry once into the retained node" that item 5 defers, and both halves want doing together,
   which is why this is recorded here rather than bolted onto a paint commit.
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

### Phase 24: Memory Measurement Harness

Implement ADR-0043 decision 1, which the fonts and atlas work in Phase 19 is otherwise unverifiable
against. Read `/proc/[pid]/smaps_rollup` for the Supervisor and each live Renderer, sum PSS, record
per-Renderer USS, and sample the two-Renderer PBA handoff window as its own number. Report GPU memory
from DRM fdinfo's `drm-*-memory` fields rather than assuming `smaps` captures it.

No dependency on the paint pass; buildable at any point, and more useful before Phase 19 than after,
since it gives the font decision a before-and-after number.

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

### The missing animation model

The largest remaining gap, and it was found by pulling on a smaller one. `CONTEXT.md`'s Lease exists
to hold a removed node's GPU resource alive for "a wallpaper crossfade, an in-flight transition", and
it has had no caller since Phase 12 (ADR-0023 item 7). The reason is not that the API is missing. A
grep for transition, animation, or easing across `docs/` and `renderer/src/` returns nothing but
Hyprland's compositor-side `layerrule`. The feature the lease was built to serve was never specified.

Underneath that, Lua has no clock. Capability pushes are the only thing that changes a value over
time, they arrive at the Supervisor's pace, and ADR-0048 keeps `os.time` as a read rather than adding
a callback. A config cannot animate anything today, whatever API the lease grows.

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

---

## 6. Continuation Testing & Validation Protocols

```bash
#!/usr/bin/env bash
set -euo pipefail

# 1. Structural compile check across the now-larger workspace
cargo check --workspace --release

# 2. Full test suite, including the new transport/loader/layout/watcher seams
cargo test --workspace

# 3. Dry-run a reference config against the real loader
cargo run -p renderer -- --validate ~/.config/oblisk/shell.lua
```
