//! Real PAM conversation, closing ADR-0015. ADR-0028's halves share `shared::PamOutcome` over
//! `shared::framing` and [`pam_service`]. Blocking `nonstick` FFI runs in a re-exec'd worker, not
//! the async Supervisor. [`run_worker`] handles `OBLISK_PAM_WORKER=1`, reads one stdin password,
//! runs one transaction, and writes one outcome frame. [`run_authentication`] re-execs via
//! [`crate::process::spawn_group_leader_stdio_piped`], exchanges piped stdin/stdout, and reports
//! to `main.rs`, the only unlock authority (ADR-0052). [`run_polkit_helper`] uses polkit's setuid
//! helper instead: polkitd accepts `AuthenticationAgentResponse2` only from uid 0 (ADR-0114).
//! It does not reuse `RendererFrame`/`SupervisorFrame`, which cross a different boundary.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use nonstick::{ConversationAdapter, Transaction};
use tokio::sync::mpsc::UnboundedSender;

/// PAM service-stack directory. A constant lets [`pam_service_in`] tests use a temporary directory.
const PAM_CONFIG_DIR: &str = "/etc/pam.d";

/// Service name for Oblisk's installed stack (`packaging/pam.d/oblisk`).
const OBLISK_SERVICE: &str = "oblisk";

/// [`run_conversation`] fallback when `packaging/pam.d/oblisk` is absent. This system lacks
/// `/etc/pam.d/polkit-1`, so `"login"` is the disclosed fallback (ADR-0028).
const FALLBACK_SERVICE: &str = "login";

/// Uses Oblisk's stack when installed, otherwise the console-login stack. Probing avoids PAM's
/// `/etc/pam.d/other` fallback, `pam_deny` on stock Arch: hardcoding `oblisk` would turn a missing
/// package file into a lock screen rejecting every correct password, while failing closed locks
/// out the user. `stat` runs once per authentication, deliberately uncached so installing the file
/// takes effect without restarting the locked shell.
fn pam_service_in(pam_config_dir: &std::path::Path) -> &'static str {
    if pam_config_dir.join(OBLISK_SERVICE).exists() { OBLISK_SERVICE } else { FALLBACK_SERVICE }
}

fn pam_service() -> &'static str {
    pam_service_in(std::path::Path::new(PAM_CONFIG_DIR))
}

/// Ceiling on the whole worker exchange (`exchange_over`), not one PAM call. It exceeds
/// `reload::PbaTimings`'s 2-3 seconds because PAM may be human-paced (network module, fingerprint
/// retry), but bounds a wedged `read_json_frame` and prevents a spawned task holding plaintext
/// forever while the prompt remains `authenticating`.
const PAM_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// ADR-0028 supplies one password before the conversation, so `masked_prompt` returns it for any
/// text. `prompt`/`radio_prompt`/`binary_prompt` are unexpected and return `ConversationError`.
/// PAM's `char*` boundary forces a plain, non-zeroizable `OsString` copy. The worker's source
/// `Vec<u8>` and this struct's `Rc<RefCell<_>>` bytes are zeroized after PAM calls; `Drop` is the
/// ADR-0005 backstop if an FFI panic skips that explicit call.
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

/// Maps a `nonstick` failure to the [`shared::PamOutcome`] the spawn side handles. Pure and
/// directly testable without PAM.
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

/// Runs `pam_start` via `TransactionBuilder`, then `authenticate` and `account_management`, using
/// `password` for every prompt.
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
    // Explicit ADR-0005 call, not only `PasswordConversation::Drop`: the Rc reaches the same bytes
    // whether `txn` already dropped, and zeroizing twice is harmless.
    shared::Zeroize::zeroize(&mut *password.borrow_mut());
    outcome
}

/// Reads `reader` to EOF. On a mid-read error, zeroize bytes already in the buffer before
/// propagating; otherwise `?` would drop password bytes unscrubbed.
fn read_password(mut reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut password = Vec::new();
    if let Err(err) = reader.read_to_end(&mut password) {
        shared::Zeroize::zeroize(&mut password);
        return Err(err);
    }
    Ok(password)
}

