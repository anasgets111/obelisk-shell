//! Real PAM conversation, closing ADR-0015. Both halves of ADR-0028's design live here, sharing
//! the wire protocol (`shared::PamOutcome` over `shared::framing`) and the PAM service name
//! ([`pam_service`]). PAM runs in a re-exec'd worker, not inline, because `nonstick`'s FFI blocks
//! and this codebase forbids a blocking call inline in the async Supervisor (ADR-0028).
//! [`run_worker`] is the worker side: runs when re-exec'd with `OBLISK_PAM_WORKER=1` (`main.rs`'s
//! branch, ahead of D-Bus/tokio-runtime/audio-thread setup), drives one blocking `nonstick`
//! transaction against its stdin password, and writes one [`shared::PamOutcome`] frame to stdout.
//! [`drive_pam_and_respond`] is the spawn side: resolves a polkit challenge's uid, re-execs this
//! binary as a worker ([`crate::process::spawn_group_leader_stdio_piped`]), exchanges the
//! password/outcome over piped stdin/stdout, and reports to polkitd via
//! `AuthenticationAgentResponse2`. [`authenticate_current_user`] is its session-lock sibling
//! (ADR-0052): no polkit, the uid is this process's own, outcome returned not reported.
//! No reuse of `RendererFrame`/`SupervisorFrame`: a different boundary than Supervisor<->Renderer.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use nonstick::{ConversationAdapter, Transaction};
use tokio::sync::mpsc::UnboundedSender;

/// Where PAM keeps its per-service stacks. A constant so [`pam_service_in`] can be pointed at a
/// temporary directory by its tests without a real `/etc` to install into.
const PAM_CONFIG_DIR: &str = "/etc/pam.d";

/// The service Oblisk's own stack is installed as (`packaging/pam.d/oblisk`).
const OBLISK_SERVICE: &str = "oblisk";

/// What [`run_conversation`] authenticates against when `packaging/pam.d/oblisk` isn't installed.
/// This system has no `/etc/pam.d/polkit-1`, so `"login"` is the disclosed fallback (ADR-0028).
const FALLBACK_SERVICE: &str = "login";

/// The PAM service this worker authenticates against: Oblisk's own stack when the admin has
/// installed one, the console-login stack otherwise. Chosen by probing, not hardcoding: a missing
/// service file makes PAM fall through to `/etc/pam.d/other`, which is `pam_deny` on a stock Arch
/// install, so naming `oblisk` unconditionally would turn "the packager forgot one file" into "the
/// lock screen refuses every correct password", and failing closed locks the user out of their own
/// machine. The probe is a `stat` per authentication (once per typed password), deliberately
/// uncached: an admin installing the file shouldn't have to restart the shell, least convenient
/// exactly when the session is locked.
fn pam_service_in(pam_config_dir: &std::path::Path) -> &'static str {
    if pam_config_dir.join(OBLISK_SERVICE).exists() { OBLISK_SERVICE } else { FALLBACK_SERVICE }
}

fn pam_service() -> &'static str {
    pam_service_in(std::path::Path::new(PAM_CONFIG_DIR))
}

/// Ceiling on the whole write-password/read-outcome exchange with the worker (`exchange_over`),
/// not any single PAM call inside it. Generous relative to `reload::PbaTimings`'s 2-3 second
/// deadlines, since PAM is human-paced (a network-backed module, a fingerprint retry loop) but
/// still bounded: without it a wedged worker never returns from `read_json_frame`, and since
/// `drive_pam_and_respond` is awaited inside `main.rs`'s top-level `select!`, that hangs the whole
/// Supervisor. [`authenticate_current_user`] runs on a spawned task instead but wants the same
/// ceiling, so an unbounded exchange there doesn't leave a stray task and plaintext password.
const PAM_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oblisk's flow has only one pre-supplied password known before the conversation starts
/// (ADR-0028), so [`ConversationAdapter::masked_prompt`] always answers with it regardless of
/// prompt text; `prompt`/`radio_prompt`/`binary_prompt` are never expected, so `ConversationError`
/// is correct if PAM asks one anyway. Its `OsString` is a plain, non-zeroizable copy of the
/// password, forced by PAM's C API boundary (`char*`). Everything else is zeroized: the source
/// `Vec<u8>` in [`run_worker`] after the conversation completes, and this struct's own `password`,
/// explicitly by `run_conversation` via the shared `Rc<RefCell<_>>` handle right after the PAM
/// calls finish. Since `masked_prompt` only takes `&self`, it could only be zeroized from `Drop`
/// (a backup per ADR-0005): the `Drop` impl below covers the path where `run_conversation` never
/// reaches its own call (an FFI panic).
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
/// should react to. Pure, so it's directly unit-testable without any real PAM call.
fn outcome_for_error(err: nonstick::ErrorCode) -> shared::PamOutcome {
    use nonstick::ErrorCode::*;
    match err {
        MaxTries => shared::PamOutcome::MaxTries,
        AuthenticationError | PermissionDenied | UserUnknown | CredentialsInsufficient | CredentialsExpired => {
            shared::PamOutcome::AuthFailed
        }
        other => shared::PamOutcome::PamError(format!("{other:?}")),
    }
}

