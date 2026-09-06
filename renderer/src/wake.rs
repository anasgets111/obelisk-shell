//! One eventfd wakes the Wayland thread's poll (ADR-0124). Poll blocks on the connection and this
//! fd with no timeout; the socket thread writes after each handed-over frame and decode workers
//! after each result. The loop runs when work exists, not 66 times per second on a 15ms timer.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

/// Cloneable handle on the shared fd.
#[derive(Clone)]
pub struct Waker(Arc<OwnedFd>);

impl Waker {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the returned fd is owned here and nowhere else.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is valid and owned by this process.
        Ok(Waker(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Makes the fd readable until [`Waker::drain`]. Eventfd counts accumulate, so a burst is one
    /// pending wakeup.
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes eight local bytes to the owned fd. `EAGAIN` means poll is already due.
        let _ = unsafe { libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    /// Clears the count so the next `poll` blocks. Called after wakeup, before servicing the turn,
    /// so a wake during that turn is not lost.
    pub fn drain(&self) {
        let mut count: u64 = 0;
        // SAFETY: reads eight bytes into a local from the owned nonblocking fd; empty is `EAGAIN`.
        let _ = unsafe { libc::read(self.0.as_raw_fd(), (&raw mut count).cast(), 8) };
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Wakes poll when the socket thread drops its `Sender`, which the loop reads as Supervisor exit.
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