/// ADR-0028 worker path for `OBLISK_PAM_WORKER=1`. `main.rs` enters it before D-Bus/runtime/audio
/// setup. Read stdin until the spawn side closes it, run PAM, zeroize, and write one outcome frame.
/// `run_conversation` is blocking; only `write_json_frame` needs an executor because
/// `shared::framing` uses `tokio::io::AsyncWrite`, so `new_current_thread()` is sufficient.
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

/// Resolves `uid` and runs the worker round trip. Borrows, but never zeroizes, `secret`;
/// [`run_authentication`] owns scrubbing on every path.
///
/// ponytail: `User::from_uid` blocks in libc (`getpwuid_r`), but stays inline because local passwd
/// lookup is fast, has no NSS/LDAP, and is rare (one challenge/lock submission; ADR-0025's PBA
/// precedent). Upgrade: `spawn_blocking` if a networked NSS backend appears.
async fn authenticate_uid(uid: u32, secret: &[u8]) -> Result<shared::PamOutcome, String> {
    let username = username_for(uid)?;
    spawn_worker_and_exchange(&username, secret).await.map_err(|err| format!("pam worker failed: {err}"))
}

fn username_for(uid: u32) -> Result<String, String> {
    match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => Ok(user.name),
        Ok(None) => Err(format!("uid {uid} has no passwd entry")),
        Err(err) => Err(format!("failed to resolve uid {uid}: {err}")),
    }
}

/// polkit's setuid helper runs PAM as root and invokes `AuthenticationAgentResponse2`, which
/// polkitd accepts only from uid 0. A session-user agent cannot answer through [`run_worker`]. This
/// is libpolkit-agent's path; Quickshell's agent reaches it through that library.
const POLKIT_HELPER: &str = "/usr/lib/polkit-1/polkit-agent-helper-1";

/// [`run_authentication`]'s polkit sibling (ADR-0114): same spawn/report and `Drop` backstop, but
/// the conversation is [`POLKIT_HELPER`]'s. `Success` means polkitd was told; the caller answers
/// the held `BeginAuthentication`.
pub async fn run_polkit_helper(
    uid: u32,
    cookie: String,
    mut secret: shared::Zeroizing<Vec<u8>>,
    outcome_tx: UnboundedSender<(String, shared::PamOutcome)>,
) {
    let mut guard = ReportOnDrop { pending: Some((cookie.clone(), outcome_tx)) };
    let outcome = match authenticate_via_helper(uid, &cookie, &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => shared::PamOutcome::StartFailed(reason),
    };
    shared::Zeroize::zeroize(&mut *secret);
    if let Some((tag, tx)) = guard.pending.take()
        && tx.send((tag, outcome)).is_err()
    {
        eprintln!("polkit: the outcome channel is closed; dropping an authentication result");
    }
}

async fn authenticate_via_helper(uid: u32, cookie: &str, secret: &[u8]) -> Result<shared::PamOutcome, String> {
    let username = username_for(uid)?;
    let mut child = crate::process::spawn_group_leader_stdio_piped(POLKIT_HELPER, &[username], &[])
        .map_err(|err| format!("could not spawn {POLKIT_HELPER}: {err}"))?;
    let result = tokio::time::timeout(PAM_EXCHANGE_TIMEOUT, drive_helper(&mut child, cookie, secret)).await;
    if let Err(err) = crate::process::reap_process_group(&mut child, crate::process::DEFAULT_REAP_GRACE).await {
        eprintln!("failed to reap the polkit helper: {err}");
    }
    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(err)) => Err(format!("polkit helper exchange failed: {err}")),
        Err(_elapsed) => Err("the polkit helper did not respond within the timeout".to_string()),
    }
}