/// Drives one whole PAM transaction (`pam_start` via `TransactionBuilder`, then `authenticate` and
/// `account_management`) against `username`, answering every prompt with `password`.
fn run_conversation(username: &str, password: &[u8]) -> shared::PamOutcome {
    let password = Rc::new(RefCell::new(password.to_vec()));
    let conversation = PasswordConversation { password: Rc::clone(&password) };
    let outcome = match nonstick::TransactionBuilder::new_with_service(pam_service())
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
    // Explicit call, not left to PasswordConversation's own Drop alone (ADR-0005): the Rc clone
    // reaches the same backing bytes regardless of whether txn has already dropped, and a zeroize
    // on an already-zeroized buffer is a harmless no-op.
    shared::Zeroize::zeroize(&mut *password.borrow_mut());
    outcome
}

/// Reads `reader` (stdin, locked) to exhaustion. `read_to_end` can leave real password bytes in
/// the buffer on a mid-read I/O error, so `?`-ing straight out would drop them unscrubbed into
/// freed heap memory: zeroize whatever was read before propagating the error.
fn read_password(mut reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut password = Vec::new();
    if let Err(err) = reader.read_to_end(&mut password) {
        shared::Zeroize::zeroize(&mut password);
        return Err(err);
    }
    Ok(password)
}

/// The worker side of ADR-0028: runs when this binary is re-exec'd with `OBLISK_PAM_WORKER=1` set
/// (`main.rs`'s branch, ahead of D-Bus/tokio-runtime/audio-thread setup, must never touch any of
/// that). Reads the password off stdin (the spawn side closes the write half so this hits EOF),
/// drives the PAM conversation, zeroizes the password, and writes one [`shared::PamOutcome`] frame
/// to stdout. [`run_conversation`] is entirely synchronous/blocking, since `nonstick`'s FFI calls
/// block anyway; only the final `write_json_frame` needs an async executor (`shared::framing` is
/// built on `tokio::io::AsyncWrite`), so `new_current_thread()` is the minimal correct runtime.
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

/// Logs why `drive_pam_and_respond` is giving up on `cookie` and zeroizes `secret`, factored out
/// so each early-return failure branch needn't repeat the same `eprintln!`-then-`zeroize` pair.
fn deny(secret: &mut Vec<u8>, cookie: &str, reason: impl std::fmt::Display) {
    eprintln!("polkit authentication for cookie {cookie:?} failed: {reason}");
    shared::Zeroize::zeroize(secret);
}

/// The spawn side of ADR-0028: resolves `challenge`'s uid to a username, re-execs this binary as a
/// PAM worker, exchanges `secret` for a [`shared::PamOutcome`], and reports success back to
/// polkitd via `AuthenticationAgentResponse2`. `main.rs`'s `RendererFrame::SecureSubmit` arm for
/// `("polkit", "authenticate")` is this function's only caller; `secret` is zeroized on every
/// return path (ADR-0005 forbids dropping it without a `.zeroize()` call first).
pub async fn drive_pam_and_respond(
    authority: &zbus_polkit::policykit1::AuthorityProxy<'_>,
    challenge: crate::polkit::BeginAuthenticationCall,
    mut secret: Vec<u8>,
) {
    let Some(uid) = crate::polkit::first_unix_user_uid(&challenge.identities) else {
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
            let identity =
                zbus_polkit::policykit1::Identity { identity_kind: "unix-user", identity_details: &identity_details };
            if let Err(err) = authority.authentication_agent_response2(uid, &challenge.cookie, &identity).await {
                eprintln!("authentication_agent_response2 failed for cookie {:?}: {err}", challenge.cookie);
            }
        }
        other => {
            // AuthenticationAgentResponse2 is documented as "invoke on successful
            // authentication": there is no "report failure" D-Bus call. Letting polkitd's own
            // challenge timeout apply is correct, not a missing case.
            eprintln!("polkit authentication for cookie {:?} did not succeed: {other:?}", challenge.cookie);
        }
    }
}

