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
| Capability start acknowledgement | A Renderer asks for each capability exactly once per generation (`CommandSender::start_capability` keeps a `started` set) and nothing acknowledges the ask, so a `StartCapability` lost after it leaves -- a connection dying mid-write -- is lost for the life of that generation. ADR-0156 closed the path that dropped these in bulk, and the lockout it describes is what a lost one costs: `lock` fails as a lockout, not as a missing feature | Acknowledge a start and re-send an unacknowledged one; or make the roster a property of the generation that the Supervisor reconciles, rather than a stream of one-shot asks | Which capabilities a config asks for |
| CLI state cannot run an effect | `oblisk set`/`toggle` writes a `state`, and a surface reading it re-renders. That covers every declarative case -- a modal opens because its `visible` follows the state -- but not an imperative one. `IPC.qml`'s `rec toggle` starts a recording, which is a call, not a value, and the only handler a config can hang an effect on is a capability's `on_change`; the nearest thing to a timer is `oblisk.system`, so a keybind built that way answers up to a second late | Give a named `state` a change handler, the way a capability has one, so a CLI write can run config code at the moment it lands | Which keybinds exist, and what they do |
| Idle replay after a logind block | `IdleGate::observe` returns before recording, so a seat that went idle *during* a block is not in `idled` and `set_blocked(false)` has nothing to replay. The compositor will not resend `idled`, so that idle period is invisible for its whole length. Code-level only: every live symptom attributed to it in ADR-0159 was later explained by a Wayland surface inhibitor (ADR-0160) or by duplicate Supervisors | Rebuild the affected listeners from the dispatch thread that owns the queue, not from the `BlockInhibited` task; and give the notify half a way to notice a dead listener so a reload repairs one instead of reusing it | Which thresholds a config asks for |
| Idle registrations cannot be cancelled | `register_threshold` has no counterpart, so `dev-config` runs one 1s threshold and rebuilds stage timing on a wall clock in Lua; the file says so at its top. Quickshell gives each `IdleMonitor` its own notification, destroyed with the monitor and recreated when `timeout` changes, which is what makes per-stage delays and chained gates work there. Unregister alone is not enough: the fan-out shares one listener per duration, so a later registration inherits a partly elapsed timer or misses an `idled` the shared listener already sent | Return a cancellable handle that drops the callbacks and releases the listener when it is the last user; give a registration its own notification rather than sharing by duration | Stage order and what "done" means |
| Idle stage timing is wall-clock | `oblisk.system.time` is `SystemTime`, so a clock adjustment moves every armed stage's deadline. The engine publishes no monotonic reading a config could count on instead | Publish a monotonic elapsed value beside `time`, or give timers a Rust owner | Which delays a config picks |
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
| Animation | `animate` eases any number, percent, colour or edge-table property by value shape, with `from` for entry, `animate.exit` for a dropped child, `delay(signal, ms)` for close-hold, paint-only `scale`/`rotate`/`translate`/`origin`, QML's whole easing list beside a cubic Bezier and steps, `keyframes`/`loops` for a sequence, a `delay` lead-in on any entry, `pulse(signal, ms)` to re-fire a one-shot, and `spring` for motion that keeps its speed through a change of target (ADR-0145, ADR-0146, ADR-0149 through ADR-0154); no move transition for the siblings that close the gap | A move transition needs the solver's old and new rects for every sibling. **`spring` was added against this row's own rule and the rule stands: decide against a real consumer before adding any.** The spring itself was kept by a later ruling (ADR-0154 amendment); the rule was not repealed with it. Nothing in `dev-config` or the reference uses a spring, or a `delay` on a spec; both are unexercised outside their tests, and the first config to want either should be read before either grows a knob |

## Keep in config or use existing tools

| Feature | Existing route | Do not build for parity alone |
| :--- | :--- | :--- |
| Weather and other HTTP data | `process.run` with an HTTP CLI, then `json.decode` | A weather/currency/geolocation capability |
| Audio spectrum | Stream Cava output into Lua state; render with available drawing operations | A native FFT service merely to replace Cava |
| Screen recording | Declare the recorder with `session_process` so it survives a reload, and drive it from config | Video encoding inside the shell |
| Input display | Stream an external input backend | Global input capture inside the renderer |
| Wallpaper UI | Background `panel`, `image`, watched folders and persisted preferences | A wallpaper service or fixed wallpaper surfaces |
| Compound controls | Lua components over existing nodes | Rust sliders, calendars, launchers or settings panels |
| Preferences | `persistent_table` with config-declared files | A framework-owned settings schema or fixed state file |
| Simple keybinds | `oblisk set` / `oblisk toggle` | Dedicated IPC commands for each panel |
| Lazy popups/windows | Wayland objects are created when shown | A QML-style loader just to defer surface creation. The evaluation cost that prompted the question was measured on 2026-09-07 and answered by ADR-0157 instead: the 5ms cap guards one outermost signal resolve, not a config evaluation, and the expense was `EvaluationMemo` being scoped to one property rather than one pass. Worst getter went 1.35ms -> 0.57ms under load. Correcting what this row said earlier the same day: the cap does not only blow under a parallel `cargo test` run -- it blew once on a live shell during a build, and the scene kept its prior frame. Deferring surface construction would not have touched it either way, the cost being signal-chain resolution and not surface creation |
| Extra platforms/authentication | Current target is a Wayland session shell | X11/I3, Greetd or general PAM conversations without a product requirement |
