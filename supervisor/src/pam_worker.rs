//! Real PAM conversation (build-steps.md Phase 15 item 3, closing docs/adr/0015 item 1).
//! Both halves of docs/adr/0028's design live in this one file: they share the wire protocol
//! (`shared::PamOutcome` over `shared::framing`) and the PAM service name, and neither half is
//! large enough on its own to earn a separate module.
//!
//! - [`run_worker`] is the *worker* side: it runs only when this binary is re-exec'd with
//!   `OBLISK_PAM_WORKER=1` set (see `main.rs`'s branch ahead of any D-Bus/tokio-runtime/
//!   audio-thread setup). It drives one blocking `nonstick` PAM transaction against the
//!   pre-supplied password read from its own stdin, then writes a single [`shared::PamOutcome`]
//!   frame to its stdout and exits.
//! - [`drive_pam_and_respond`] is the *spawn* side: it runs in the normal, already-async
//!   Supervisor process. It resolves the polkit challenge's uid to a username, re-execs this
//!   same binary as a worker (via [`crate::process::spawn_group_leader_stdio_piped`]), exchanges
//!   the password/outcome over the worker's piped stdin/stdout, and reports the result back to
//!   polkitd via `AuthenticationAgentResponse2`. [`authenticate_current_user`] is its sibling for
//!   the session lock, which has no polkit challenge and no polkitd: the same worker exchange,
//!   the uid taken from this process's own owner, the outcome returned rather than reported
//!   (docs/adr/0052).
//!
//! ADR-0028: no reuse of `RendererFrame`/`SupervisorFrame` for the worker protocol -- that's the
//! wrong domain (Supervisor<->Renderer), this is a completely different process boundary
//! (Supervisor<->its own re-exec'd PAM worker).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use nonstick::{ConversationAdapter, Transaction};
use tokio::sync::mpsc::UnboundedSender;

/// The PAM service this worker authenticates against. This system has no
/// `/etc/pam.d/polkit-1` -- `"login"` is the disclosed fallback (ADR-0028), matching both
/// Quickshell's and Noctalia's own default. Not configurable: no spec asks for that, YAGNI.
const PAM_SERVICE: &str = "login";

/// Ceiling on the whole write-password/read-outcome exchange with the worker (`exchange_over`),
/// not on any single PAM call inside it. Generous relative to `reload::PbaTimings`' 2-3 second
/// deadlines (which bound this codebase's *other* rare-and-bounded inline-`.await`ed operation,
/// a PBA swap) because PAM itself is human-paced and can legitimately be slow (a network-backed
/// auth module, a fingerprint retry loop) in a way a local Wayland handshake never is -- but it
/// must still be bounded. Without this, a wedged worker (the exact scenario `exchange_over`'s own
/// tests already simulate -- "a PAM module blocked on an unreachable network auth backend") never
/// returns from `read_json_frame`, and since `drive_pam_and_respond` is `.await`ed directly
/// inside `main.rs`'s top-level `select!` (not `tokio::spawn`ed), that single hang stalls the
/// entire Supervisor: no more polkit challenges, no more Renderer frames, no more process
/// reaping, until the process is killed from outside (Correctness review finding). The lock
/// screen's [`authenticate_current_user`] no longer runs on that loop (`main.rs` spawns it, see
/// its own doc comment), but it wants the same ceiling for a different reason: an unbounded
/// exchange there is a task and a plaintext copy of a password that never go away.
const PAM_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oblisk's flow only ever has one pre-supplied password known before the conversation starts
/// (ADR-0028 -- matches Noctalia's model, not Quickshell's live-relay model), so
/// [`ConversationAdapter::masked_prompt`] always answers with it regardless of the actual
/// prompt text, and `prompt`/`radio_prompt`/`binary_prompt` (regular/non-masked/extension
/// prompts) are never expected in this flow -- `ConversationError` is the correct response if
/// PAM ever asks one anyway.
///
/// The `OsString` built in `masked_prompt` is a plain, non-zeroizable copy of the password --
/// an unavoidable consequence of PAM's own C API boundary (`char*`), not something this code
/// can avoid. Everything on this side of that boundary is zeroized: the *source* `Vec<u8>` in
/// [`run_worker`] right after the whole conversation (`authenticate` + `account_management`)
/// completes, not per-call, since `masked_prompt` may be invoked more than once per conversation
/// (e.g. a PAM module configured to retry) -- and this struct's own `password` (a second,
/// independent copy `run_conversation` hands to `TransactionBuilder::build`, which loses direct
/// access to it once ownership moves into the `Transaction`) is zeroized explicitly by
/// `run_conversation` itself right after the transaction's PAM calls finish, via the shared
/// `Rc<RefCell<_>>` handle it keeps -- `ConversationAdapter::masked_prompt` only ever takes
/// `&self`, so a plain owned `Vec<u8>` here could *only* ever be zeroized from `Drop`, exactly
/// the "Drop as the only mechanism" ADR-0005 distrusts, not "Drop as a backup" like every other
/// secret buffer in this codebase. The `Drop` impl below still runs (harmless on an
/// already-zeroized buffer) as the same backup-of-last-resort `SecureBuffer` itself uses, for a
/// path where `run_conversation` never reaches its own explicit call (e.g. an FFI panic).
struct PasswordConversation {
    password: Rc<RefCell<Vec<u8>>>,
}