/// The half [`drive_pam_and_respond`] and [`authenticate_current_user`] share: resolve `uid` to a
/// username and run the whole worker round trip. `Err` carries why the conversation never
/// happened, reported per caller's own protocol (polkitd gets silence and a log line,
/// `oblisk.lock` gets a lock-screen string). Borrows `secret` and never zeroizes it, since both
/// callers already scrub it on every return path (ADR-0005) and scrubbing here too would make
/// ownership harder to audit.
///
/// ponytail: `User::from_uid` is a blocking libc call (`getpwuid_r`), inline here rather than via
/// `spawn_blocking`, since the lookup is local-passwd-file-fast (no NSS/LDAP) and rare (one
/// challenge or lock submission at a time; ADR-0025's precedent for PBA swaps). Upgrade:
/// `spawn_blocking` if a networked NSS backend appears.
async fn authenticate_uid(uid: u32, secret: &[u8]) -> Result<shared::PamOutcome, String> {
    let username = match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => user.name,
        Ok(None) => return Err(format!("uid {uid} has no passwd entry")),
        Err(err) => return Err(format!("failed to resolve uid {uid}: {err}")),
    };
    spawn_worker_and_exchange(&username, secret).await.map_err(|err| format!("pam worker failed: {err}"))
}

/// [`drive_pam_and_respond`]'s sibling for the session lock (ADR-0042, ADR-0052): the same worker
/// exchange, but with no polkit challenge to read a uid from and no polkitd to report to. The user
/// is this process's own owner, since the Supervisor runs as the session user. Returns the outcome
/// instead of acting on it: `main.rs`'s `secure_submit(lock, authenticate)` arm is the only caller,
/// and `tokio::spawn`s this rather than awaiting inline (an Enter key at a lock screen is neither
/// bounded nor rare), so the outcome comes back over a channel to the `pam_outcomes` arm, the only
/// place allowed to turn a `Success` into an unlock order (ADR-0042). `secret` is zeroized on
/// every return path, and, unlike [`drive_pam_and_respond`], on a never-returns path too: spawned
/// rather than awaited, this future can be dropped mid-`.await` by a runtime shutdown or unwound
/// by a panic, neither of which reaches the explicit `zeroize` below (ADR-0005 requires
/// `.zeroize()` before every drop, not just the happy one). `zeroize::Zeroizing` backs that with a
/// `Drop` fallback, the posture `shared::SecureBuffer` takes for `secure_submit`'s writer;
/// `SecureBuffer` doesn't fit here since its only load path is `push_str(&str)` and this secret is
/// a raw `Vec<u8>`. The wrapper is a parameter rather than applied inside the body, since a
/// `tokio::spawn`ed future's arguments are captured when built but the body only runs once polled:
/// wrapping inside it could let a shutdown drop a bare `Vec<u8>` before that ever happens. A
/// failure to even reach PAM becomes `StartFailed`, the same variant a `pam_start` failure uses,
/// so the lock screen has one `error` string either way.
pub async fn authenticate_current_user(mut secret: shared::Zeroizing<Vec<u8>>) -> shared::PamOutcome {
    let outcome = match authenticate_uid(nix::unistd::Uid::current().as_raw(), &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => shared::PamOutcome::StartFailed(reason),
    };
    shared::Zeroize::zeroize(&mut *secret);
    outcome
}

