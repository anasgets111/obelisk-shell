# Framework gaps and scope

Source inspection compared the current implementation with the reference QML config and Quickshell C++ at
`2d3b3e9`. This is a scope guide, not a commitment to full Qt/Quickshell parity or a live hardware validation.
[API](oblisk-idl-api-specs.md) and [services](oblisk-supervisor-services-dbus.md) describe
what exists; [decisions](decisions.md) holds history.

Rust owns platform connections, validation, secret handling, resource lifetimes, input and
rendering. Lua owns composition, appearance, user preferences and orchestration. A feature absent
from `dev-config` is not necessarily an engine gap.

## Recommended engine work

Recommendations, not accepted API designs. Correctness comes before feature expansion.

| Area | Current limit | What to do | Keep out of Rust |
| :--- | :--- | :--- | :--- |
| Command authority | Ordinary command dispatch does not enforce the envelope's generation/revision claims | Enforce sender/authority checks; settle stale-revision semantics before relying on them | Generation IDs and validation in Lua |
| Text editing | Append/backspace input; no editing cursor, selection, clipboard, undo or IME composition | Native ordinary-field editing and composition; retain the separate secure input path | Form validation policy and SMS/search UI |
| Keyboard and accessibility | Field navigation callbacks, no general focus traversal or accessibility tree | Focusable controls, keyboard activation and accessibility semantics | Widget appearance and panel navigation policy |
| Timers | Clock updates at 1 Hz; no config-owned timer | Cancellable one-shot/repeating callbacks with reload cleanup; no idle redraw loop | Retry intervals, debounce delays and OSD duration |
| Paths and drawing | Boxes, text and images; no dynamic paths, arcs, gradients or transforms | Add the smallest drawing operations needed by a real component; use SVG for static artwork | Dedicated notch, gauge or spectrum widgets |

## Needs discussion before implementation

| Area | Gap | Decision needed |
| :--- | :--- | :--- |
| Process control | Line output and kill only; no stdin writes, signal selection, explicit cwd/env or detached mode | Which caller needs each extension? Define reload ownership before allowing detached children |
| External IPC | `oblisk set/toggle` writes named state; no callable methods or returned results | Are state writes sufficient, or do integrations need request/response commands? |
| General I/O | No native HTTP, socket client/server or arbitrary watched file contents; JSON storage and folder watching exist | Prefer subprocess helpers first. Add native I/O only for demonstrated lifecycle, latency or data-volume needs |
| KDE Connect | No native device/plugin model | Dedicated Supervisor capability versus a helper streaming state; do not expose unrestricted D-Bus just for parity |
| Windows and displays | Workspace summaries and one active client; screens are read-only | Select the required window actions and output settings, then define Niri/Hyprland differences and apply/revert behavior |
| Service depth | MPRIS lacks stop/shuffle/repeat/rate/volume and capability flags; PipeWire lacks channel/peak/link detail; UPower exposes a composite battery | Extend existing capabilities for concrete controls; do not mirror every upstream property |
| Bluetooth codecs | `codec` is nil; no codec command is accepted | Whether codec selection is needed, and how the audio capability should own device profiles/routes |
| Blur and effects | No blur, shadows, arbitrary masks or shaders | Separate blurring our own images from capturing content behind a surface; settle compositor support and GPU cost |
| Capture | No screen/window image or live texture | Build for previews/screenshots only when requested; external recording does not require renderer capture |
| Large collections | Every list item is constructed; no viewport delegate reuse or grid layout | Measure the target workload before adding virtualization or layout vocabulary |
| Wayland/input extras | No shortcut inhibition, per-surface idle inhibition, touch gestures or cross-app drag/drop | Pick supported hardware/protocols and an actual consumer; logind inhibition is already available |
| Runtime construction | Top-level declarations change through generation swaps | Keep current topology rules unless dynamic windows require a different lifetime model |
| Fonts and localization | Global font chain; no per-node family or translation API; application names are unlocalized | Decide supported language/font requirements before expanding text and application metadata |
| Animation | `animate` eases numbers and colours between resolved values (ADR-0145); no exit animation, looping/indeterminate motion, edge-table or transform tweens | Exit needs a node to outlive `visible = false` or a config timer; loops need a running-state model. Decide against a real consumer before adding either |

## Keep in config or use existing tools

| Feature | Existing route | Do not build for parity alone |
| :--- | :--- | :--- |
| Weather and other HTTP data | `process.run` with an HTTP CLI, then `json.decode` | A weather/currency/geolocation capability |
| Audio spectrum | Stream Cava output into Lua state; render with available drawing operations | A native FFT service merely to replace Cava |
| Screen recording | Control an external recorder | Video encoding inside the shell |
| Input display | Stream an external input backend | Global input capture inside the renderer |
| Wallpaper UI | Background `panel`, `image`, watched folders and persisted preferences | A wallpaper service or fixed wallpaper surfaces |
| Compound controls | Lua components over existing nodes | Rust sliders, calendars, launchers or settings panels |
| Preferences | `persistent_table` with config-declared files | A framework-owned settings schema or fixed state file |
| Simple keybinds | `oblisk set` / `oblisk toggle` | Dedicated IPC commands for each panel |
| Lazy popups/windows | Wayland objects are created when shown | A QML-style loader just to defer surface creation |
| Extra platforms/authentication | Current target is a Wayland session shell | X11/I3, Greetd or general PAM conversations without a product requirement |