impl Drop for PasswordConversation {
    fn drop(&mut self) {
        shared::Zeroize::zeroize(&mut *self.password.borrow_mut());
    }
}

impl ConversationAdapter for PasswordConversation {
    fn prompt(&self, _request: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        Err(nonstick::ErrorCode::ConversationError)
    }

    fn masked_prompt(&self, _request: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(self.password.borrow().clone()))
    }

    fn error_msg(&self, message: impl AsRef<std::ffi::OsStr>) {
        eprintln!("pam worker: {}", message.as_ref().to_string_lossy());
    }

    fn info_msg(&self, message: impl AsRef<std::ffi::OsStr>) {
        eprintln!("pam worker: {}", message.as_ref().to_string_lossy());
    }
}

/// Maps a `nonstick` transaction failure to the [`shared::PamOutcome`] variant the spawn side
/// should react to. A pure function so it's directly unit-testable without any real PAM call --
/// see this module's tests.
fn outcome_for_error(err: nonstick::ErrorCode) -> shared::PamOutcome {
    use nonstick::ErrorCode::*;
    match err {
        MaxTries => shared::PamOutcome::MaxTries,
        AuthenticationError | PermissionDenied | UserUnknown | CredentialsInsufficient | CredentialsExpired => shared::PamOutcome::AuthFailed,
        other => shared::PamOutcome::PamError(format!("{other:?}")),
    }
}

/// Drives one whole PAM transaction (`pam_start` via `TransactionBuilder`, then `authenticate`
/// and `account_management`) against `username`, answering every prompt with `password`. Purely
/// synchronous/blocking -- `nonstick`'s FFI calls block anyway, and this has no need of an async
/// executor itself (see [`run_worker`]'s doc comment for why only the final frame write gets
/// one).
fn run_conversation(username: &str, password: &[u8]) -> shared::PamOutcome {
    let password = Rc::new(RefCell::new(password.to_vec()));
    let conversation = PasswordConversation { password: Rc::clone(&password) };
    let outcome = match nonstick::TransactionBuilder::new_with_service(PAM_SERVICE)
        .username(username)
        .build(conversation.into_conversation())
    {
        Ok(mut txn) => {
            if let Err(err) = txn.authenticate(nonstick::AuthnFlags::empty()) {
                outcome_for_error(err)
            } else if let Err(err) = txn.account_management(nonstick::AuthnFlags::empty()) {
                outcome_for_error(err)
            } else {
                shared::PamOutcome::Success
            }
        }
        Err(err) => shared::PamOutcome::StartFailed(format!("{err:?}")),
    };
    // Explicit call, not left to PasswordConversation's own Drop alone (ADR-0005) -- this
    // codebase's own `Rc` clone reaches the same backing bytes regardless of whether `txn` (and
    // the `PasswordConversation` it owns) has already dropped by this point or not; zeroizing an
    // already-zeroized buffer is a harmless no-op, so there's no ordering hazard either way.
    shared::Zeroize::zeroize(&mut *password.borrow_mut());
    outcome
}

/// Reads `reader` (the real caller: stdin, locked) to exhaustion. On an I/O error partway
/// through the read, `read_to_end` can leave real password bytes already sitting in the buffer
/// it was filling -- `?`-ing straight out of that call would drop that partially-filled `Vec`
/// unscrubbed (Correctness review finding: a mid-stream pipe error while the spawn side is
/// writing the password, for instance, would otherwise leak plaintext password bytes into freed
/// heap memory). Zeroize whatever was read so far before propagating the error.
fn read_password(mut reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut password = Vec::new();
    if let Err(err) = reader.read_to_end(&mut password) {
        shared::Zeroize::zeroize(&mut password);
        return Err(err);
    }
    Ok(password)
}