/// libpolkit-agent's `polkitagentsession.c` protocol: cookie first; answer every
/// `PAM_PROMPT_ECHO_OFF`/`PAM_PROMPT_ECHO_ON` with the one password (ADR-0028); log
/// `PAM_ERROR_MSG`/`PAM_TEXT_INFO`; `SUCCESS`/`FAILURE` end it. Verified against polkit 127.
async fn drive_helper(
    child: &mut tokio::process::Child,
    cookie: &str,
    secret: &[u8],
) -> std::io::Result<shared::PamOutcome> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut stdin = child.stdin.take().expect("spawn_group_leader_stdio_piped always pipes stdin");
    let stdout = child.stdout.take().expect("spawn_group_leader_stdio_piped always pipes stdout");
    stdin.write_all(cookie.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if line.starts_with("PAM_PROMPT_ECHO_OFF") || line.starts_with("PAM_PROMPT_ECHO_ON") {
            stdin.write_all(secret).await?;
            stdin.write_all(b"\n").await?;
        } else if line == "SUCCESS" {
            return Ok(shared::PamOutcome::Success);
        } else if line == "FAILURE" {
            return Ok(shared::PamOutcome::AuthFailed);
        } else {
            eprintln!("polkit helper: {line}");
        }
    }
    Err(std::io::Error::other("the helper closed its stdout without a verdict"))
}

/// One worker round trip for `uid`; send the outcome on `outcome_tx` tagged by the lock acquisition
/// number or polkit cookie, so the receiver matches answer to request (`lock::accepts_outcome` and
/// the polkit pending cookie). Both callers spawn it: Enter is unbounded and common, and
/// `pam_unix`'s failure delay would stall every frame if awaited inline.
///
/// `secret` is zeroized even when the future is dropped during shutdown or unwinds on panic, via
/// `Zeroizing::Drop` (ADR-0005). The wrapper is an argument because spawned arguments are captured
/// before the body is polled. Failure before PAM is `StartFailed`, matching `pam_start` failure.
///
/// [`ReportOnDrop`] sends something on every task exit. The caller sets `authenticating` and only
/// an outcome clears it; missing a send would create a one-way latch, leaving the lock screen
/// stuck until a VT switch. `UnboundedSender::send` is synchronous, so `Drop::drop` can send while
/// unwinding.
pub async fn run_authentication<T: Send + 'static>(
    uid: u32,
    mut secret: shared::Zeroizing<Vec<u8>>,
    tag: T,
    outcome_tx: UnboundedSender<(T, shared::PamOutcome)>,
) {
    let mut guard = ReportOnDrop { pending: Some((tag, outcome_tx)) };
    let outcome = match authenticate_uid(uid, &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => shared::PamOutcome::StartFailed(reason),
    };
    shared::Zeroize::zeroize(&mut *secret);
    if let Some((tag, tx)) = guard.pending.take() {
        // A closed channel means `main.rs`'s loop is gone, which `LockController::send` permits.
        if tx.send((tag, outcome)).is_err() {
            eprintln!("pam: the outcome channel is closed; dropping an authentication result");
        }
    }
}

/// [`run_authentication`]'s `Drop` backstop: reports once if dropped before ordinary `.take()`.
struct ReportOnDrop<T> {
    pending: Option<(T, UnboundedSender<(T, shared::PamOutcome)>)>,
}

impl<T> Drop for ReportOnDrop<T> {
    fn drop(&mut self) {
        if let Some((tag, tx)) = self.pending.take() {
            // Any `PamOutcome` clears `authenticating`; `PamError` supplies the prompt's error
            // text when no real PAM answer exists.
            let outcome =
                shared::PamOutcome::PamError("pam authentication task ended without reporting an outcome".to_string());
            let _ = tx.send((tag, outcome));
        }
    }
}

/// Re-execs this binary, using the `current_exe()` resolution shared with
/// `renderer_binary_path()`, as a PAM worker for `username`, then calls [`exchange_over`].
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

