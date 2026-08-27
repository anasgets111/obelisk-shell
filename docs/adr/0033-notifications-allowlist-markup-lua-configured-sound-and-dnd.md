# ADR-0033: Notifications advertises an allowlist-parsed capability set, with Lua-configured sound and do-not-disturb

## Status

Accepted

## Context

`docs/oblisk-supervisor-services-dbus.md` §1 specs a FIFO-capped, sanitize-to-plain-text
`org.freedesktop.Notifications` server. `docs/oblisk-idl-api-specs.md` §2.7/§3.2 spec a 20-item
Lua-facing feed and a single `dismiss(id)` write row. Both docs undersell or misstate the real
protocol in several places, and the user's own framing ("a server that has all capabilities")
plus a direct request for `body-markup`/`body-hyperlinks`/`body-images`/`sound` pushed this past
what either doc describes. A round-trip through real prior art (Noctalia v5, a from-scratch
native C++ shell — the closest architectural peer to Oblisk's own from-scratch Rust renderer,
unlike Qt/Quickshell-based shells that get rich text for free from `Text.RichText`) confirmed
`ActionInvoked`'s real signature and that no shell in this space actually implements
client-hint-driven `sound`. The user's own personal Quickshell config (`NotificationService.qml`)
confirmed Qt-based shells advertise markup/hyperlinks only because the framework's `Text`
component renders it for free, and left `body-images` off even there.

## Decisions

**`GetCapabilities` advertises 10 strings: `action-icons`, `actions`, `body`, `body-hyperlinks`,
`body-images`, `body-markup`, `icon-static`, `persistence`, `sound`, `inline-reply`.** Excludes
`icon-multi` only — the base `Notify()` signature has no wire mechanism for a client to hand over
multiple icon sizes per notification, so the capability has nothing to attach to, not merely low
priority. `inline-reply` is a KDE Plasma extension, not a base-spec string, kept because §1.2
already names the `x-kde-reply` hint it rides on. `sound` was initially excluded on the theory that
honoring client-chosen playback was an annoyance vector; reversed once a real precedence rule
closed the concern (below) — there was never a technical reason to reject it, only an unresolved
policy question.

**Body markup is allowlist-parsed into structured spans, not stripped to flat text.** §1.1's literal
"strips out all executable scripts, style tags, and image elements... passing clean plain text" is
replaced with a parser that accepts exactly the five constructs the freedesktop markup spec
defines (`<b>`, `<i>`, `<u>`, `<a href="URL">`, `<img src="PATH" alt="ALT">`) and rejects everything
else exactly as before — scripts and style tags still never survive. Output is
`Vec<NotificationSpan>`: a text run carries `{text, bold, italic, underline, href}`, an image run
carries `{image_path}`. This is a security-relevant, hard-to-reverse call: it swaps a blanket-strip
sanitizer for a five-construct allowlist grammar, the kind of change that's easy to get wrong once
and costly to audit twice.

**`<img src>`, `image-path`, and action-icon names all resolve through one new path-trust
validator — not the spec doc's `system:find_icon`, which has zero implementation anywhere in the
codebase today.** A sender-supplied reference is accepted only as an absolute path under a small
allowlist of trusted directories (`/usr/share/icons`, `/usr/share/pixmaps`,
`$HOME/.local/share/icons`, `$HOME/.icons`), confirmed to exist and be a regular file under the
same size cap as `image-data`, then spooled the same way. A bare theme name (no full XDG
icon-theme resolution exists yet) degrades to no icon rather than pretending to resolve it — same
"degrade gracefully, don't fake success" posture as every other missing-capability path in this
codebase. Closes off arbitrary local-file disclosure through body markup, the one new attack
surface an allowlist parser (versus a full strip) opens up. `system:find_icon` stays a separate,
unbuilt IDL row (`oblisk-idl-api-specs.md` §3.2) — real theme-name resolution is its job, not
this controller's, when it eventually lands.

**Span rendering is out of scope this round — supervisor delivers correct data, renderer draws it
later.** Oblisk's FemtoVG/cosmic-text pipeline (ADR-0012, no code yet) has no rich-text/span
support today, unlike Qt's free `Text.RichText`. Building that is a real, separate slice — the
first real consumer of ADR-0012's glyph-atlas work — tracked as a named follow-up in
`docs/build-steps.md`, not silently deferred. Markup will be correctly parsed and delivered before
it's visually rendered.

**`ActionInvoked` emits the real 2-argument base-spec signal, not the spec doc's invented 3-arg
version.** §1.2's `ActionInvoked(id, "inline-reply", text)` doesn't match the real
`ActionInvoked(id: u32, action_key: string)` every existing D-Bus client (browsers, Signal-desktop)
expects — a 3-arg signal on that name would break real clients' introspection assumptions.
Confirmed via Noctalia's source: reply text rides inside `action_key` itself as
`"inline-reply::<text>"`; a bare `"inline-reply"` with no `::` is treated as malformed and logged,
not acted on. Adopted as-is — a battle-tested convention beats inventing one.