/// The worker side of ADR-0028: runs when this binary is re-exec'd with `OBLISK_PAM_WORKER=1`
/// set (`main.rs`'s branch, ahead of any D-Bus/tokio-runtime/audio-thread setup -- this must
/// never touch any of that). Reads the whole password off stdin (written once by the spawn
/// side, which then closes the write half so this read hits EOF), drives the PAM conversation,
/// zeroizes the password, and writes exactly one [`shared::PamOutcome`] frame to stdout.
///
/// The worker's own PAM-driving logic ([`run_conversation`]) is entirely synchronous/blocking --
/// `nonstick`'s FFI calls block anyway. The single `write_json_frame` call is the *only* thing
/// that needs an async executor, since `shared::framing` is built on `tokio::io::AsyncWrite` --
/// spinning up a full multi-thread runtime for that one write would be wasteful;
/// `new_current_thread()` is the minimal correct choice. The whole worker is deliberately not
/// async around the blocking FFI calls.
pub fn run_worker() -> Result<(), Box<dyn std::error::Error>> {
    let username = std::env::var("OBLISK_PAM_USERNAME")?;

    let mut password = read_password(std::io::stdin().lock())?;

    let outcome = run_conversation(&username, &password);
    shared::Zeroize::zeroize(&mut password);

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let mut stdout = tokio::io::stdout();
        shared::framing::write_json_frame(&mut stdout, &outcome).await
    })?;
    Ok(())
}

/// Logs why `drive_pam_and_respond` is giving up on `cookie` and zeroizes `secret` -- the one
/// step every one of its early-return failure branches shares, factored out so those branches
/// (no matching identity, no passwd entry, a uid-resolution error, a worker failure) don't each
/// repeat the same `eprintln!`-then-`zeroize` pair with only the message varying.
fn deny(secret: &mut Vec<u8>, cookie: &str, reason: impl std::fmt::Display) {
    eprintln!("polkit authentication for cookie {cookie:?} failed: {reason}");
    shared::Zeroize::zeroize(secret);
}

/// The spawn side of ADR-0028: resolves `challenge`'s uid to a username, re-execs this binary
/// as a PAM worker, exchanges `secret` for a [`shared::PamOutcome`], and reports success back to
/// polkitd via `AuthenticationAgentResponse2`. Runs in the normal, already-async Supervisor
/// process -- `main.rs`'s `RendererFrame::SecureSubmit` arm for `("polkit", "authenticate")` is
/// this function's only caller.
///
/// `secret` is zeroized on every return path -- the plaintext must never live longer than
/// necessary and must never be dropped without a `.zeroize()` call first (ADR-0005).
pub async fn drive_pam_and_respond(
    authority: &zbus_polkit::policykit1::AuthorityProxy<'_>,
    challenge: crate::dbus::polkit::BeginAuthenticationCall,
    mut secret: Vec<u8>,
) {
    let Some(uid) = crate::dbus::polkit::first_unix_user_uid(&challenge.identities) else {
        deny(&mut secret, &challenge.cookie, "carried no unix-user identity; cannot authenticate");
        return;
    };

    let outcome = match authenticate_uid(uid, &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => {
            deny(&mut secret, &challenge.cookie, reason);
            return;
        }
    };
    shared::Zeroize::zeroize(&mut secret);

    match outcome {
        shared::PamOutcome::Success => {
            let identity_details: std::collections::HashMap<&str, zbus::zvariant::Value> =
                std::collections::HashMap::from([("uid", zbus::zvariant::Value::from(uid))]);
            let identity = zbus_polkit::policykit1::Identity { identity_kind: "unix-user", identity_details: &identity_details };
            if let Err(err) = authority.authentication_agent_response2(uid, &challenge.cookie, &identity).await {
                eprintln!("authentication_agent_response2 failed for cookie {:?}: {err}", challenge.cookie);
            }
        }
        other => {
            // AuthenticationAgentResponse2 is documented (zbus_polkit's own source, quoted in
            // this module) as "invoke on successful authentication" -- there's no equivalent
            // "report failure" D-Bus call for an agent to make. Not calling it and letting
            // polkitd's own challenge timeout apply is the correct behavior on failure, not a
            // missing case.
            eprintln!("polkit authentication for cookie {:?} did not succeed: {other:?}", challenge.cookie);
        }
    }
}

