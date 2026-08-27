//! Native buffer for typed secrets (passwords) that must never live in the Lua VM
//! heap (ADR-0005). `textfield`'s `secure_submit` path is the eventual writer: keystrokes
//! land here directly, never as a Lua string.
//!
//! ADR-0005 explicitly distrusts `Drop` timing alone under a panic or early return: the
//! caller must call `.zeroize()` itself right after the one sanctioned read
//! (`expose_secret`, serializing the secret into an outgoing IPC envelope). `Drop` still
//! zeroizes as a backup, via `zeroize::ZeroizeOnDrop`, in case that call is skipped.

use zeroize::{Zeroize, ZeroizeOnDrop};

/// A growable byte buffer that zeroizes its full backing allocation on an explicit
/// `.zeroize()` call, and again on `Drop` as a backup (ADR-0005).
#[derive(Default, Zeroize, ZeroizeOnDrop)]
pub struct SecureBuffer {
    bytes: Vec<u8>,
}

impl SecureBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `s`'s UTF-8 bytes. The eventual `textfield` write site calls this once per
    /// edit diff (ADR-0009), not once per keystroke.
    ///
    /// Growth is handled manually rather than left to `Vec`: `Vec`'s own reallocation
    /// allocates a new block, copies the old bytes over, and frees the old block without
    /// zeroing it first, which would hand a plaintext prefix of the secret back to the
    /// allocator unscrubbed -- past what the later `.zeroize()` call or `Drop` can reach,
    /// since both only ever touch the *current* backing allocation. When more capacity is
    /// needed, this allocates the new storage itself, copies the bytes across, zeroizes the
    /// old storage in place, and only then lets it drop.
    pub fn push_str(&mut self, s: &str) {
        let needed = self.bytes.len() + s.len();
        if needed > self.bytes.capacity() {
            let mut grown = Vec::with_capacity(needed);
            grown.extend_from_slice(&self.bytes);
            // `Vec<u8>: Zeroize` clears the elements in place without reallocating, so
            // this can't itself trigger the same unscrubbed-free bug it's guarding against.
            let mut old = std::mem::replace(&mut self.bytes, grown);
            old.zeroize();
        }
        self.bytes.extend_from_slice(s.as_bytes());
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The one sanctioned read: crossing the trust boundary to serialize this secret into
    /// an outgoing IPC envelope. Callers must call `.zeroize()` right after that send
    /// completes (ADR-0005) -- don't rely on `Drop` alone.
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_str_is_readable_via_expose_secret() {
        let mut buf = SecureBuffer::new();
        buf.push_str("hunter2");
        assert_eq!(buf.expose_secret(), b"hunter2");
        assert_eq!(buf.len(), 7);
        assert!(!buf.is_empty());
    }

    #[test]
    fn new_buffer_is_empty() {
        let buf = SecureBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.expose_secret(), b"");
    }

    /// The real trust-boundary assertion: after `.zeroize()`, not just the logical length
    /// but the *entire backing allocation* -- including bytes past the new length that a
    /// naive `.clear()` would leave sitting on the heap -- reads back as zero. Reading past
    /// `len()` into `capacity()` here is defined behavior (the Vec still owns and hasn't
    /// deallocated that memory; `u8` has no invalid bit pattern), unlike reading memory
    /// after the buffer itself has been dropped and freed (see the separate Drop test in
    /// shared/tests/secure_buffer_drop_zeroizes.rs, which needs a different, allocator-level
    /// technique to stay within defined behavior).
    #[test]
    fn explicit_zeroize_clears_the_full_backing_allocation() {
        let mut buf = SecureBuffer::new();
        buf.push_str("correct horse battery staple");
        let capacity = buf.bytes.capacity();
        let ptr = buf.bytes.as_ptr();
        assert!(capacity > 0);

        buf.zeroize();

        assert!(buf.is_empty());
        assert_eq!(buf.expose_secret(), b"");
        // SAFETY: `buf.bytes` still owns this allocation (zeroize does not deallocate,
        // only clears), so the pointer and capacity captured above are still valid to
        // read as plain bytes.
        let backing = unsafe { std::slice::from_raw_parts(ptr, capacity) };
        assert!(backing.iter().all(|&b| b == 0), "backing allocation was not fully zeroed");
    }
}
