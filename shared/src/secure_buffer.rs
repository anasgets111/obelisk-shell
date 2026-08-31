//! Native buffer for typed secrets (passwords) that must never live in the Lua VM heap
//! (ADR-0005). `textfield`'s `secure_submit` path is the writer: the Renderer reads the keyboard
//! itself for a focused `secure_submit` field and pushes the bytes straight in here, never as a
//! Lua string and never through an input method.
//!
//! ADR-0005 explicitly distrusts `Drop` timing alone under a panic or early return: the caller
//! must call `.zeroize()` itself right after the one sanctioned read (`expose_secret`,
//! serializing into an outgoing IPC envelope). `Drop` still zeroizes as a backup, via
//! `zeroize::ZeroizeOnDrop`, in case that call is skipped.

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

    /// Appends `s`'s UTF-8 bytes. Called once per `commit_string` edit diff (input method,
    /// ADR-0009) and once per keystroke on a lock screen's keyboard path.
    ///
    /// Growth is handled manually rather than left to `Vec`: `Vec`'s own reallocation frees the
    /// old block without zeroing it first, handing a plaintext prefix of the secret to the
    /// allocator unscrubbed -- past what the later `.zeroize()`/`Drop` can reach, since both only
    /// touch the *current* backing allocation. So this allocates new storage itself, copies the
    /// bytes across, zeroizes the old storage in place, and only then lets it drop.
    pub fn push_str(&mut self, s: &str) {
        let needed = self.bytes.len() + s.len();
        if needed > self.bytes.capacity() {
            let mut grown = Vec::with_capacity(needed);
            grown.extend_from_slice(&self.bytes);
            // `Vec<u8>: Zeroize` clears in place without reallocating, so this can't itself
            // trigger the bug it's guarding against.
            let mut old = std::mem::replace(&mut self.bytes, grown);
            old.zeroize();
        }
        self.bytes.extend_from_slice(s.as_bytes());
    }

    /// Backspace on a `secure_submit` field: drops the last UTF-8 character and zeroizes the
    /// bytes it dropped, in place, before shortening the length.
    ///
    /// A `Vec::truncate` alone would leave the deleted byte live in the backing allocation for
    /// as long as the user keeps typing -- the explicit `.zeroize()` that ends a submit is far
    /// too late, since a lock screen holds this buffer across a whole password entry, corrections
    /// included (docs/adr/0005).
    ///
    /// **A whole scalar, not a byte.** `expose_secret` sends the bytes straight into an IPC
    /// envelope with no second UTF-8 decode to catch a split character, so removing one byte of
    /// a multi-byte scalar would put an invalid sequence on the wire. `rposition` on the
    /// non-continuation bytes finds the last scalar's start, the only index a delete may cut at.
    ///
    /// Returns `false` on an empty buffer -- Backspace in an empty field, not an error.
    pub fn pop_char(&mut self) -> bool {
        let Some(start) = self.bytes.iter().rposition(|byte| (byte & 0xC0) != 0x80) else {
            return false;
        };
        // `truncate` neither reallocates nor frees, so the scrubbed bytes stay inside this same
        // allocation rather than being handed back to the allocator.
        self.bytes[start..].zeroize();
        self.bytes.truncate(start);
        true
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// How many characters have been typed, for a masked field to draw that many mask glyphs.
    ///
    /// Characters, not [`Self::len`]'s bytes: a password with one non-ASCII character would
    /// otherwise draw two or three dots for one keystroke, and a user counting dots against what
    /// they typed is the entire reason a mask is drawn at all.
    ///
    /// Counting non-continuation bytes rather than decoding: the same `(byte & 0xC0) != 0x80`
    /// test [`Self::pop_char`] already uses to find a scalar boundary, and it touches the bytes
    /// without copying any of them out. This is the only read that is not `expose_secret`, and
    /// what it discloses is the length -- which is exactly what a row of dots on screen
    /// discloses anyway.
    pub fn char_count(&self) -> usize {
        self.bytes.iter().filter(|byte| (*byte & 0xC0) != 0x80).count()
    }

    /// The one sanctioned read: crossing the trust boundary to serialize this secret into an
    /// outgoing IPC envelope. Callers must call `.zeroize()` right after (ADR-0005) -- don't
    /// rely on `Drop` alone.
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_count_counts_characters_not_bytes() {
        let mut buf = SecureBuffer::new();
        buf.push_str("pa\u{00df}w\u{00f6}rd");
        assert_eq!(buf.len(), 9, "two of these seven characters are two bytes each");
        assert_eq!(buf.char_count(), 7, "a masked field must draw one dot per keystroke, not per byte");
    }

    #[test]
    fn char_count_follows_a_backspace() {
        let mut buf = SecureBuffer::new();
        buf.push_str("ab\u{00e9}");
        assert_eq!(buf.char_count(), 3);
        assert!(buf.pop_char());
        assert_eq!(buf.char_count(), 2, "deleting one multi-byte character removes exactly one dot");
    }

    #[test]
    fn an_empty_buffer_has_no_characters() {
        assert_eq!(SecureBuffer::new().char_count(), 0);
    }

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

    /// The real trust-boundary assertion: after `.zeroize()`, the *entire backing allocation* --
    /// including bytes past the new length a naive `.clear()` would leave on the heap -- reads
    /// back as zero. Reading past `len()` into `capacity()` here is defined behavior (the Vec
    /// still owns and hasn't deallocated that memory), unlike reading memory after the buffer has
    /// been dropped and freed (see the separate Drop test in
    /// shared/tests/secure_buffer_drop_zeroizes.rs).
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

    /// The reason [`SecureBuffer::pop_char`] exists rather than a bare `Vec::truncate`: a lock
    /// screen holds a live buffer for as long as the user is typing, and a deleted character must
    /// not still be readable out of this process's heap in the meantime.
    #[test]
    fn pop_char_zeroizes_the_bytes_it_removes() {
        let mut buf = SecureBuffer::new();
        buf.push_str("hunter2");
        let ptr = buf.bytes.as_ptr();

        assert!(buf.pop_char());

        assert_eq!(buf.expose_secret(), b"hunter");
        // SAFETY: `pop_char` truncates without deallocating, so the allocation this pointer names
        // is still owned by `buf.bytes` and the byte past the new length is valid to read as a
        // plain `u8`.
        assert_eq!(unsafe { *ptr.add(6) }, 0, "the removed byte was left in the backing allocation");
    }

    /// A multi-byte character is one Backspace, not one byte of one: `expose_secret` serializes
    /// straight onto the wire with no second decode to catch a split scalar (docs/adr/0005).
    #[test]
    fn pop_char_removes_a_whole_utf8_scalar_and_reports_an_empty_buffer() {
        let mut buf = SecureBuffer::new();
        buf.push_str("a\u{e9}");
        assert_eq!(buf.len(), 3);

        assert!(buf.pop_char());
        assert_eq!(buf.expose_secret(), b"a");

        assert!(buf.pop_char());
        assert!(buf.is_empty());
        assert!(!buf.pop_char());
    }
}