/// The half [`drive_pam_and_respond`] and [`authenticate_current_user`] share: resolve `uid` to a
/// username and run the whole worker round trip against it. `Err` carries the reason the
/// conversation never happened (no passwd entry, a uid-resolution failure, a worker failure), for
/// each caller to report the way its own protocol demands -- polkitd gets silence and a log line,
/// `oblisk.lock` gets a string on the lock screen.
///
/// Borrows `secret` and never zeroizes it: both callers own that buffer and each already scrubs
/// it on every one of its own return paths (ADR-0005), so scrubbing here as well would make the
/// ownership harder to audit, not the plaintext shorter-lived.
///
/// ponytail: `User::from_uid` is a blocking libc call (`getpwuid_r`), not routed through
/// `spawn_blocking`, inline in this async fn -- same "the main loop blocks for real work,
/// bounded and rare" precedent docs/adr/0025 already established for PBA swaps, and on the lock
/// path it blocks a spawned task's runtime worker thread rather than the `select!` loop itself.
/// A uid lookup is normally local-passwd-file-fast; on this system there's no NSS/LDAP backend to
/// make it otherwise, and adding a `spawn_blocking` hop for a call this cheap and this rare (one
/// polkit challenge or one lock screen submission at a time, ADR-0028 and
/// `LockController::try_begin_authentication`) would be speculative generality, not a fix. The
/// upgrade path if a networked NSS backend ever does appear: wrap this one call in
/// `tokio::task::spawn_blocking`, which needs nothing else here to change.
async fn authenticate_uid(uid: u32, secret: &[u8]) -> Result<shared::PamOutcome, String> {
    let username = match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => user.name,
        Ok(None) => return Err(format!("uid {uid} has no passwd entry")),
        Err(err) => return Err(format!("failed to resolve uid {uid}: {err}")),
    };
    spawn_worker_and_exchange(&username, secret).await.map_err(|err| format!("pam worker failed: {err}"))
}

/// [`drive_pam_and_respond`]'s sibling for the session lock (docs/adr/0042, docs/adr/0052): the
/// same worker exchange with no polkit challenge to read a uid out of and no polkitd to report
/// to. The user being authenticated is this process's own owner, because the Supervisor runs as
/// the session user and the lock screen is that session's.
///
/// Returns the outcome instead of acting on it. `main.rs`'s `secure_submit(lock, authenticate)`
/// arm is the only caller, and it `tokio::spawn`s this rather than `.await`ing it inline the way
/// the polkit arm does its own sibling: an Enter key at a lock screen is neither bounded nor rare,
/// so the outcome comes back to the `select!` loop over a channel (see that arm for the argument).
/// The `pam_outcomes` arm it lands in is the only place allowed to turn a `Success` into an unlock
/// order, which is what keeps ADR-0042's "never call `unlock_and_destroy` except on a successful
/// authentication" a property of one call site.
///
/// `secret` is zeroized on every return path, the same standard [`drive_pam_and_respond`] holds
/// -- and, unlike that function, also on a path that isn't a *return* at all: this async fn is
/// `tokio::spawn`ed by [`run_lock_authentication`] rather than `.await`ed inline in `main.rs`'s
/// `select!`, so it can be dropped mid-`.await` by a runtime shutdown, or have a panic unwind
/// through it (e.g. one of the `.expect()` calls in `write_secret_then_read_outcome` firing if
/// `spawn_group_leader_stdio_piped` ever stopped piping stdin/stdout). Neither is a `return`, so
/// the explicit `shared::Zeroize::zeroize` call below -- reached only on the ordinary path --
/// would not run, and the plain `Vec<u8>` this function used to hold as `secret` directly would
/// drop with the plaintext still intact (Correctness review finding; ADR-0005 requires an
/// explicit `.zeroize()` before *every* drop, not just the happy one). `zeroize::Zeroizing` closes
/// that the same way `shared::SecureBuffer` does for `secure_submit`'s Renderer-side writer: a
/// `Drop` impl as the backstop under an explicit call, not as the only mechanism. It is the
/// `zeroize` crate's own newtype for exactly this, re-exported through `shared` beside `Zeroize`,
/// so nothing here hand-rolls a second `Drop` impl over a plaintext buffer. `SecureBuffer` itself
/// is the wrong shape rather than the wrong idea: its only way to load bytes is `push_str(&str)`,
/// and this secret arrives as a raw `Vec<u8>` off the wire.
///
/// **The wrapper is a parameter rather than something applied in here**, and that is a real leak
/// rather than a style point (Correctness review pass five). A `tokio::spawn`ed future's arguments
/// are captured the moment it is built, but its body does not run until the first poll. Wrapping
/// inside the body leaves a bare `Vec<u8>` owned by a task a runtime shutdown can drop before ever
/// polling it, which drops the plaintext unscrubbed on exactly the never-returns path this wrapper
/// exists for. Wrapping at the call site puts it inside a scrubbing owner for the task's whole
/// life, including a task that never runs at all.
///
/// A failure to even reach PAM becomes `StartFailed`: it is pre-conversation, exactly like the
/// `pam_start` failure that variant was named for, and the lock screen has one `error` string to
/// show either way.
pub async fn authenticate_current_user(mut secret: shared::Zeroizing<Vec<u8>>) -> shared::PamOutcome {
    let outcome = match authenticate_uid(nix::unistd::Uid::current().as_raw(), &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => shared::PamOutcome::StartFailed(reason),
    };
    shared::Zeroize::zeroize(&mut *secret);
    outcome
}