**Sound: Lua sets the per-urgency default, a client's own `sound-file` hint overrides it for that
one notification, `suppress-sound` always wins to silence.** Every reference implementation checked
(Noctalia, general Linux notification-daemon practice) either ignores client sound hints entirely
or plays one fixed bundled sound — none honor per-notification client-supplied sound choice, which
is why the first pass here rejected it too. But there's no real reason to: `notifications:set_sound(
urgency, path)` still registers Lua's default per `"low"|"normal"|"critical"` tier, and Oblisk being
a framework (not a fixed shell config) argues for exposing the client's own preference as data
rather than discarding it. Resolution order per `Notify()` call: `hints["suppress-sound"] == true`
forces silence unconditionally; else a valid `hints["sound-file"]` (resolved through the same
path-trust validator as image paths, no new trust surface) plays instead of the tier default; else
the tier's `set_sound` registration plays if one exists; else nothing plays. `hints["sound-name"]`
(XDG sound-theme name) is not honored — it needs the same theme-name-resolution capability
`system:find_icon` would provide and doesn't yet, the identical YAGNI call already made for bare
icon theme names elsewhere in this ADR, not a new gap. Playback itself is unchanged: a small
one-shot PipeWire stream triggered over an internal Rust channel from the notifications controller,
no Lua/wire round-trip for the trigger, matching the user's own framing ("a signal from notif
service to audio").

**Do-not-disturb is a real backend, not a config-doc footnote.** `notifications:set_dnd(bool)`
toggles a Supervisor-held global boolean (not per-generation — a user preference, not
reload-scoped registration state). It gates sound playback only; `notifications.feed` keeps
receiving everything regardless, since Oblisk has no supervisor-owned popup/toast concept to
suppress and suppressing feed delivery would make a missed notification unrecoverable once DND
turns back off. Critical-urgency notifications bypass DND for sound, mirroring every real
DND/Focus-mode implementation (GNOME, KDE, macOS) and the existing critical-bypasses-expire_timeout
rule below. State lives in Supervisor memory only this round — surviving a full Supervisor restart
needs `docs/oblisk-supervisor-services-dbus.md` §14's XDG atomic state manager, spec'd but with zero
code today (Phase 17). Pushed to Lua as `notifications.dnd: boolean` in the same `StateSnapshot` as
the feed, no new wire shape.

**Critical urgency ignores `expire_timeout`.** Common daemon convention (mako, dunst): a
critical-urgency notification persists until explicitly dismissed regardless of what
`expire_timeout` the app requested. The base spec doesn't mandate this; it's a policy choice this
ADR fixes for consistency with the DND-bypass rule above (both trace to the same "critical means
stay until acknowledged" intent).

**Wire shape reuses `StateSnapshot`, no new `SupervisorFrame` variant.** Unlike idle
(`ADR-0032`'s `IdleEvent`, genuinely edge-triggered with no natural poll target), every
notifications mutation — new notify, dismiss, reply, DND toggle — changes the feed list or its
sibling `dnd` flag directly. A fresh `StateSnapshot{capability: "notifications"}` push after each
mutation is the same proven shape as tray/network/bluetooth, not a new pattern.

**SHM path gets the `$UID` fix ADR-0031 already established for tray.** §1.1's literal
`/dev/shm/oblisk-notifications/notif-{id}.png` repeats the exact multi-user-unsafe mistake
ADR-0031 corrected for tray icons. This round uses `/dev/shm/oblisk-$UID/notifications/notif-{id}.png`,
matching that precedent exactly rather than re-litigating it.

**`notifications.feed`'s 20-item cap is a truncated view over the 100-item backing FIFO, not an
independent structure.** The Supervisor keeps 100 in memory so `dismiss(id)`/`reply(id, text)`
still resolve an item that's scrolled out of Lua's visible-20 window; Lua only ever sees the
newest 20.

**Icon file lifecycle is tied to the queue's own remove/replace paths.** A `replaces_id` update
with no fresh `image-data`/`image-path` clears `icon_path` and deletes the old file rather than
leaving a stale image attached to new text. A FIFO eviction past the 100-item cap deletes the
evicted item's spooled file in the same step that drops it from the queue, preventing unbounded
`/dev/shm` growth over long uptimes.

**Missing wire-contract rows added**: `notifications:reply(id, text)`, `notifications:set_sound(urgency,
path)`, `notifications:set_dnd(enabled)` join `notifications:dismiss(id)` in §3.2's write table.
`urgency: "low"|"normal"|"critical"`, `has_reply: boolean`, and the span-array `body` replace the
flat-string `body`/missing-`has_reply` gap in §2.7's read shape.

## Consequences

- Markup parsing and rendering are two separate slices landing in two separate rounds — Lua authors
  get correct structured data before they get a widget that visually honors it. `build-steps.md`
  needs an explicit follow-up line so this gap doesn't read as "forgotten" to a future reader.
- The allowlist parser is new attack surface (five-construct grammar plus a path-trust boundary for
  `<img src>`) in exchange for markup support neither the base spec's typical implementations nor
  the user's own reference config actually ship. Warrants extra scrutiny in the correctness review
  pass, not a free feature.
- Do-not-disturb and per-urgency sound are both real, Lua-facing backend features with no
  prior-art precedent found anywhere researched — genuinely new design, not a port of an existing
  pattern, so their correctness rests entirely on this ADR's reasoning rather than a confirmed
  external reference.
- Notifications joins idle (ADR-0032) as a controller whose `StateSnapshot` push fires on more
  triggers than "Lua asked for fresh state" — every dismiss/reply/DND-toggle is itself a state
  change worth an unprompted push, same pattern as tray icon updates.

## Upgrade path

- DND surviving a full Supervisor restart, once Phase 17's XDG atomic state manager exists.
- Renderer-side span rendering (bold/italic/underline font weight, clickable hyperlink regions,
  inline image layout) — the first real consumer of ADR-0012's glyph-atlas work, tracked separately
  in `build-steps.md`.
- Per-app (not just per-urgency) sound registration, if a real product need surfaces one.
