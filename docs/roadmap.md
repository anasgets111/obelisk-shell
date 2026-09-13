# Framework gaps and scope

Source inspection compared the current implementation with the reference QML config and Quickshell C++ at
`2d3b3e9`. This is a scope guide, not a commitment to full Qt/Quickshell parity or a live hardware validation.
[API](lua-api.md) and [services](services.md) describe
what exists; [decisions](decisions.md) holds history.

Rust owns platform connections, validation, secret handling, resource lifetimes, input and
rendering. Lua owns composition, appearance, user preferences and orchestration. A feature absent
from `dev-config` is not necessarily an engine gap.

## Recommended engine work

Recommendations, not accepted API designs. Correctness comes before feature expansion.

| Area | Current limit | What to do | Keep out of Rust |
| :--- | :--- | :--- | :--- |
| Command authority | Ordinary command dispatch does not enforce the envelope's generation/revision claims | Enforce sender/authority checks; settle stale-revision semantics before relying on them | Generation IDs and validation in Lua |
| Capability start acknowledgement | A Renderer asks for each capability once per generation (`CommandSender::start_capability` keeps a `started` set) and nothing acknowledges the ask, so a `StartCapability` lost after it leaves is lost for the life of that generation. ADR-0156 closed the path that dropped these in bulk; what one loss costs is the lockout it describes, `lock` failing as a lockout rather than as a missing feature | Acknowledge a start and re-send an unacknowledged one; or make the roster a property of the generation that the Supervisor reconciles, rather than a stream of one-shot asks | Which capabilities a config asks for |
| Idle replay after a logind block | `IdleGate::observe` returns before recording, so a seat that went idle *during* a block is not in `idled` and `set_blocked(false)` has nothing to replay. The compositor will not resend `idled`, so that idle period is invisible for its whole length. Code-level only: every live symptom attributed to it in ADR-0159 was later explained by a Wayland surface inhibitor (ADR-0160) or by duplicate Supervisors | Rebuild the affected listeners from the dispatch thread that owns the queue, not from the `BlockInhibited` task; and give the notify half a way to notice a dead listener so a reload repairs one instead of reusing it | Which thresholds a config asks for |
| Idle registrations cannot be cancelled | `register_threshold` has no counterpart, so `dev-config` runs one 1s threshold and rebuilds stage timing from `system.monotonic` in Lua. Quickshell gives each `IdleMonitor` its own notification, destroyed with the monitor and recreated when `timeout` changes, which is what makes per-stage delays and chained gates work there. Unregister alone is not enough: the fan-out shares one listener per duration, so a later registration inherits a partly elapsed timer or misses an `idled` the shared listener already sent | Return a cancellable handle that drops the callbacks and releases the listener when it is the last user; give a registration its own notification rather than sharing by duration | Stage order and what "done" means |
| Text editing | Append/backspace input, plus `on_navigate` and `on_cancel` for the keys a single-line field has no edit for (ADR-0112). No caret movement, selection, undo or IME composition | Native ordinary-field editing and composition; retain the separate secure input path | Form validation policy and SMS/search UI |
| Keyboard and accessibility | Field navigation callbacks, no general focus traversal or accessibility tree | Focusable controls, keyboard activation and accessibility semantics | Widget appearance and panel navigation policy |
| Timers | Clock updates at 1 Hz; no config-owned timer | Cancellable one-shot/repeating callbacks with reload cleanup; no idle redraw loop | Retry intervals, debounce delays and OSD duration |
| Paths and drawing | Boxes, text, images and the rounded arcs a box paints itself with, plus paint-only `scale`/`rotate`/`translate` (ADR-0149) and compositor blur (ADR-0195). No config-facing paths, arcs or gradients | Add the smallest drawing operations needed by a real component; use SVG for static artwork | Dedicated notch, gauge or spectrum widgets |

## Needs discussion before implementation