/// Wraps [`authenticate_current_user`] so `outcome_tx` is guaranteed to receive *something* no
/// matter how the `tokio::spawn`ed task built from this future ends: the ordinary path where
/// `authenticate_current_user` returns normally, a panic unwinding out of it, or this future
/// being dropped mid-`.await` by a runtime shutdown before it ever polls to completion.
///
/// Without this wrapper, `LockController::try_begin_authentication` sets `authenticating`
/// (docs/adr/0052) on one side of the spawn, and only a `LockEvent::Authenticated` landing in
/// `main.rs`'s `pam_outcomes` arm ever clears it. A task that never reaches its own `.send()`
/// call (Correctness review finding) leaves that flag a one-way latch: `may_authenticate` stays
/// false forever and every later Enter on the lock screen is dropped (`try_begin_authentication`'s
/// own refusal log line), stranding the session behind the lock with no way back in short of a VT
/// switch.
///
/// [`ReportOnDrop`] is the mechanism. `UnboundedSender::send` is synchronous, so calling it from
/// `Drop::drop` still runs during an unwind -- unlike a second `.await`ed cleanup step, which a
/// panicking task would never reach. This is the choice between the two the review raised: read a
/// `JoinHandle`'s `Result` back in `main.rs`'s `select!`, or let the task's own scope report its
/// own failure. A `JoinHandle` would need that already-large `select!` to grow a second arm and
/// hand-translate a `JoinError` into this same channel; a `Drop` guard keeps the failure path
/// local to the one task that can fail, and changes nothing about the channel's shape or about
/// `main.rs` beyond the one line that used to build this future inline.
/// `acquisition` is the tag `LockController::try_begin_authentication` handed out when this
/// conversation was admitted, carried back untouched with the outcome. This side never interprets
/// it: it exists because an answer outlives the lock it answers for (`pam_unix`'s ~1 second,
/// [`PAM_EXCHANGE_TIMEOUT`]'s 30), and only `lock::accepts_outcome` can say whether the lock it was
/// typed against is still the one on the glass. Carrying it here rather than re-reading the
/// controller's current acquisition in the `pam_outcomes` arm is the whole point -- by then the
/// number has moved.
pub async fn run_lock_authentication(
    secret: shared::Zeroizing<Vec<u8>>,
    acquisition: u64,
    outcome_tx: UnboundedSender<(u64, shared::PamOutcome)>,
) {
    let mut guard = ReportOnDrop { acquisition, outcome_tx: Some(outcome_tx) };
    let outcome = authenticate_current_user(secret).await;
    if let Some(tx) = guard.outcome_tx.take() {
        // A closed channel means `main.rs`'s loop is gone, i.e. the Supervisor is shutting down
        // -- the same posture `LockController::send` takes.
        if tx.send((acquisition, outcome)).is_err() {
            eprintln!("lock: the pam outcome channel is closed; dropping an authentication result");
        }
    }
}

/// [`run_lock_authentication`]'s `Drop` backstop: reports a fallback outcome the one time it is
/// dropped still holding its sender, i.e. on every exit from that function except the ordinary
/// one, which already `.take()`s the sender before sending the real outcome. See that function's
/// own doc comment for why this exists instead of an awaited `JoinHandle`.
struct ReportOnDrop {
    /// Carried so the fallback is bound to the same lock the real answer would have been. A
    /// fallback tagged with anything else would be applied to whatever lock happens to be up when
    /// it lands, which is the case `lock::accepts_outcome` exists to refuse.
    acquisition: u64,
    outcome_tx: Option<UnboundedSender<(u64, shared::PamOutcome)>>,
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.outcome_tx.take() {
            // `main.rs`'s pam_outcomes arm only needs *a* PamOutcome to run LockEvent::Authenticated
            // and clear `authenticating` -- the exact variant doesn't matter here the way it does
            // on the ordinary path, since there is no real PAM answer to report. PamError also
            // means the lock screen's `error` string says something happened rather than nothing.
            let outcome = shared::PamOutcome::PamError("pam authentication task ended without reporting an outcome".to_string());
            let _ = tx.send((self.acquisition, outcome));
        }
    }
}