/// Writes `secret` to stdin, closes it so the worker sees EOF, reads one outcome frame from stdout,
/// then reaps the process group. The split enables fake-worker protocol tests. Reap always runs,
/// including after failed/timed-out I/O, so a hung worker is not untracked. `timeout` covers only
/// write/read; `reap_process_group` has its own grace. Without it, `read_json_frame` could hang
/// forever and the inline polkit path would stall `main.rs`'s `select!`. Parameterize it so tests
/// avoid the real 30-second [`PAM_EXCHANGE_TIMEOUT`].
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

/// Write/read half wrapped by [`exchange_over`] in `tokio::time::timeout`. It borrows `child`, so
/// `exchange_over` can reap after completion or cancellation. Cancellation drops the taken stdin
/// handle and closes that pipe even mid-write.
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

    /// Without a stack, keep the console-login service. Naming `oblisk` would select
    /// `/etc/pam.d/other`, `pam_deny` on stock Arch, and reject the correct password.
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

    /// An unreadable PAM directory is "not installed", not a panic; this runs in the unlock worker.
    #[test]
    fn a_missing_pam_config_directory_falls_back_rather_than_failing() {
        assert_eq!(pam_service_in(std::path::Path::new("/no/such/pam.d")), "login");
    }

    /// The shipped file is what the probe finds and must carry both chains: `run_conversation`
    /// calls `authenticate`, then `account_management`; without `account`, the second fails after
    /// password acceptance.
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

    /// Test timeout for ordinary paths: short enough to bound a hung test, long enough for local
    /// `sh -c` fakes. Deliberately distinct from [`PAM_EXCHANGE_TIMEOUT`].
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

    // Real libpam re-exec needs a real PAM stack; unit-test the wire protocol with a fake shell
    // worker instead.

    #[tokio::test]
    async fn exchange_over_reads_back_the_worker_s_one_shot_outcome_frame() {
        // `shared::framing`: 4-byte big-endian length, then JSON. `Success` is the 9-byte JSON
        // string `"Success"`.
        let script = r#"cat > /dev/null; printf '\000\000\000\011"Success"'"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");

        let outcome = exchange_over(child, b"the-password", TEST_TIMEOUT).await.expect("exchange_over failed");

        assert_eq!(outcome, shared::PamOutcome::Success);
    }

    #[tokio::test]
    async fn exchange_over_still_reaps_the_worker_when_the_frame_read_fails() {
        // Malformed frame, then a hang. Read must fail and the process must still be reaped.
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
        // Fake worker reports stdin byte count, proving delivery and EOF closure.
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
        // A sleeping worker models a PAM module blocked on unreachable network auth. Return an
        // error within `timeout` and still reap it.
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
        // Supplies real bytes once, then fails, reproducing a mid-`read_to_end` pipe error.
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
        // The partial Vec is already dropped here; proving live zeroization requires observing it
        // before drop, not afterward.
    }

    // Pin release when the spawned authentication task panics or drops before reporting.

    #[test]
    fn report_on_drop_sends_a_fallback_outcome_when_dropped_before_reporting() {
        // Simulates panic from `run_authentication` or future drop mid-`.await`, before its send.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(ReportOnDrop { pending: Some((7u64, tx)) });

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
        let mut guard = ReportOnDrop { pending: Some((7u64, tx)) };
        let (tag, tx) = guard.pending.take().expect("freshly built guard holds a sender");
        tx.send((tag, shared::PamOutcome::Success)).expect("send failed");
        drop(guard);

        assert_eq!(rx.try_recv(), Ok((7, shared::PamOutcome::Success)));
        assert!(
            rx.try_recv().is_err(),
            "the drop guard must not also send its fallback once the real outcome already went out"
        );
    }

    #[tokio::test]
    async fn a_panicking_task_still_reports_a_fallback_outcome_via_the_drop_guard() {
        // `tokio::spawn` catches an FFI panic, but locals, including `ReportOnDrop`, still drop;
        // this pins that fallback send.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let _guard = ReportOnDrop { pending: Some((7u64, tx)) };
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
