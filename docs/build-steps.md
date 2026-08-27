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
4.  **Static Layer Constraints**: Register the three static surfaces returned by your layout (ADR-0007):
    *   `main_bar`: anchored on top, marked exclusive.
    *   `overlay_canvas`: anchored to all four edges, non-exclusive, transparent. On boot, immediately commit an empty input region (`wl_compositor::create_region` with no added coordinates) to allow background applications to receive pointer clicks.
    *   `wallpaper_layer`: `Background` layer, non-exclusive, one per monitor.

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
   this.

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

Notifications (§ 1), Tray (§ 2), MPRIS (§ 3), NetworkManager (§ 4), BlueZ (§ 5), idle (§ 7,
ADR-0032 — split into two named sub-items below), telemetry (§ 11), and power/thermals (§ 13) all
follow the pattern already proven twice in this codebase, `dbus::polkit` (D-Bus proxy/agent
registration) and `audio::mixer` (event-driven
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

## 5. Continuation Testing & Validation Protocols

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