/// Wraps [`authenticate_current_user`] so `outcome_tx` always receives something regardless of how
/// the `tokio::spawn`ed task ends: ordinary return, a panic, or the future dropped mid-`.await` by
/// a runtime shutdown. Without this, `LockController::try_begin_authentication` sets
/// `authenticating` (ADR-0052), and only a `LockEvent::Authenticated` in `main.rs`'s
/// `pam_outcomes` arm ever clears it; a task that never reaches its own `.send()` leaves that a
/// one-way latch, stranding the session behind the lock with no way back short of a VT switch.
/// [`ReportOnDrop`] is the mechanism: `UnboundedSender::send` is synchronous, so it still runs
/// from `Drop::drop` during an unwind, unlike an awaited cleanup step a panicking task would never
/// reach. A `Drop` guard keeps the failure path local to the one task that can fail, instead of
/// growing `main.rs`'s `select!` a second arm to translate a `JoinHandle`'s `JoinError`.
/// `acquisition` is the tag `LockController::try_begin_authentication` handed out when admitted,
/// carried back untouched: an answer outlives the lock it answers for (`pam_unix`'s ~1 second,
/// [`PAM_EXCHANGE_TIMEOUT`]'s 30), and only `lock::accepts_outcome` knows whether that lock is
/// still the one on the glass; re-reading the controller's current acquisition would find a
/// number that already moved.
pub async fn run_lock_authentication(
    secret: shared::Zeroizing<Vec<u8>>,
    acquisition: u64,
    outcome_tx: UnboundedSender<(u64, shared::PamOutcome)>,
) {
    let mut guard = ReportOnDrop { acquisition, outcome_tx: Some(outcome_tx) };
    let outcome = authenticate_current_user(secret).await;
    if let Some(tx) = guard.outcome_tx.take() {
        // A closed channel means main.rs's loop is gone, the posture LockController::send takes.
        if tx.send((acquisition, outcome)).is_err() {
            eprintln!("lock: the pam outcome channel is closed; dropping an authentication result");
        }
    }
}

/// [`run_lock_authentication`]'s `Drop` backstop: reports a fallback outcome the one time it's
/// dropped still holding its sender (every exit but the ordinary one, which `.take()`s it first).
/// See that function's doc comment for why this exists over an awaited `JoinHandle`.
struct ReportOnDrop {
    /// Bound to the same lock the real answer would have been, the case `lock::accepts_outcome`
    /// exists to refuse a fallback tagged with the wrong one.
    acquisition: u64,
    outcome_tx: Option<UnboundedSender<(u64, shared::PamOutcome)>>,
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.outcome_tx.take() {
            // main.rs's pam_outcomes arm only needs a PamOutcome to run LockEvent::Authenticated
            // and clear authenticating; the variant doesn't matter since there's no real PAM
            // answer, and PamError gives the lock screen's error string something to say.
            let outcome =
                shared::PamOutcome::PamError("pam authentication task ended without reporting an outcome".to_string());
            let _ = tx.send((self.acquisition, outcome));
        }
    }
}

/// Re-execs this binary (the same `current_exe()` resolution `renderer_binary_path()` uses for the
/// renderer) as a PAM worker for `username`, then delegates the wire exchange to [`exchange_over`].
async fn spawn_worker_and_exchange(username: &str, secret: &[u8]) -> std::io::Result<shared::PamOutcome> {
    let exe = std::env::current_exe()?;
    let child = crate::process::spawn_group_leader_stdio_piped(
        &exe.to_string_lossy(),
        &[],
        &[
            ("OBLISK_PAM_WORKER".to_string(), "1".to_string()),
            ("OBLISK_PAM_USERNAME".to_string(), username.to_string()),
        ],
    )?;
    exchange_over(child, secret, PAM_EXCHANGE_TIMEOUT).await
}

/// Writes `secret` to `child`'s stdin (closing the write half so the worker's read hits EOF),
/// reads one [`shared::PamOutcome`] frame back over its stdout, then reaps the process group.
/// Split out from [`spawn_worker_and_exchange`] so the wire protocol can be tested against a fake
/// child without a real re-exec'd PAM worker; the reap always runs, even on a failed or timed-out
/// write or read, so a hung worker is never left running untracked. `timeout` bounds only the
/// write+read exchange, not the reap that follows (`reap_process_group` has its own grace period):
/// without it, a worker that never writes or exits hangs `read_json_frame` forever, and on the
/// polkit path, awaited inline in `main.rs`'s top-level `select!`, that stalls the entire
/// Supervisor. Taken as a parameter, matching `reap_process_group`'s `grace`, so tests can use a
/// short one instead of [`PAM_EXCHANGE_TIMEOUT`]'s real 30 seconds.
async fn exchange_over(
    mut child: tokio::process::Child,
    secret: &[u8],
    timeout: Duration,
) -> std::io::Result<shared::PamOutcome> {
    let outcome_result = match tokio::time::timeout(timeout, write_secret_then_read_outcome(&mut child, secret)).await {
        Ok(result) => result,
        Err(_elapsed) => {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "pam worker did not respond within the timeout"))
        }
    };

    if let Err(err) = crate::process::reap_process_group(&mut child, crate::process::DEFAULT_REAP_GRACE).await {
        eprintln!("failed to reap pam worker: {err}");
    }

    outcome_result
}

