# ADR-0034: Keyboard backlight/locks/layout, camera privacy, and native Arch update checking

## Status

Accepted

## Context

Five hardware-fact capabilities the user asked for by name — keyboard backlight, keyboard
lock state (caps/num/scroll), keyboard layout, a camera-in-use privacy indicator, and Arch
package update checking — none of which either spec doc or `docs/build-steps.md`'s Phase 16
scope names. Same shape as Notifications' growth past spec (ADR-0033): real, user-requested
work following the proven D-Bus/hardware-controller pattern, not a build-steps-scoped item
waiting to be picked up.

The design grill ran against real prior art: the user's own dotfiles
(`github.com/anasgets111/dotfiles/quickshell/.config/quickshell/`), Quickshell itself, and
Noctalia v5 (a from-scratch native C++ shell, the closest architectural peer). Two claims made
mid-design turned out wrong on verification. This ADR records both alongside what replaced
them, so a future reader hitting the same instinct finds the answer already settled.

## Decisions

**No new "adapter" trait unifying all five.** The five mechanisms share nothing structurally
(D-Bus signal listener, PipeWire registry callback, sysfs+evdev, subprocess+timer, compositor
socket IPC). Every existing Phase 16 controller already follows an identical *convention*
without a trait: own connection/thread, push into a channel, `main.rs` `select!`s the receiver.
Forcing a trait before any caller needs `Vec<Box<dyn Adapter>>` polymorphism is Speculative
Generality by this repo's own review-skill definition — the user agreed and dropped the framing.

**IDL grouping is domain-split, not one flat bucket.** `oblisk.keyboard` (already declared,
unbuilt) grows to hold `backlight_pct`, `caps_lock`, `num_lock`, `scroll_lock`,
`active_layout`, `active_layout_index`, `layout_count`, plus write actions
`keyboard:set_backlight(pct)` and `keyboard:switch_layout(index)`. New `oblisk.privacy`
(`camera_users: table`). New `oblisk.updates` (`count`, `packages`, progress fields, plus
`updates:install()` and `updates:configure({interval})`). Matches every existing precedent
(`oblisk.battery`, `oblisk.audio`) — domain-named, never lumped.