/// Re-execs this binary (`std::env::current_exe()`, the same resolution `renderer_binary_path()`
/// already uses for the renderer) as a PAM worker for `username`, then delegates the actual
/// wire exchange to [`exchange_over`].
async fn spawn_worker_and_exchange(username: &str, secret: &[u8]) -> std::io::Result<shared::PamOutcome> {
    let exe = std::env::current_exe()?;
    let child = crate::process::spawn_group_leader_stdio_piped(
        &exe.to_string_lossy(),
        &[],
        &[("OBLISK_PAM_WORKER".to_string(), "1".to_string()), ("OBLISK_PAM_USERNAME".to_string(), username.to_string())],
    )?;
    exchange_over(child, secret, PAM_EXCHANGE_TIMEOUT).await
}

/// Writes `secret` to `child`'s stdin (closing the write half so the worker's own stdin read
/// hits EOF), reads exactly one [`shared::PamOutcome`] frame back over its stdout, then reaps
/// the process group -- split out from [`spawn_worker_and_exchange`] so the wire protocol
/// itself (write secret, read one frame, reap) can be tested against a fake child process
/// without spawning a real re-exec'd PAM worker (see this module's tests). The worker exits on
/// its own right after writing the outcome frame in the ordinary case, so the reap below is just
/// cleanup then, not part of the protocol -- but the reap always runs, even when the write or
/// read above fails or times out (a worker that died mid-write, panicked, wrote a malformed
/// frame, or is simply wedged and never responds), rather than short-circuiting past it: a hung
/// or wedged worker (e.g. a PAM module blocked on an unreachable network auth backend) must not
/// be left running untracked just because this function is about to return an error.
///
/// `timeout` bounds the write+read exchange, not the reap that follows it (`reap_process_group`
/// has its own bounded grace period). Without this bound, a worker that never writes anything
/// and never exits would hang `read_json_frame` forever -- and on the polkit path, where this
/// whole call chain is `.await`ed inline in `main.rs`'s top-level `select!` rather than
/// `tokio::spawn`ed, that single hang would stall the entire Supervisor's event loop (Correctness
/// review finding). Takes `timeout`
/// as a parameter, matching `reap_process_group`'s own `grace` parameter, so tests can use a
/// short one instead of [`PAM_EXCHANGE_TIMEOUT`]'s real-world 30 seconds.
async fn exchange_over(mut child: tokio::process::Child, secret: &[u8], timeout: Duration) -> std::io::Result<shared::PamOutcome> {
    let outcome_result = match tokio::time::timeout(timeout, write_secret_then_read_outcome(&mut child, secret)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "pam worker did not respond within the timeout")),
    };

    if let Err(err) = crate::process::reap_process_group(&mut child, crate::process::DEFAULT_REAP_GRACE).await {
        eprintln!("failed to reap pam worker: {err}");
    }

    outcome_result
}