/// The actual write-then-read half of the exchange, wrapped by [`exchange_over`] in a
/// `tokio::time::timeout`. Split out so the timeout wraps a plain future borrowing `child`, which
/// `exchange_over` can still reach afterward to reap it whether this future completed or was
/// cancelled: a cancelled future drops everything it owns (child.stdin's taken handle), closing
/// that pipe end cleanly even mid-write.
async fn write_secret_then_read_outcome(
    child: &mut tokio::process::Child,
    secret: &[u8],
) -> std::io::Result<shared::PamOutcome> {
    let mut stdin = child.stdin.take().expect("spawn_group_leader_stdio_piped always pipes stdin");
    tokio::io::AsyncWriteExt::write_all(&mut stdin, secret).await?;
    drop(stdin); // closes the write half so the worker's stdin read hits EOF

    let mut stdout = child.stdout.take().expect("spawn_group_leader_stdio_piped always pipes stdout");
    shared::framing::read_json_frame(&mut stdout).await.map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pam_service_in ----

    /// The safe direction, and the one that matters: with no stack installed the worker keeps
    /// using the console-login service that has been authenticating all along. Naming `oblisk`
    /// here instead would send PAM to `/etc/pam.d/other`, which is `pam_deny` on a stock Arch
    /// install -- a lock screen that refuses the correct password.
    #[test]
    fn without_an_installed_stack_the_service_falls_back_to_login() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(pam_service_in(dir.path()), "login");
    }

    #[test]
    fn an_installed_stack_is_preferred_over_the_console_login_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("oblisk"), "auth include system-auth\n").unwrap();
        assert_eq!(pam_service_in(dir.path()), "oblisk");
    }

    /// A directory PAM could not read at all is the same case as "not installed", not a panic:
    /// this runs inside the worker on the path to an unlock.
    #[test]
    fn a_missing_pam_config_directory_falls_back_rather_than_failing() {
        assert_eq!(pam_service_in(std::path::Path::new("/no/such/pam.d")), "login");
    }

    /// The file this repo ships is what the probe looks for, and it has to carry both chains --
    /// `run_conversation` calls `authenticate` and then `account_management`, and a stack with no
    /// `account` line fails the second one after the password was already accepted.
    #[test]
    fn the_shipped_pam_stack_declares_both_chains_the_worker_drives() {
        let shipped =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../packaging/pam.d/oblisk")).unwrap();
        let directives: Vec<&str> =
            shipped.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).collect();
        assert!(directives.iter().any(|line| line.starts_with("auth")), "no auth chain in {directives:?}");
        assert!(directives.iter().any(|line| line.starts_with("account")), "no account chain in {directives:?}");
        assert!(
            directives.iter().all(|line| line.starts_with("auth") || line.starts_with("account")),
            "the worker opens no session and changes no password, so anything else is dead config: {directives:?}"
        );
    }

    /// A generous timeout for tests exercising the ordinary (non-timeout) paths -- short enough
    /// to keep a hung test from stalling the suite, long enough it never fires against these
    /// fast, local `sh -c` fake workers. Distinct from [`PAM_EXCHANGE_TIMEOUT`] on purpose.
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

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
        assert_eq!(
            outcome_for_error(nonstick::ErrorCode::SystemError),
            shared::PamOutcome::PamError("SystemError".to_string())
        );
    }

    // drive_pam_and_respond / spawn_worker_and_exchange call real libpam via a real subprocess
    // re-exec, which can't be meaningfully unit-tested without a real PAM stack. What can and
    // must be tested is the wire protocol itself, exercised against a fake "worker" (a shell
    // one-liner) rather than a real re-exec'd PAM worker.

    #[tokio::test]
    async fn exchange_over_reads_back_the_worker_s_one_shot_outcome_frame() {
        // Frame wire format (shared::framing): a 4-byte big-endian length prefix, then the JSON
        // payload. PamOutcome::Success serializes as the 9-byte JSON string "Success".
        let script = r#"cat > /dev/null; printf '\000\000\000\011"Success"'"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");

        let outcome = exchange_over(child, b"the-password", TEST_TIMEOUT).await.expect("exchange_over failed");

        assert_eq!(outcome, shared::PamOutcome::Success);
    }

    #[tokio::test]
    async fn exchange_over_still_reaps_the_worker_when_the_frame_read_fails() {
        // A "worker" that writes a malformed (undecodable) frame and then hangs instead of
        // exiting. The read must fail, but the process must still be reaped, not left running.
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
        assert!(
            gone.is_ok(),
            "the worker (pid {pid}) should be reaped even though exchange_over returned an error, not left sleeping"
        );
    }

    #[tokio::test]
    async fn exchange_over_actually_delivers_the_secret_to_the_child_s_stdin() {
        // A fake worker that reports back the byte count it read on stdin, proving the write
        // half actually reaches the child (and closes, so the child's read hits EOF).
        let script = r#"n=$(wc -c < /dev/stdin); printf '\000\000\000\025{"PamError":"got %s"}' "$n""#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");

        let outcome = exchange_over(child, b"the-password", TEST_TIMEOUT).await.expect("exchange_over failed");

        assert_eq!(
            outcome,
            shared::PamOutcome::PamError("got 12".to_string()),
            "the fake worker must have seen all 12 bytes of the secret"
        );
    }

    #[tokio::test]
    async fn exchange_over_times_out_and_still_reaps_a_worker_that_never_responds() {
        // A "worker" that writes nothing and just sleeps -- a PAM module blocked on an
        // unreachable network auth backend. Must return an error within `timeout`, and the
        // worker must still end up reaped.
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
        assert!(
            gone.is_ok(),
            "the worker (pid {pid}) should be reaped even after a timeout, not left sleeping for the full 30s"
        );
    }

    #[test]
    fn read_password_zeroizes_whatever_it_already_read_before_a_mid_stream_error() {
        // A reader that hands back real bytes on its first read() call, then fails on its
        // second, reproducing a pipe error partway through a read_to_end loop.
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
        // By the time this test can observe anything, the partially-filled Vec has already been
        // dropped inside read_password -- proving a live buffer was zeroized needs inspecting it
        // before it drops, not after.
    }

    // The next three tests pin this module's own code: authenticating must be releasable even
    // when the spawned task that would otherwise release it panics or is dropped before
    // reporting.

    #[test]
    fn report_on_drop_sends_a_fallback_outcome_when_dropped_before_reporting() {
        // Simulates a panic unwinding out of authenticate_current_user, or
        // run_lock_authentication's future being dropped mid-.await: either way, the guard is
        // dropped without its own take-then-send ever running.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(ReportOnDrop { acquisition: 7, outcome_tx: Some(tx) });

        let (acquisition, outcome) =
            rx.try_recv().expect("a fallback outcome must be sent when the guard is dropped before reporting");
        assert!(
            matches!(outcome, shared::PamOutcome::PamError(_)),
            "the fallback must be a PamOutcome so main.rs's pam_outcomes arm can still clear `authenticating`"
        );
        assert_eq!(
            acquisition, 7,
            "and it must be tagged with the lock it was started for, or lock::accepts_outcome cannot place it"
        );
    }

    #[test]
    fn report_on_drop_does_not_double_send_once_the_real_outcome_already_went_out() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut guard = ReportOnDrop { acquisition: 7, outcome_tx: Some(tx) };
        guard
            .outcome_tx
            .take()
            .expect("freshly built guard holds a sender")
            .send((7, shared::PamOutcome::Success))
            .expect("send failed");
        drop(guard);

        assert_eq!(rx.try_recv(), Ok((7, shared::PamOutcome::Success)));
        assert!(
            rx.try_recv().is_err(),
            "the drop guard must not also send its fallback once the real outcome already went out"
        );
    }

    #[tokio::test]
    async fn a_panicking_task_still_reports_a_fallback_outcome_via_the_drop_guard() {
        // An FFI panic unwinding out of authenticate_current_user before it reaches its own
        // send. tokio::spawn catches the unwind, but the task's own locals -- including the
        // ReportOnDrop guard -- still drop normally during it, the property this test pins.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let _guard = ReportOnDrop { acquisition: 7, outcome_tx: Some(tx) };
            panic!("simulated panic before the task's own explicit send");
        });
        let join_result = handle.await;
        assert!(
            join_result.is_err(),
            "the task did panic -- that part of the simulation, not the fix, is what's asserted here"
        );

        let (_acquisition, outcome) = rx
            .try_recv()
            .expect("the drop guard must still report a fallback outcome even though the task panicked first");
        assert!(matches!(outcome, shared::PamOutcome::PamError(_)));
    }
}
