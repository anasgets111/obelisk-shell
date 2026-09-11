//! `obelisk log`: the shell's own stdout and stderr, kept somewhere a detached run can be read
//! from (ADR-0199).
//!
//! Every diagnostic in both binaries is an `eprintln!`, so capturing them is two `dup2` calls and
//! not a logging framework. The Renderer inherits the descriptors through
//! `process::spawn_group_leader`, and a panic reaches the file directly, because no thread of ours
//! sits between the process and the write.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

/// How often `--follow` looks for new bytes. inotify would want a runtime in a subcommand that has
/// none, and a fifth of a second is nobody's problem in a log reader.
const POLL: Duration = Duration::from_millis(200);

/// Points stdout and stderr at [`shared::log_path`] when they currently go to `/dev/null`.
///
/// That is the only case with nothing to lose. `spawn-at-startup "obelisk"` hands the shell
/// `/dev/null` and a whole session's diagnostics go there; a terminal, a file or a pipe is
/// somewhere someone chose, and taking those descriptors would make `obelisk >mine.log` write an
/// empty file.
///
/// Truncated per run rather than appended. It is per-login state beside the control socket, and
/// the run worth reading is the current one.
pub fn capture() -> io::Result<()> {
    if !stderr_is_discarded() {
        return Ok(());
    }
    let path = shared::log_path()?;
    // Not truncating on open: the lock below is what establishes that nobody else owns this file,
    // and a second shell must not blank a running one's log on its way to finding that out.
    let file = OpenOptions::new().write(true).create(true).truncate(false).open(&path)?;
    if !take_lock(&file) {
        eprintln!("obelisk: another shell owns {}, so this run's output stays where it is", path.display());
        return Ok(());
    }
    file.set_len(0)?;
    for target in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: both arguments are live descriptors. `file`'s comes from the `open` above, and
        // the target is a standard stream this process has not closed.
        if unsafe { libc::dup2(file.as_raw_fd(), target) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    // `file` drops here; the lock does not. It belongs to the open file description that the two
    // descriptors dup'd from it share, so the kernel releases it only once the last of them
    // closes. Process exit is one way that happens, which is what [`writing`] reads.
    Ok(())
}

/// `obelisk log [--follow]`, printing what [`capture`] collected.
///
/// Boxed rather than `io::Result` so the one failure a user meets prints as a sentence: `main`
/// renders its error with `Debug`, and an `io::Error` renders there as its struct.
pub fn print(follow: bool) -> Result<(), Box<dyn std::error::Error>> {
    let path = shared::log_path()?;
    let mut file = File::open(&path).map_err(|err| {
        format!("no log at {}: {err}. A shell with a terminal or a redirect writes there instead", path.display())
    })?;
    let mut out = io::stdout().lock();
    loop {
        io::copy(&mut file, &mut out)?;
        out.flush()?;
        // Read first, then decide: a shell that wrote its last words and exited still gets them
        // printed.
        if !follow || !writing(&path) {
            return Ok(());
        }
        // A restarting shell truncates the file, which leaves this reader past the new end.
        if file.stream_position()? > file.metadata()?.len() {
            file.seek(SeekFrom::Start(0))?;
        }
        std::thread::sleep(POLL);
    }
}

/// Whether this process's stderr is `/dev/null`, read off `/proc/self/fd`.
///
/// Only a confirmed `/dev/null` counts. A readlink that fails says nothing about where the
/// descriptor goes, and guessing there is how a redirect gets silently swallowed.
fn stderr_is_discarded() -> bool {
    std::fs::read_link("/proc/self/fd/2").is_ok_and(|target| target == Path::new("/dev/null"))
}

/// Whether a shell still holds the log's lock, and so may still write to it.
///
/// Taking the lock is the test, and taking it proves nobody else has it. `flock` belongs to the
/// open file description, so the kernel drops a dead writer's lock whether it exited or crashed; a
/// pid file would have to be right about both.
fn writing(path: &Path) -> bool {
    // This descriptor closes at the end of the call, releasing any lock taken here with it.
    File::open(path).is_ok_and(|file| !take_lock(&file))
}

/// `flock(LOCK_EX | LOCK_NB)`: true when the caller now holds the lock.
fn take_lock(file: &File) -> bool {
    // SAFETY: a live descriptor from `open`. `LOCK_NB` is what keeps this from blocking.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole of `--follow`'s stop condition. `flock` is per open file description rather than
    /// per process, so a second descriptor on the same file is refused here exactly as another
    /// process would be.
    #[test]
    fn a_held_lock_is_what_says_a_shell_is_still_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obelisk-shell.log");
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(&path).unwrap();

        assert!(!writing(&path), "an unlocked log has nobody writing it");
        assert!(take_lock(&file), "a free lock is takeable");
        assert!(writing(&path), "a held lock is a live writer");

        // What a crash does, since the kernel closes the descriptors either way.
        drop(file);
        assert!(!writing(&path), "the last descriptor closing ends the follow");
        assert!(!writing(&dir.path().join("absent")), "no file, no writer");
    }
}
