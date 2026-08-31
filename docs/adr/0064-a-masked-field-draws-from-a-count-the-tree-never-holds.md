# A masked field draws from a count the tree never holds

`textfield` painted nothing. `lock.lua` carried a comment measuring exactly what that cost:

> a `textfield` paints nothing today (`paint.rs` skips the kind), so this reserves 28px and
> swallows keystrokes while showing no masked characters at all.

That was recorded as a cosmetic gap. It is not one. Measured on this machine: `pam_unix` answers a
wrong password with `pam_fail_delay`, 2 seconds nominal with a quarter jitter (1671, 2162 and
1905 ms across three runs), and `pam_faillock` is on its built-in defaults of `deny=3` and
`unlock_time=600`. So typing a password into an invisible field means a typo is indistinguishable
from a slow unlock, and three of them lock the account for ten minutes. The lock screen is the one
surface where drawing nothing can lock a user out of their own machine.

## Decision 1: paint takes the character count as an input, not from the tree

The obvious route is to put the masked value where every other drawn string lives: a resolved
property on the node, so `parse_content` picks it up and nothing else changes.

ADR-0005 forbids it, and the reason survives the fact that the value would be mask glyphs rather
than the secret. The typed bytes live in `shared::SecureBuffer`, deliberately outside the Lua VM
and outside the scene; putting anything derived from them into the retained tree means the
Renderer's reconcile, clone and retire machinery starts carrying it, and the boundary stops being
one thing you can point at. `Scene::surface` deep-clones a tree per surface per pass, and
`last_painted` now keeps a display list until the surface changes.

So the count travels beside the tree instead of inside it:

```rust
pub struct SecureField<'a> {
    pub target: &'a node::SecureSubmitTarget,
    pub filled: usize,
}

pub fn build(root: &ResolvedNode, scale: f32, focus: Option<&SecureField>) -> DisplayList
```

`shared::SecureBuffer::char_count` is the only read on it that is not `expose_secret`, and what it
discloses is a length. That is precisely what a row of dots on a screen discloses anyway.

Characters, not `len()`'s bytes. A password with one non-ASCII character would otherwise draw two
or three dots for one keystroke, and a user counting dots against what they typed is the entire
reason the mask is drawn.

## Decision 2: focus is a destination, not a node

`SecureField` carries the `{ capability, action }` pair rather than a node id, because that is what
the input layer already tracks: `wayland::input::FocusedField` is a surface id plus a
`SecureSubmitTarget`, and the node that declared that pair is the focused one.

This also makes the routing rule fall out rather than needing to be restated. A field addressed to
`("network", "connect")` does not fill because the lock screen's field is focused, so a Wi-Fi PSK's
length cannot appear on a lock screen. That is the same rule `retarget_secure_submit` enforces for
the bytes themselves, which exists because the two got crossed once already (see its doc comment).

An unfocused field draws its placeholder, and that is the honest thing rather than a fallback:
changing focus zeroizes the buffer, so there is no typed state anywhere else to represent.

## Decision 3: a keystroke repaints, it does not re-resolve

Typing changes no property in the retained tree, so `re_resolve_if_dirty` has nothing to notice and
`repaint_mapped_surfaces` never ran. Without something, the mask would appear only when an
unrelated push happened to repaint the surface, which on a lock screen with a clock means once a
second.

`App::secure_input_changed` is set by `apply_secure_key` and by `focus_secure_submit`, and the
event loop repaints on it without re-resolving. The tree really is unchanged; only `build`'s input
differs.

This is the first thing to lean on ADR-0063's display list from the other side. The flag says
"some surface may draw differently", and the list comparison in `paint_surface` narrows that to the
one surface holding the field, so a keystroke repaints the lock screen and nothing else. Marking
the scene dirty instead would have re-resolved all eleven surfaces per keypress to move one glyph.

## Decision 4: the PAM service is probed, and falls back

Separately found while measuring the unlock path: `PAM_SERVICE` was the constant `"login"`, the
console-login stack. Between a typed password and an unlocked screen it runs `pam_shells`,
`pam_nologin`, `pam_access`, `pam_time`, `pam_lastlog2`, `pam_motd`, `pam_mail`, `pam_loginuid`,
`pam_keyinit` and `pam_systemd`. Two of those are ways to be locked out rather than merely surplus:
`pam_nologin` refuses authentication while `/etc/nologin` exists, and `pam_shells` refuses an
account whose shell is not in `/etc/shells`. Neither has an opinion about whether the person at the
keyboard is the one who locked the screen.

`packaging/pam.d/oblisk` is the stack Oblisk wants: `auth` and `account`, both `include
system-auth`, so the machine's real password policy still answers. No `session` or `password`
chain, because `run_conversation` calls `authenticate` and then `account_management` and opens
nothing.

`pam_service_in` probes for the installed file and falls back to `"login"` when it is absent. The
fallback is the whole point. PAM answers a missing service out of `/etc/pam.d/other`, which on a
stock Arch install is `pam_deny`, so naming `oblisk` unconditionally would turn "the packager did
not copy one file" into "the lock screen refuses every correct password". Failing closed is the
wrong direction for the one program that can lock a user out of their own machine.

The probe is a `stat` per authentication, which is once per typed password, and is deliberately not
cached: an admin who installs the file should not have to restart the shell, and a Supervisor
holding a stale "no oblisk stack" from boot is worst exactly when the session is locked.

## What this does not fix

The two second delay stays, and should. It is `pam_unix`'s answer to a wrong password, not
something Oblisk imposes, and removing it would be removing brute-force protection from a lock
screen.

`pam_faillock` also stays, and with the field now drawn, three typos are a mistake a user can see
themselves making. Whether a screen locker should be able to lock a user out of their own session
for ten minutes is a real question and an admin's to answer in `faillock.conf`, not one to settle
by quietly dropping the module from the stack this ships.

There is still no caret and no placeholder styling, and the mask is one `draw_line` of repeated
glyphs rather than a real text field. That is enough to see what you typed, which was the whole
defect.