/// The actual write-then-read half of the exchange, wrapped by [`exchange_over`] in a
/// `tokio::time::timeout`. Split out so the timeout wraps a plain, ownership-clean future
/// (borrowing `child` for its duration) that `exchange_over` can still reach `child` through
/// afterward to reap it, whether this future ran to completion or was cancelled by the timeout
/// firing first -- a cancelled future drops everything it owns (here, `child.stdin`'s taken
/// handle), which still closes that pipe end cleanly even mid-write.
async fn write_secret_then_read_outcome(child: &mut tokio::process::Child, secret: &[u8]) -> std::io::Result<shared::PamOutcome> {
    let mut stdin = child.stdin.take().expect("spawn_group_leader_stdio_piped always pipes stdin");
    tokio::io::AsyncWriteExt::write_all(&mut stdin, secret).await?;
    drop(stdin); // closes the write half so the worker's stdin read hits EOF

    let mut stdout = child.stdout.take().expect("spawn_group_leader_stdio_piped always pipes stdout");
    shared::framing::read_json_frame(&mut stdout).await.map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generous timeout for tests exercising the ordinary (non-timeout) paths -- short enough
    /// to keep a hung test from stalling the suite, long enough that it never fires by accident
    /// against these fast, local, `sh -c` fake workers. Distinct from [`PAM_EXCHANGE_TIMEOUT`]'s
    /// real 30-second value on purpose (tests must not depend on that constant's exact size).
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    // outcome_for_error is pure and needs no real PAM call to test.

    #[test]
    fn outcome_for_error_maps_max_tries() {
        assert_eq!(outcome_for_error(nonstick::ErrorCode::MaxTries), shared::PamOutcome::MaxTries);
    }

    #[test]
    fn outcome_for_error_maps_authentication_error_to_auth_failed() {
        assert_eq!(outcome_for_error(nonstick::ErrorCode::AuthenticationError), shared::PamOutcome::AuthFailed);
    }

    #[test]
    fn outcome_for_error_maps_an_arbitrary_other_variant_to_pam_error() {
        assert_eq!(outcome_for_error(nonstick::ErrorCode::SystemError), shared::PamOutcome::PamError("SystemError".to_string()));
    }

    // drive_pam_and_respond / spawn_worker_and_exchange ultimately call real libpam via a real
    // subprocess re-exec, which can't be meaningfully unit-tested without a real PAM stack and
    // a real (or deliberately wrong) system password -- same category as this codebase's
    // existing Wayland dispatch code, which relies on live manual verification rather than unit
    // tests. What *can* and must be tested without any of that is the wire protocol itself:
    // write the secret to the child's stdin, close it, read exactly one PamOutcome frame back.
    // `exchange_over` is exercised directly against a fake "worker" (a shell one-liner that
    // discards stdin and prints a canned frame) rather than a real re-exec'd PAM worker.

    #[tokio::test]
    async fn exchange_over_reads_back_the_worker_s_one_shot_outcome_frame() {
        // Frame wire format (shared::framing): a 4-byte big-endian length prefix, then the
        // JSON payload. `PamOutcome::Success` serializes as the 9-byte JSON string `"Success"`.
        let script = r#"cat > /dev/null; printf '\000\000\000\011"Success"'"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");

        let outcome = exchange_over(child, b"the-password", TEST_TIMEOUT).await.expect("exchange_over failed");

        assert_eq!(outcome, shared::PamOutcome::Success);
    }

    #[tokio::test]
    async fn exchange_over_still_reaps_the_worker_when_the_frame_read_fails() {
        // A "worker" that writes a malformed (undecodable) frame and then hangs, rather than
        // exiting -- simulating a worker that's still alive (e.g. stuck in a blocked PAM module)
        // at the moment its frame turns out to be unreadable. exchange_over's read_json_frame
        // call must fail here, but the process must still be reaped (SIGTERM'd down), not left
        // running just because this function is about to return an error.
        let script = r#"printf '\000\000\000\004evil'; sleep 5"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");
        let pid = child.id().expect("freshly spawned child has a pid") as i32;

        let result = exchange_over(child, b"the-password", TEST_TIMEOUT).await;

        assert!(result.is_err(), "an undecodable frame must surface as an error, not a silent outcome");

        let gone = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the worker (pid {pid}) should be reaped even though exchange_over returned an error, not left sleeping");
    }

    #[tokio::test]
    async fn exchange_over_actually_delivers_the_secret_to_the_child_s_stdin() {
        // A fake "worker" that reports back the exact byte count it read on stdin (via `wc -c`),
        // packed into a canned PamOutcome::PamError frame -- proving exchange_over's write half
        // actually reaches the child (and the write half is closed so the child's read hits
        // EOF), not just that its read half can parse a reply someone else already wrote.
        let script = r#"n=$(wc -c < /dev/stdin); printf '\000\000\000\025{"PamError":"got %s"}' "$n""#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");

        let outcome = exchange_over(child, b"the-password", TEST_TIMEOUT).await.expect("exchange_over failed");

        assert_eq!(outcome, shared::PamOutcome::PamError("got 12".to_string()), "the fake worker must have seen all 12 bytes of the secret");
    }

    #[tokio::test]
    async fn exchange_over_times_out_and_still_reaps_a_worker_that_never_responds() {
        // A "worker" that writes nothing at all and just sleeps -- the scenario this module's
        // own doc comments describe as real (a PAM module blocked on an unreachable network auth
        // backend). Without a timeout, exchange_over's read_json_frame call would hang here
        // forever (Correctness review finding); with one, it must return an error within
        // `timeout` and the worker must still end up reaped, not left running.
        let script = "sleep 30";
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");
        let pid = child.id().expect("freshly spawned child has a pid") as i32;

        let started = tokio::time::Instant::now();
        let result = exchange_over(child, b"the-password", Duration::from_millis(50)).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "a worker that never responds must surface as an error, not hang forever");
        assert!(elapsed < Duration::from_secs(5), "exchange_over took {elapsed:?} -- the timeout did not bound it");

        let gone = tokio::time::timeout(Duration::from_millis(500), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the worker (pid {pid}) should be reaped even after a timeout, not left sleeping for the full 30s");
    }

    #[test]
    fn read_password_zeroizes_whatever_it_already_read_before_a_mid_stream_error() {
        // A reader that hands back real "password" bytes on its first read() call, then fails on
        // its second -- reproducing a pipe error partway through a read_to_end loop. Correctness
        // review finding: read_password must not let those already-read bytes reach the caller
        // (or a dropped, unzeroized Vec) once the error propagates.
        struct FailsAfterFirstChunk {
            chunk: &'static [u8],
            handed_out: bool,
        }
        impl std::io::Read for FailsAfterFirstChunk {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.handed_out {
                    self.handed_out = true;
                    let n = self.chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.chunk[..n]);
                    Ok(n)
                } else {
                    Err(std::io::Error::other("simulated mid-read pipe failure"))
                }
            }
        }

        let err = read_password(FailsAfterFirstChunk { chunk: b"hunter2", handed_out: false })
            .expect_err("a reader that errors mid-stream must surface that error, not silently truncate");

        assert_eq!(err.to_string(), "simulated mid-read pipe failure");
        // The zeroize call itself is a 1-line, directly auditable part of read_password's error
        // branch (see its body just above) -- by the time this test can observe anything, the
        // partially-filled Vec has already been dropped inside read_password, the same
        // Rust-level limit shared/src/secure_buffer.rs's own tests document (proving a *live*
        // buffer was zeroized needs inspecting it before it drops, not after).
    }

    // The next three tests pin the half of the two Correctness review findings that is this
    // module's own code: `authenticating` must be releasable even when the spawned task that
    // would otherwise release it panics or is dropped before reporting. The scrub-on-every-exit
    // half is `zeroize::Zeroizing`'s job now, and testing that crate's `Drop` impl here would be
    // testing someone else's library.

    #[test]
    fn report_on_drop_sends_a_fallback_outcome_when_dropped_before_reporting() {
        // Simulates a panic unwinding out of authenticate_current_user, or run_lock_authentication's
        // future being dropped by a runtime shutdown mid-.await: either way, the guard is dropped
        // without its own `.take()`-then-send ever running.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(ReportOnDrop { acquisition: 7, outcome_tx: Some(tx) });

        let (acquisition, outcome) = rx.try_recv().expect("a fallback outcome must be sent when the guard is dropped before reporting");
        assert!(matches!(outcome, shared::PamOutcome::PamError(_)), "the fallback must be a PamOutcome so main.rs's pam_outcomes arm can still clear `authenticating`");
        assert_eq!(acquisition, 7, "and it must be tagged with the lock it was started for, or lock::accepts_outcome cannot place it");
    }

    #[test]
    fn report_on_drop_does_not_double_send_once_the_real_outcome_already_went_out() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut guard = ReportOnDrop { acquisition: 7, outcome_tx: Some(tx) };
        guard.outcome_tx.take().expect("freshly built guard holds a sender").send((7, shared::PamOutcome::Success)).expect("send failed");
        drop(guard);

        assert_eq!(rx.try_recv(), Ok((7, shared::PamOutcome::Success)));
        assert!(rx.try_recv().is_err(), "the drop guard must not also send its fallback once the real outcome already went out");
    }

    #[tokio::test]
    async fn a_panicking_task_still_reports_a_fallback_outcome_via_the_drop_guard() {
        // The scenario run_lock_authentication's doc comment describes as real: an FFI panic (or
        // any other panic) unwinding out of authenticate_current_user before it reaches its own
        // send. tokio::spawn catches the unwind (surfacing it as an Err from the JoinHandle, which
        // this test doesn't need), but the task's own locals -- including the ReportOnDrop guard
        // -- still drop normally during that unwind, which is the property this test pins.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let _guard = ReportOnDrop { acquisition: 7, outcome_tx: Some(tx) };
            panic!("simulated panic before the task's own explicit send");
        });
        let join_result = handle.await;
        assert!(join_result.is_err(), "the task did panic -- that part of the simulation, not the fix, is what's asserted here");

        let (_acquisition, outcome) = rx.try_recv().expect("the drop guard must still report a fallback outcome even though the task panicked first");
        assert!(matches!(outcome, shared::PamOutcome::PamError(_)));
    }
}