**Keyboard backlight rides UPower, not sysfs.** `org.freedesktop.UPower.KbdBacklight`. Live
`busctl --system introspect` against this dev machine (verified, not assumed from UPower's
docs) found a narrower real interface than first proposed: a single fixed object at
`/org/freedesktop/UPower/KbdBacklight` — `GetBrightness() -> i`, `GetMaxBrightness() -> i`,
`SetBrightness(i)`, signal `BrightnessChanged(i)` (plus `BrightnessChangedWithSource(i, s)`,
unused — nothing here needs the source string). No `EnumerateKbdBacklights`, no
`SetPercentage`, no `DeviceAdded`/`DeviceRemoved` — those don't exist on this UPower version;
there is exactly one keyboard-backlight object, not a registry of them, so there's nothing to
enumerate or hotplug. `GetMaxBrightness()` is read once at controller construction (the same
resolve-once precedent ADR-0035's hwmon chip resolution and `IdleController`'s degrade-once
already established) and cached — a keyboard's brightness step count doesn't change at
runtime. `backlight_pct` is derived Supervisor-side as `round(100 * brightness / max)`
(round-half-away-from-zero, matching ADR-0035's `round_milli_c` precedent) since the D-Bus
interface itself has no percent notion, only a raw `[0, max]` scale — this machine reports
`max = 3`, so real hardware can be this coarse. `keyboard:set_backlight(pct)` converts the
other direction (`round(pct * max / 100)`, clamped to `[0, max]`) and calls `SetBrightness`.
A machine with no keyboard backlight at all fails the constructor's `GetMaxBrightness()` call
(no such D-Bus object) — degrades to `backlight_pct = -1` (the same "sentinel, not a fabricated
zero" convention `temp_gpu` already established), logged once, `set_backlight` becomes a
no-op. Reuses the already-open system D-Bus connection NetworkManager/BlueZ/polkit/idle-inhibit
share (same precedent `docs/build-steps.md`:544 already established for idle inhibit).

**Keyboard lock state revises §5's own sourcing — and both the first- and second-choice
mechanisms were wrong, in different ways.** §5 currently specs `caps_lock` via compositor
socket interception. Real-world check: Hyprland's own `hyprctl devices -j` has no `scrollLock`
field at all, so that route caps out at caps+num on Hyprland alone and is compositor-specific
besides. The first fallback idea proposed here, `wl_keyboard.modifiers`'s `mods_locked`
bitmask, was checked and rejected: that event is gated by Wayland surface focus, and the
Supervisor is a background daemon, not a focused surface — it would never fire.

The next idea — sysfs LED nodes (`/sys/class/leds/*::{caps,num,scroll}lock/brightness`)
primary, inotify-watched for live updates, evdev `EV_LED` as the fallback only when no
matching LED sysfs node exists — turned out backwards on the one point that matters most:
**live-tested on this dev machine (physically toggling Caps Lock twice, watched with
`inotifywait -m`), the sysfs `brightness` file's value genuinely changed (`0`→`1`→`0`,
confirmed by direct reads) but fired *zero* inotify `MODIFY` events.** This kernel's
`input_leds` driver (the one that creates these LED classdevices from a keyboard's `EV_LED`
capability) doesn't call `sysfs_notify()` when it drives a brightness change itself — inotify
on this path is silently dead, not a corner case. Direct evdev `EV_LED` reads (pure-Rust
`evdev` crate, matching this codebase's `hound`/`png` pure-Rust-over-FFI preference) don't have
this problem: the event stream *is* the kernel's own live-notification mechanism for exactly
this state, the same signal that drives the LED hardware in the first place — confirmed
reliable by the same live toggle test. Waybar's own `keyboard_state.cpp`, the most widely-
deployed implementation of this exact feature, already uses evdev for precisely this reason.

**Fixed design: evdev is primary, sysfs is a static (non-live) fallback, not the reverse.**
`evdev::Device::open` on the keyboard device (selected by `supported_leds()` capability —
whichever device reports `LED_CAPSL`) gives both the initial state (`get_led_state()`) and
every live change (`into_event_stream()`'s `EV_LED` events, whose own carried value needs no
re-read) in one mechanism. Sysfs stays in the design for the one thing it's actually good at —
permission: this dev machine's LED `brightness` files are root-owned but world-*readable*
(`-rw-r--r--`), needing no group membership at all, while `/dev/input/eventN` needs `input`
group or `uaccess` (confirmed present and sufficient on this machine, but per the ADR's own
Consequences section, not guaranteed everywhere). If evdev can't be opened (permission denied,
or no LED-capable device found), sysfs is read once at construction for a best-effort static
initial value — real state, just frozen after boot, degrading gracefully rather than reporting
nothing. If neither resolves, all three lock fields default `false`, logged once.

**Camera privacy: kernel-level detection primary, PipeWire supplementary — the reverse of the
first proposal.** Initial research read Noctalia's PipeWire `Video/Source` node classification
as strictly better than the dotfiles' `inotifywait`+`fuser` approach. The user corrected this
directly from experience ("camera is usually not reported by pipewire, most apps use v4l2"),
and it checked out: PipeWire only sees camera access routed through the `xdg-desktop-portal`
Camera portal or the opt-in `pw-v4l2` `LD_PRELOAD` shim — raw V4L2 opens (mpv, ffmpeg, OBS's
native v4l2 source, most native Linux apps) never touch it. The two mechanisms cover disjoint
app populations, not overlapping ones. Fixed design: kernel-level detection is primary — the
`/sys/class/video4linux/video<n>/streaming` flag (kernel 6.3+) where present, else
`inotifywait -e open,close`-equivalent plus a `fuser`-style confirm (inotify alone can't tell
"one handle closed" from "device free"). PipeWire's `Video/Source` node classification extends
the *already-running* `audio::mixer` registry thread (no second PipeWire connection) purely as
a **name-enrichment** layer: kernel detection proves `camera_active`; PipeWire supplies a real
`application.name` for whichever active user it happens to see (portal-routed apps); a raw
v4l2 user with no matching PipeWire link falls back to a `/proc/<pid>/comm` lookup off the
`fuser`-reported PID. `privacy.camera_users: table`, array of `{app_name}`, empty = inactive —
richer than the dotfiles' bare boolean, free given PipeWire data already in hand for its subset.

**Arch updates: the `alpm` crate, not `checkupdates`+`expac` subprocesses — empirically
verified, not assumed.** `alpm` (`github.com/archlinux/alpm.rs`, the Arch org's own binding to
`libalpm`, 1.5M+ downloads, used in production by `paru`) replaces both subprocess calls.
`checkupdates` wraps its sync step in `fakeroot`; whether that's a hard requirement of the sync
operation itself, not just the `pacman` CLI's own policy, was an open question resolved by
building and running the real thing rather than trusting research inference: a throwaway
program copied `/var/lib/pacman` to a user-owned temp dir, called `Alpm::new(root, db_path)`,
registered `core`/`extra`/`multilib` sync repos, and called `syncdbs_mut().update(force: true)`
— the actual network sync step. It ran as `uid=1000`, no `fakeroot`, no root, downloaded a real
8.9MB `extra.db` from a live mirror, and found the same 3 outdated packages the real
`checkupdates` binary reports on the same machine. `fakeroot`'s reason for existing is the
`pacman` CLI's own privilege-check policy, not a structural requirement `alpm` inherits by
binding `libalpm` directly. One honest gap: not independently confirmed against `libalpm`'s C
source, only against this repo's real behavior — flagged, not blocking.

**Arch updates: fully separate service from §11's `sysinfo` scheduler, not a fourth metric on
it.** Same interval-suspend *shape* (`updates:configure({interval})`, suspend at `interval=0`)
for consistency, zero shared code — the user asked for these to have nothing to do with each
other, and §11 itself is unbuilt and out of scope here regardless.

**Arch updates: full signal shape, not count-only.** `updates.count`, `updates.packages`
(`[{name, old_version, new_version, download_size, installed_size}]` — the size fields cost one
more `alpm` package-metadata read per poll, no subprocess, so the earlier "minimal to avoid a
second subprocess" objection doesn't apply once `expac` itself is gone), `last_successful_check`,
`check_error`.

**Arch updates: the capability owns install + progress, not bare Lua `process.run`.** First
proposal was "no install action, Lua calls `process.run` itself" — wrong per the dotfiles' own
`UpdateService.qml`, which owns triggering and progress tracking in the service layer
(`currentStep`, `currentPackage`, `progressDeterminate`, `errorMessage`, `rebootRequired`), never
pushing raw subprocess output to the UI layer to parse. `updates:install()` builds on the same
piped-subprocess primitive `process::spawn_group_leader_piped` already provides internally, with
its own progress fields riding the `updates` signal. Installing needs root; routes through
Oblisk's existing polkit agent (`dbus::polkit`, Phase 5) for privilege elevation — the first
Phase-16-style write action that needs it, not a new escalation mechanism.

**Keyboard layout gets a real compositor trait now, deliberately narrow.** Unlike the "no
adapter trait" call above, the user asked for this one built ahead of the (still-unbuilt)
workspace adaptor (§10's "Rust Workspace Trait") it will likely one day sit beside. Scope is
exactly what layout needs today — `active_layout(&self) -> Signal<String>`,
`switch_layout(&self, index: usize)`, `kind(&self) -> CompositorKind` — not widened to guess
workspace's eventual method surface (active window, focus events); that's the same
speculative-generality call as the "no adapter trait" decision, just one level down. Two
implementors, both built this round: Hyprland (`.socket2.sock`'s `activelayout` event as a
"something changed" trigger, full state resynced via `hyprctl -j devices`, write via
`hyprctl switchxkblayout <device> <index>`) and Niri (JSON-RPC `KeyboardLayouts` query,
event-driven off `KeyboardLayoutSwitched`, write via `{"SwitchLayout":{"layout":<index>}}`).
Both dotfiles' compositor implementations are real, populated, actively used — not
hypothetical. Hyprland's live verification happens on the user's own machine later; Niri is
live-tested in this environment. The Supervisor picks an implementor at startup by probing
`$HYPRLAND_INSTANCE_SIGNATURE`/`$NIRI_SOCKET` (the env vars each compositor itself sets for
every session process — no socket probing). Neither set: `active_layout` degrades to
unavailable, the same "degrade gracefully, don't fake success" posture as every other
missing-capability path in this codebase.

**Keyboard layout: single primary device, index-based write only.** IDL §2.1 already declares
`active_layout` as a singular string, and Noctalia tracks main-device only — no reason to
diverge into per-device tracking. Write is `keyboard:switch_layout(index)` alone; no separate
next/prev actions, since `active_layout_index` + `layout_count` in the read signal let Lua
compute cycling itself. Both compositors natively support index-based *write* selection already
(confirmed, not assumed: `hyprctl switchxkblayout <device> <index>` and niri's
`{"SwitchLayout":{"layout":{"Index":<index>}}}` both take a real index), so switching isn't
extra implementation cost.

**Correction (Spec review, implementation round): reading `active_layout_index` back is not
symmetric between the two compositors, and Lua-side cycling only actually works on Niri today.**
Niri's `KeyboardLayouts` event gives a names list plus a current index directly — trivial to
mirror into `active_layout_index`. Hyprland's `hyprctl -j devices` gives only `active_keymap` (a
human-readable name, e.g. `"English (US)"`) and `layout` (a comma-separated XKB-code list, e.g.
`"us,ara"`), with no code↔name correlation table in the JSON to place `active_keymap` at a
position in `layout`. So on Hyprland, `active_layout_index` cannot be derived from anything
`hyprctl` reports and is left at its last-known value (`0` until the user manually confirms
otherwise on their own machine) — `layout_count` is still accurate (Hyprland does report the
configured layout count correctly), but Lua's "compute cycling from index + count" strategy
silently can't cycle correctly on Hyprland as shipped. This is a disclosed, structural gap, not
a bug to fix later: closing it needs either a Hyprland-side config convention Oblisk can rely on
(a fixed layout order Oblisk itself controls, matching XKB variant strings against configured XKB
layouts) or an upstream Hyprland IPC change, neither of which exists today. Flagged for the user
to verify and, if it matters to them, address on their own Hyprland machine.

**Module layout: a new `supervisor/src/hardware/` tree, sibling to `dbus/`.** None of these
five are D-Bus interfaces (only backlight even touches D-Bus, and only as one client among
several already on the shared connection) — `dbus/` would misdescribe them, the same problem
`idle.rs` already has today (mostly Wayland-protocol code, one D-Bus proxy riding along).
`hardware/{keyboard/{backlight,locks,layout,controller}.rs, idle/}`, top-level `privacy/` and
`updates/`, each split one file per concern — executed alongside this ADR as a pure,
behavior-preserving reorganization (see the accompanying reorg work), not folded into this
decision's own scope.

## Consequences

- `alpm` is a real `libalpm` FFI binding, not pure Rust — a different tradeoff than
  `hound`/`png`, but the library is guaranteed present on any Arch system (`pacman` itself
  needs it), so it's not a new install-time dependency in practice.
- The `evdev` crate's fallback path needs read access to `/dev/input/event*`. Modern
  `systemd-logind` grants this via its per-seat `uaccess` udev tag without explicit
  `input`-group membership; older/non-systemd setups may not have it. Worth a runtime
  degrade-and-log if the open fails, not a hard requirement — same posture as every other
  missing-capability path.
- Camera detection is inherently two-tier: kernel-level proves *whether* the camera is active
  for every app; PipeWire only names *which* app for the portal-routed subset it can see. A raw
  v4l2 user with an unhelpful `/proc/<pid>/comm` (e.g. a wrapped binary) may show a less
  friendly name than a PipeWire-routed one. Documented behavior, not a bug to chase.
- `updates:install()` is the first Phase-16-style write action requiring privilege elevation
  beyond what any D-Bus session-bus controller has needed so far — real exercise of the
  existing polkit agent from a second call site, not new machinery.
- The keyboard-layout compositor trait is deliberately narrow. Whether it grows to cover
  workspaces or gets superseded by a separate, wider trait is an open question left to whoever
  designs §10, not foreclosed here.

## Upgrade path

- AUR helper support (`paru`/`yay`) for `updates`, if a real need surfaces — this round is
  repo-only, matching the only real prior art found anywhere.
- `camera_users` naming could get richer (desktop-file-ID matching instead of raw
  `/proc/<pid>/comm`) if the plain-`comm` name proves too unfriendly in practice.
- The `/sys/class/video4linux/video<n>/streaming` fast-path flag: implemented as a `/proc`
  fd-scan only (verified live end-to-end on this dev machine: real inotify `OPEN`/`CLOSE`
  events on `/dev/video0`, a real opener correctly detected and named). This machine's real UVC
  webcam doesn't expose the `streaming` attribute despite kernel 7.1, so the flag couldn't be
  verified live and was dropped rather than shipped untested — the fd-scan is sufficient on its
  own regardless, since `camera_users` needs real opener pids either way. Worth adding as a
  cheap early-exit skip-the-scan optimization on hardware that does expose it, once such a
  machine is available to verify against.
- Workspace adaptor (§10), once designed, decides whether it extends `CompositorLink` or
  defines its own trait alongside it.
