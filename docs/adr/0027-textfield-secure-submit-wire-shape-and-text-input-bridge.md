# `textfield`'s wire shape: a distinguished `SecureSubmit` frame, protocol-native submit, one more cross-thread bridge

Grilled against `build-steps.md` Phase 15 item 2 and ADR-0005/0009/0014/0015, checking real prior art (a local clone of Noctalia's from-scratch C++ v5 rewrite, and the actual installed Quickshell AUR build's source) before deciding rather than guessing. Four questions, three real decisions and one correction to ADR-0026:

**Correction to ADR-0026:** it claimed no `wp-text-input-v3` crate exists in this dependency tree at all. Wrong -- `renderer/Cargo.toml`'s existing `wayland-protocols = { features = ["client", "unstable"] }` already gates `text_input::zv3` (`src/wp.rs:522`, `#[cfg(feature = "unstable")]` in the vendored 0.32.13 source). The dependency was already there and already enabled since this crate's original `unstable` feature was added; only the `TextInputService`/scene-node code is missing, not the Cargo dependency.

**Seat binding:** exactly one `wl_seat`, via SCTK's own `seat` module (ADR-0009 already assumes this is available, treating it as standard infrastructure, not a design point). No multi-seat handling exists anywhere else in this codebase either.

**`on_submit`'s trigger is `zwp_text_input_v3`'s own protocol-native `action` event, not a separate `wl_keyboard` listener.** The v2 protocol addition defines `ACTION_SUBMIT`, described in its own XML as "intended for input entries that expect some sort of activation after user interaction" -- exactly `on_submit`, and IME-correct (works with CJK composition, unlike raw keystroke detection). Confirmed against real code, not just the XML: Noctalia's C++ v5 `TextInputService::handleAction` does exactly this (`if (action == ZWP_TEXT_INPUT_V3_ACTION_SUBMIT) m_pendingEdit.submit = true;`, applied on the next `done` event per the protocol's own double-buffering rule). `on_submit` itself was never actually in question -- `build-steps.md`'s original Phase 5 text already named it alongside `on_change`, and ADR-0005 already specifies its no-argument behavior when masked; `docs/oblisk-idl-api-specs.md` §5.2 just never got updated to list it (fixed alongside this ADR).

**Wire shape for the secret: a distinguished `RendererFrame::SecureSubmit { generation_id, capability, action, secret: Vec<u8> }`, never `CommandEnvelope::arguments`.** `CommandEnvelope.arguments` is generic `Vec<serde_json::Value>` -- routing `SecureBuffer`'s bytes through it would produce an intermediate `serde_json::Value`/`String` copy of the plaintext that `.zeroize()` can never reach, directly undermining ADR-0014's entire point. `SecureSubmit` is built once from `SecureBuffer::expose_secret()` at the one sanctioned read site, serialized, sent, and the source `SecureBuffer` is `.zeroize()`'d immediately after the write completes (ADR-0005's own stated requirement). Checked whether Noctalia's C++ v5 rewrite has an equivalent pattern to borrow: it doesn't need one. Noctalia is one process end to end -- `PamAuthenticator::authenticateCurrentUser(std::string_view password, ...)` reads the buffer directly in-process, no serialization boundary exists to cross. This half of the problem is specific to Oblisk's supervisor/renderer process split; no prior art hands us an answer, but none contradicts this one either.