| Area | Gap | Decision needed |
| :--- | :--- | :--- |
| Process control | `run` streams both pipes and returns an exit code, `detach` lets go of a program entirely (ADR-0188), and `session_process` outlives a reload (ADR-0175) and takes `signal`/`stop`. No stdin writes or explicit cwd/env | Which caller needs each extension? Reload ownership and signalling are settled; what is left is whether anything needs to write to a child after starting it |
| External IPC | `obelisk set/toggle` writes named state and `obelisk call` runs a config's own `action`, printing what the handler returned as JSON (ADR-0197). All of it is one-way in: nothing outside can read a state back or be pushed a change | Does an integration need to read out or subscribe, or is write-and-call enough? |
| General I/O | No native HTTP, socket client/server or arbitrary watched file contents; JSON storage and folder watching exist | Prefer subprocess helpers first. Add native I/O only for demonstrated lifecycle, latency or data-volume needs |
| KDE Connect | No native device/plugin model | Dedicated Supervisor capability versus a helper streaming state; do not expose unrestricted D-Bus just for parity |
| Windows and displays | Workspace summaries, special workspaces and one active client, with `focus` and `toggle_special` as commands (ADR-0119). No window actions, and screens are read-only | Select the required window actions and output settings, then define Niri/Hyprland differences and apply/revert behavior |
| Service depth | MPRIS lacks stop/shuffle/repeat/rate/volume and capability flags; PipeWire lacks channel/peak/link detail; UPower exposes a composite battery | Extend existing capabilities for concrete controls; do not mirror every upstream property |
| Blur and effects | A node asks for the desktop behind it to be blurred and the engine derives the region from where that node is painted, over `ext-background-effect-v1` (ADR-0195). A config *may* supply a fragment shader, but only as an `image.transition` over that node's two endpoints (ADR-0184), and the engine's own cross-dissolve is one of those shaders rather than a special case (ADR-0186). No shadows, no arbitrary masks, and no shader over an arbitrary subtree or as a persistent filter | Whether anything beyond a two-endpoint transition can keep a stable contract. Two endpoints and a progress number can; an arbitrary subtree cannot, which is what keeps that scope closed |
| Capture | No screen/window image or live texture | Build for previews/screenshots only when requested; external recording does not require renderer capture |
| Large collections | Every list item is constructed; no viewport delegate reuse or grid layout. Measured on a cached re-apply at about 32 us a row: 0.53 ms at 12 rows, 3.98 ms at 125, 15.1 ms at 500 (ADR-0191). ADR-0124's hidden-subtree freeze keeps a closed list at zero, so this is the cost of an open one | The window has to be the engine's, read off last pass's solved rects with overscan. Four things block it (ADR-0191): virtualization would have to *require* `key`, which a config can be told but not made to supply; a scrolled-out child must be retained without becoming `leaving`; the content extent has to survive the window; and a `geometry` signal on an unbuilt item goes stale |
| Wayland/input extras | No shortcut inhibition, per-surface idle inhibition, touch gestures or cross-app drag/drop | Pick supported hardware/protocols and an actual consumer; logind inhibition is already available |
| Runtime construction | Top-level declarations change through generation swaps | Keep current topology rules unless dynamic windows require a different lifetime model |
| Fonts and localization | `text.font` sets a family per node, with the global chain behind it as coverage (ADR-0144). No translation API, and `Name`/`GenericName`/`Keywords` are all read unlocalized (ADR-0112) | Decide supported language/font requirements before expanding text and application metadata |
| Animation | `animate` eases any number, percent, colour or edge-table property by value shape, with `from` for entry, `animate.exit` for a dropped child, `delay(signal, ms)` for close-hold, paint-only `scale`/`rotate`/`translate`/`origin`, QML's whole easing list beside a cubic Bezier and steps, `keyframes`/`loops` for a sequence, a `delay` lead-in on any entry, `pulse(signal, ms)` to re-fire a one-shot, and `spring` for motion that keeps its speed through a change of target (ADR-0145 through ADR-0154). A paint-only tween advances where it stands rather than laying the tree out again (ADR-0178). No move transition for the siblings that close the gap | A move transition needs the solver's old and new rects for every sibling. Decide against a real consumer before adding any: `spring` was added against that rule, and the later ruling that kept it did not repeal the rule |

## Keep in config or use existing tools

| Feature | Existing route | Do not build for parity alone |
| :--- | :--- | :--- |
| Weather and other HTTP data | `process.run` with an HTTP CLI, then `json.decode` | A weather/currency/geolocation capability |
| Audio spectrum | Stream Cava output into Lua state; render with available drawing operations | A native FFT service merely to replace Cava |
| Clipboard | `process.detach("wl-copy", { text })`. A Wayland selection belongs to a process that stays alive to serve it, which is exactly what `detach` provides | A clipboard capability, which would have to own that same process |
| Screen recording | Declare the recorder with `session_process` so it survives a reload, and drive it from config | Video encoding inside the shell |
| Input display | Stream an external input backend | Global input capture inside the renderer |
| Wallpaper UI | Background `panel`, `image` with `async`/`retain`/`transition`, watched folders and persisted preferences | A wallpaper service or fixed wallpaper surfaces |
| Compound controls | Lua components over existing nodes | Rust sliders, calendars, launchers or settings panels |
| Preferences | `persistent_table` with config-declared files | A framework-owned settings schema or fixed state file |
| Simple keybinds | `obelisk set` / `obelisk toggle` | Dedicated IPC commands for each panel |
| Lazy popups/windows | Wayland objects are created when shown | A QML-style loader just to defer surface creation. The evaluation cost that prompted the question was `EvaluationMemo` being scoped to one property rather than one pass, and the 5ms cap guards one outermost signal resolve rather than a whole config evaluation (ADR-0157). Worst getter went 1.35ms to 0.57ms under load. Deferring surface construction would not have touched it |
| Extra platforms/authentication | Current target is a Wayland session shell | X11/I3, Greetd or general PAM conversations without a product requirement |
