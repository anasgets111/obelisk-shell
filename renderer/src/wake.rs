//! One eventfd that wakes the Wayland thread's poll (ADR-0124). The thread blocks in `poll` on
//! the connection fd and this one, with no timeout: the socket thread writes it after every frame
//! it hands over, a decode worker after every result, so the loop runs a turn when there is a
//! turn to run and not sixty-six times a second on the chance of one, which is what the 15ms poll
//! this replaces did.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

/// Cloneable handle on the one fd; every clone wakes the same poll.
#[derive(Clone)]
pub struct Waker(Arc<OwnedFd>);

impl Waker {
    pub fn new() -> io::Result<Self> {
        // SAFETY: a plain syscall; the returned fd is owned here and nowhere else.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a valid descriptor this process just received and owns.
        Ok(Waker(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Makes the fd readable. Idempotent until the next [`Waker::drain`]: an eventfd counts, and
    /// a count already pending just grows, so a burst of frames is one wakeup.
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes eight bytes from a local into an fd this handle owns. A full counter
        // (`EAGAIN`) means the poll is already due to wake, so the error is not one.
        let _ = unsafe { libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    /// Clears the count so the next `poll` blocks again. Called after every wakeup, before the
    /// turn that services it, so a wake arriving during the turn is not lost.
    pub fn drain(&self) {
        let mut count: u64 = 0;
        // SAFETY: reads eight bytes into a local from an fd this handle owns; nonblocking, so an
        // empty counter is `EAGAIN` and nothing else.
        let _ = unsafe { libc::read(self.0.as_raw_fd(), (&raw mut count).cast(), 8) };
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Wakes the poll when dropped, for a thread whose exit the loop has to notice: the socket
/// thread's, whose `Sender` dropping is what the loop reads as the Supervisor being gone.
pub struct WakeOnDrop(pub Waker);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        self.0.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(waker: &Waker) -> bool {
        let mut fds = [nix::poll::PollFd::new(waker.fd(), nix::poll::PollFlags::POLLIN)];
        matches!(nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO), Ok(1))
    }

    #[test]
    fn a_wake_makes_the_fd_readable_once_and_a_drain_clears_it() {
        let waker = Waker::new().unwrap();
        assert!(!readable(&waker));
        waker.wake();
        waker.wake();
        assert!(readable(&waker), "two wakes are one pending wakeup");
        waker.drain();
        assert!(!readable(&waker));
    }

    #[test]
    fn dropping_the_guard_wakes() {
        let waker = Waker::new().unwrap();
        drop(WakeOnDrop(waker.clone()));
        assert!(readable(&waker));
    }
}