**Cross-thread bridge for `on_change`/`on_submit`: reuse Phase 14/15's proven `std::sync::mpsc` (Wayland thread) → `tokio::sync::mpsc` (socket thread) shape, carrying one keyed edit-diff struct.** `TextInputService` must live on the Wayland/SCTK thread (calloop-driven `Dispatch`, ADR-0009); the Lua `on_change`/`on_submit` callbacks live in the Lua VM on the socket thread (Phase 12 moved `Loader`/`Scene` there) -- the same split `process.run` already solved for output/exit callbacks (docs/adr/0026), so this reuses that bridge and dispatch-registry pattern rather than inventing a new one. The struct shape itself borrows Noctalia's `TextInputEdit` directly: commit text, preedit text, delete-before/after lengths, and a `submit` bool folded into the *same* diff rather than a separate event kind -- confirmed as the right shape by reading `src/ui/text_input_client.h` directly, not guessed. Checked whether Noctalia's own scripting layer needed an equivalent bridge: it doesn't, and its dodge doesn't transfer. `src/shell/lockscreen/lock_screen.cpp` has zero references to Noctalia's Luau plugin layer -- the lock screen is fully native C++, not plugin-scriptable, the same structural sidestep ADR-0005 already ruled out for Oblisk (Lua has to author its own Wi-Fi password field; there's no native-only escape hatch here). Noctalia's plugin API does expose a general-purpose `ui.input` node to Luau for non-secret text entry (`src/scripting/ui_prelude.h`), but never needed a cross-thread bridge for it either, because Noctalia doesn't split Wayland dispatch and script execution across OS threads the way this codebase does -- that split is Oblisk's own choice, not something prior art validates or invalidates.

**Bonus finding, not yet acted on:** while checking Quickshell's current AUR source (`/usr/src/debug/quickshell-git`, no `wp-text-input-v3` wiring exists there at all) for this ADR's questions, its PAM implementation turned up something directly relevant to Phase 15 item 3's still-ungrilled PAM design: `src/services/pam/conversation.hpp` runs the real `pam_authenticate` conversation in a forked subprocess, with the reason stated in its own source comment -- "PAM has no way to abort a running module except when it sends a message, meaning aborts for things like fingerprint scanners and hardware keys don't actually work without aborting the process... so we have a subprocess." This codebase already has `process::spawn_group_leader`/`reap_process_group` proven three times over (renderer boot, PBA candidates, `process.run`); PAM-as-a-fourth-caller is now a strong, evidence-backed default for that future grill round rather than a guess.

## Upgrade path

Not yet built: `TextInputService` itself, the `textfield` scene-node kind (`layout::scene::ensure_supported_kind` still rejects it), the seat binding, the `RendererFrame::SecureSubmit` type and its Supervisor-side receive/dispatch, and the new cross-thread channel pair. This ADR records the shape; implementation is a separate, later pass per this project's established phase-loop workflow.

## Amendment: a `secure_submit` field reads the keyboard, not the text input

This ADR chose `zwp_text_input_v3` for `textfield`, and for `on_submit` it argued the protocol's own
`ACTION_SUBMIT` is "IME-correct (works with CJK composition, unlike raw keystroke detection)". That
reasoning is right for an ordinary field and wrong for a masked one, and Phase 23 is where the
difference stops being theoretical.

text-input-v3 is the client half of a two-sided arrangement. The compositor delivers `commit_string`
only when it has an input method bound, so with no IME running the events never arrive and the field
receives nothing at all. Every `secure_submit` field written against this ADR was therefore unusable
on a bare session, which was invisible for as long as nothing depended on it. A lock screen depends
on it completely: § 6.4's lock surfaces are the only thing on the glass, and a password that cannot
be typed is a session that cannot be unlocked, which `ext-session-lock-v1` then keeps locked on
purpose (ADR-0042).

So a `secure_submit` field takes its bytes from `wl_keyboard` directly, and the `zwp_text_input_v3`
binding is gone rather than kept beside it. Deleting it is the part worth stating plainly, because
this ADR designed it: `secure_submit` turned out to be its only consumer. `on_change` and
`on_submit` were never wired to anything, so what this ADR built was a bridge serving exactly the
one field kind that must not use it. Keeping it bound would have left a live
`ContentPurpose::Password` session next to the keyboard reader, two writers on one secret buffer,
and the IME argument below defeated by the thing it argues against.

The design in this ADR is not withdrawn, only unbuilt. An ordinary, Lua-readable `textfield` is
still what text-input is right for, and wiring one brings the binding back without
`ContentPurpose::Password`. The split is not a workaround:

- **An IME must not see a password.** Composition means the candidate text lives in another process
  and is echoed to a popup. That is a reasonable price for a search box and not one a password field
  may pay, which is why swaylock and hyprlock read xkb directly and no lock screen supports IME
  composition.
- **A masked field has no composition to be correct about.** ADR-0005 already makes its value
  unreadable from Lua and its `on_submit` argument-free, so preedit, candidate windows and
  delete-surrounding-text have nothing to act on. The whole feature set this ADR chose text-input for
  is inapplicable to the one field kind now being carved out of it.

`ACTION_SUBMIT` stays the trigger for an ordinary field's `on_submit`. On a masked field, Enter is,
and Backspace is what `shared::SecureBuffer::pop_char` exists for.

ponytail: with text-input gone, a `textfield` without `secure_submit` now receives nothing at all,
where before it received nothing useful. The ceiling is that the two field kinds are designed for
different input transports and only one is built. The upgrade path is the one this ADR already
specifies, wired for real: `TextInputService`, `on_change`/`on_submit`, and a binding that omits
`ContentPurpose::Password` because that field kind is not a password.
