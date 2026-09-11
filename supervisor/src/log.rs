//! `obelisk log`: the shell's own stdout and stderr, kept somewhere a detached run can be read
//! from (ADR-0199).
//!
//! Every diagnostic in both binaries is an `eprintln!`, so this is a `dup2` per descriptor, not a
//! logging framework. The Renderer inherits them through `process::spawn_group_leader`, and a
//! panic reaches the file directly because no thread of ours sits in between.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

/// How often `--follow` looks for new bytes. inotify would want a runtime in a subcommand that has
/// none, and a fifth of a second is nobody's problem in a log reader.
const POLL: Duration = Duration::from_millis(200);

/// Points whichever of stdout and stderr go to `/dev/null` at [`shared::log_path`].
///
/// `/dev/null` is the only destination with nothing to lose; a terminal, redirect or pipe is one
/// someone chose. Per descriptor, or `obelisk >mine.log 2>/dev/null` leaves `mine.log` empty.
/// Truncated per run: per-login state beside the control socket.
pub fn capture() -> io::Result<()> {
    let discarded: Vec<i32> =
        [libc::STDOUT_FILENO, libc::STDERR_FILENO].into_iter().filter(|fd| goes_to_dev_null(*fd)).collect();
    if discarded.is_empty() {
        return Ok(());
    }
    let path = shared::log_path()?;
    // Not truncating on open: a second shell must not blank a running one's log on its way to
    // discovering the lock is taken.
    let file = OpenOptions::new().write(true).create(true).truncate(false).open(&path)?;
    if !take_lock(&file)? {
        eprintln!("obelisk: another shell owns {}, so this run's output stays where it is", path.display());
        return Ok(());
    }
    file.set_len(0)?;
    for target in discarded {
        // SAFETY: both arguments are live descriptors. `file`'s comes from the `open` above, and
        // the target is a standard stream this process has not closed.
        if unsafe { libc::dup2(file.as_raw_fd(), target) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    // `file` drops here; the lock does not. It belongs to the open file description the dup'd
    // descriptors share, and outlives them all -- Renderer copies included.
    Ok(())
}

/// `obelisk log [--follow]`, printing what [`capture`] collected.
///
/// Boxed, not `io::Result`: `main` prints errors with `Debug`, where an `io::Error` shows as its
/// struct rather than a sentence.
pub fn print(follow: bool) -> Result<(), Box<dyn std::error::Error>> {
    let path = shared::log_path()?;
    let mut file = File::open(&path).map_err(|err| {
        format!("no log at {}: {err}. A shell with a terminal or a redirect writes there instead", path.display())
    })?;
    let mut out = io::stdout().lock();
    let mut writer_left = false;
    loop {
        // Before the read, so nothing is copied from a stale offset. A restart replaces the file
        // or truncates it; the path, not the descriptor, is where the current run writes.
        match std::fs::metadata(&path) {
            Ok(latest) if latest.ino() != file.metadata()?.ino() => file = File::open(&path)?,
            // ponytail: a same-inode truncate that regrows past this offset inside one POLL is
            // invisible, costing the new run's first bytes. Upgrade path is a sequence number in
            // the filename, which costs `obelisk log` its single known path.
            Ok(latest) if latest.len() < file.stream_position()? => {
                file.seek(SeekFrom::Start(0))?;
            }
            _ => {}
        }
        io::copy(&mut file, &mut out)?;
        out.flush()?;
        if !follow || writer_left {
            return Ok(());
        }
        // Read now, acted on after one more pass above, so a last line written between the copy
        // and the exit still prints.
        writer_left = !is_locked(&file)?;
        if !writer_left {
            std::thread::sleep(POLL);
        }
    }
}

/// Whether `fd` goes to `/dev/null`, as `/proc/self/fd` spells it. Only a confirmed match counts:
/// a failed readlink says nothing, and guessing is how a redirect gets swallowed.
fn goes_to_dev_null(fd: i32) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok_and(|target| target == Path::new("/dev/null"))
}

/// A whole-file `F_OFD_*` request: the writer's claim on the log.
///
/// Not `flock`, for one property it lacks: an OFD lock can be asked about without being taken. A
/// reader probing with `flock` holds what it tests, and a shell starting in that window loses its
/// run to `/dev/null`. Both die with the last descriptor, so a crash frees them like an exit.
fn whole_file(kind: libc::c_int) -> libc::flock {
    libc::flock {
        l_type: kind as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    }
}

/// Takes the writer's lock, or reports that someone else holds it. `file` must be open for writing.
fn take_lock(file: &File) -> io::Result<bool> {
    let lock = whole_file(libc::F_WRLCK);
    // SAFETY: a live descriptor and a fully initialized `flock`. `F_OFD_SETLK` never blocks.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    // Only contention means someone else has it; anything else is reported, not read as busy.
    match err.raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => Ok(false),
        _ => Err(err),
    }
}

/// Whether anyone holds the writer's lock on `file`, which is what says a shell is still writing.
/// A query: it takes nothing.
fn is_locked(file: &File) -> io::Result<bool> {
    let mut lock = whole_file(libc::F_WRLCK);
    // SAFETY: as `take_lock`. `F_OFD_GETLK` only reads, overwriting `lock` with the holder it
    // found or `F_UNLCK`.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(lock.l_type != libc::F_UNLCK as libc::c_short)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--follow`'s stop condition, and why it is `F_OFD_GETLK` and not `flock`. OFD locks
    /// conflict between two descriptions in one process as they do between processes, so a second
    /// open stands in for a second shell.
    #[test]
    fn the_writer_is_identified_by_a_lock_the_reader_can_ask_about_without_taking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obelisk-shell.log");
        let shell = OpenOptions::new().write(true).create(true).truncate(true).open(&path).unwrap();
        let reader = File::open(&path).unwrap();

        assert!(!is_locked(&reader).unwrap(), "an unlocked log has nobody writing it");
        assert!(take_lock(&shell).unwrap(), "a free lock is takeable");
        assert!(is_locked(&reader).unwrap(), "a held lock reads as a live writer, through a read-only descriptor");

        // Probing must leave the lock where it found it, or a shell starting inside a reader's
        // poll spends its whole run logging to /dev/null.
        let second_shell = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(!take_lock(&second_shell).unwrap(), "a second shell loses to the first, and does not truncate");
        drop(second_shell);

        // What a crash does, since the kernel closes the descriptors either way.
        drop(shell);
        assert!(!is_locked(&reader).unwrap(), "the last descriptor closing ends the follow");
    }
}
