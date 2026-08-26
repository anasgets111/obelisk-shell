# PAM: `nonstick`, a re-exec worker subprocess, one-shot (not interactive) protocol

Grilled against ADR-0015 and `build-steps.md` Phase 15 item 3, which explicitly calls the PAM crate choice "unresearched" and demands its own spike before wiring in. Checked real prior art (a local clone of Noctalia's C++ v5 rewrite, and the actual installed Quickshell AUR build's source) and searched current crates.io state rather than reusing ADR-0015's four-year-old guess.

## Crate: `nonstick`, not `pam-client`

ADR-0015 named `pam-client` as one candidate. Checked both against the actual requirement (an application driving `pam_start`/`pam_authenticate`, like `login`/`sudo`/polkit's own reference agent -- not writing a PAM module): `pam-client` is the right *direction* (FFI to system `libpam` via `pam-sys`, a `Conversation`-style handler trait) but its last release was July 2022. `nonstick` covers both the PAM-application and PAM-module directions, is actively maintained (latest release March 2026), and its application-side `Conversation` trait is exactly the right shape -- `masked_prompt()` returns `PamResult<OsString>` programmatically, no terminal I/O, no blocking read (confirmed against its actual docs, not just a changelog date).

Disclosed limit, not a defect: `masked_prompt()`'s return type is a plain `OsString`, not zeroizable -- PAM's own C API boundary (`char*`), not something any Rust crate can avoid. The `OsString` handed to `nonstick` is built from `SecureBuffer`'s bytes at the last possible moment inside the `Conversation` impl; the source `SecureBuffer` is zeroized immediately after, matching ADR-0005/0014's "zero right after the sanctioned read" discipline as far as it can reach.

On this actual system: `libpam.so` exists, but no `/etc/pam.d/polkit-1` service file -- matches both Quickshell's and Noctalia's own default fallback to the `"login"` PAM service. Not a guess; `/etc/pam.d/` was checked directly.

## Isolation: a re-exec worker subprocess, not `fork()`, not a third binary crate

Both prior-art projects independently isolate the real PAM conversation in a child process rather than running it in the same process as the rest of the app:

- Quickshell's own source comment (`src/services/pam/conversation.hpp`): "PAM has no way to abort a running module except when it sends a message, meaning aborts for things like fingerprint scanners and hardware keys don't actually work without aborting the process... so we have a subprocess."
- Noctalia's C++ v5 `PamAuthenticator::authenticateCurrentUser` (`src/auth/pam_authenticator.cpp:244`) independently forks too, for the same underlying reason (no comment stating it explicitly, but the shape is identical: fork, run the conversation in the child, pipe a result back, `waitpid`).

Both use a bare `fork()` (no `exec()`), reusing the parent's already-loaded code directly in the child. That's unsafe to copy here: `supervisor` already runs a multi-threaded tokio runtime plus a raw `audio::mixer` OS thread, and `fork()` in a multi-threaded process only duplicates the calling thread -- the child inherits no other threads, and any locks they held stay locked forever. A real `spawn()`-based child (safe: `exec()` replaces the whole image before any of that matters) is required instead.

Decision: re-exec `supervisor`'s own binary via `std::env::current_exe()` (the same resolution `renderer_binary_path()` already uses for the renderer) with a new `OBLISK_PAM_WORKER=1` env var, branching `main()` into a minimal PAM-only code path before any D-Bus/tokio-runtime/audio-thread setup runs. No third binary crate added to the workspace. Reuses `process::spawn_group_leader`/`reap_process_group` as a fourth real caller (matching ADR-0018's promised upgrade path, alongside the renderer boot spawn, PBA candidates, and `process.run`).

## Protocol: one-shot, not interactive

Quickshell's `PamIpcPipes` protocol is bidirectional and live -- the child relays each PAM prompt back to the parent and waits for an answer, built for conversations where answers aren't all known upfront (fingerprint retries, multi-factor). Noctalia's is one-shot: the password is already fully known *before* forking (typed into the lock screen, "authenticate" pressed only after), so the forked child answers every PAM prompt internally with that one pre-supplied value and writes back a single final result.

Oblisk's actual flow matches Noctalia's model, not Quickshell's. Per docs/adr/0027, the password is fully captured client-side via `secure_submit` and crosses as one complete `RendererFrame::SecureSubmit` frame *before* the Supervisor spawns anything PAM-related -- there is no live prompt to relay back to the Renderer, the Supervisor already has the one value it needs.

Decision: the password is written to the worker's stdin once, the pipe is closed immediately after (matching Noctalia's `secureClear` discipline right after each use), and `nonstick`'s `Conversation::masked_prompt()` answers every PAM message with that same pre-supplied value inside the worker. The worker reports exactly one outcome frame back over stdout when the conversation ends -- no request/response round trips, no reuse of `RendererFrame`/`SupervisorFrame` (wrong domain, those are Supervisor↔Renderer wire types; this is Supervisor↔its-own-PAM-worker). The outcome uses Quickshell's own exit-code taxonomy rather than a bare bool, since it distinguishes cases the Supervisor should react to differently: `{Success, StartFailed, AuthFailed, MaxTries, PamError, OtherError}`.

## What's still open

`begin_authentication` currently discards `_identities: Vec<(String, HashMap<String, OwnedValue>)>` -- `authentication_agent_response2(uid, cookie, identity)` (`zbus_polkit`'s real signature, confirmed against its vendored source) needs a real uid/`Identity` parsed from that list, not thrown away. Not designed here; the real caller (the implementation pass) should parse it against what polkitd actually sends, not a guess made ahead of that.

## Upgrade path

Not yet built: the `OBLISK_PAM_WORKER` branch in `main()`, the worker's own minimal `pam_start`/`Conversation` loop via `nonstick`, the one-shot stdin/stdout framing (reusing `shared::framing::write_json_frame`/`read_json_frame` for a new small payload enum rather than hand-rolling Quickshell's raw binary framing -- Oblisk already has that helper, no reason to duplicate it), and wiring `identities` parsing into a real `authentication_agent_response2` call. Implementation is a separate, later pass per this project's established phase-loop workflow.
