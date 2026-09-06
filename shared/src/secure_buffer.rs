//! Native buffer for typed secrets that must not enter the Lua VM heap (ADR-0005). For a focused
//! `textfield` `secure_submit`, the Renderer reads the keyboard and pushes bytes here, bypassing
//! Lua strings and input methods.
//!
//! ADR-0005 requires callers to call `.zeroize()` immediately after the one sanctioned read
//! (`expose_secret` into an outgoing IPC envelope), because `Drop` may be delayed by a panic or
//! early return. `ZeroizeOnDrop` remains the backup.

use zeroize::{Zeroize, ZeroizeOnDrop};

/// Growable bytes whose full backing allocation is zeroized explicitly and again on `Drop`
/// (ADR-0005).
#[derive(Default, Zeroize, ZeroizeOnDrop)]
pub struct SecureBuffer {
    bytes: Vec<u8>,
}

impl SecureBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends UTF-8 bytes. Called once per `commit_string` edit diff (input method, ADR-0009) and
    /// once per lock-screen keystroke.
    ///
    /// Growth is manual because `Vec` reallocates by freeing the old block without zeroing it,
    /// leaving a plaintext prefix beyond the current allocation that later `.zeroize()`/`Drop`
    /// cannot reach. Copy into new storage, zeroize the old block, then drop it.
    pub fn push_str(&mut self, s: &str) {
        self.push_bytes(s.as_bytes());
    }

    /// [`Self::push_str`] for callers holding bytes rather than a `str`, which is what
    /// `shared::framing`'s serializer sink has. The growth rule lives here so there is one
    /// implementation of it: a second copy elsewhere is a second place to get it wrong.
    ///
    /// Grows by doubling rather than to exactly `needed`, because a serializer appends in many
    /// small writes and growing per write would copy-and-scrub on each one.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let mut grown = Vec::with_capacity(needed.max(self.bytes.capacity() * 2));
            grown.extend_from_slice(&self.bytes);
            // `Vec<u8>: Zeroize` clears in place without reallocating, so this cannot repeat the
            // reallocation bug guarded against here.
            let mut old = std::mem::replace(&mut self.bytes, grown);
            old.zeroize();
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// Backspace on `secure_submit`: zeroizes the last UTF-8 character in place before shortening
    /// the length.
    ///
    /// `Vec::truncate` would leave deleted bytes live while the user keeps typing; the submit's
    /// later `.zeroize()` is too late because a lock screen holds this buffer through corrections
    /// (ADR-0005).
    ///
    /// Delete a whole scalar, not a byte: `expose_secret` sends bytes straight into an IPC
    /// envelope with no second UTF-8 decode, so a split multi-byte scalar would reach the wire.
    /// `rposition` finds the last non-continuation byte, the only valid cut point.
    ///
    /// Returns `false` for Backspace on an empty field.
    pub fn pop_char(&mut self) -> bool {
        let Some(start) = self.bytes.iter().rposition(|byte| (byte & 0xC0) != 0x80) else {
            return false;
        };
        // `truncate` neither reallocates nor frees, so scrubbed bytes stay in this allocation.
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

    /// Number of typed characters, so a masked field draws one glyph per character.
    ///
    /// Count characters, not [`Self::len`]'s bytes: one non-ASCII character would otherwise draw
    /// two or three dots for one keystroke.
    ///
    /// Count non-continuation bytes with the same `(byte & 0xC0) != 0x80` boundary test as
    /// [`Self::pop_char`], without copying or decoding. This is the only read besides
    /// `expose_secret`; it discloses only the length already shown by the dots.
    pub fn char_count(&self) -> usize {
        self.bytes.iter().filter(|byte| (*byte & 0xC0) != 0x80).count()
    }

    /// The one sanctioned trust-boundary read, for serialization into an outgoing IPC envelope.
    /// Callers must call `.zeroize()` immediately after (ADR-0005), not rely on `Drop` alone.
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn growth_scrubs_the_block_it_leaves_behind() {
        // The trap this exists for: a plain `Vec` frees the old block with the secret still in it,
        // beyond the reach of zeroizing the final buffer.
        let mut buffer = SecureBuffer::new();
        buffer.push_bytes(b"secret");
        let first_block = buffer.expose_secret().as_ptr();
        buffer.push_bytes(&[b'x'; 4096]);
        assert_ne!(
            buffer.expose_secret().as_ptr(),
            first_block,
            "the append must have forced a reallocation for this to be testing anything"
        );
        assert!(buffer.expose_secret().starts_with(b"secretxxx"), "growth must preserve what was already there");
    }

    #[test]
    fn push_str_and_push_bytes_are_the_same_append() {
        let mut from_str = SecureBuffer::new();
        from_str.push_str("hello");
        let mut from_bytes = SecureBuffer::new();
        from_bytes.push_bytes(b"hello");
        assert_eq!(from_str.expose_secret(), from_bytes.expose_secret());
    }
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

    /// After `.zeroize()`, the entire backing allocation, including bytes past `len()` that
    /// `.clear()` would leave on the heap, reads as zero. Reading through `capacity()` is defined
    /// while the Vec still owns the allocation, unlike reading after drop (see
    /// `shared/tests/secure_buffer_drop_zeroizes.rs`).
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
        // SAFETY: `zeroize` clears but does not deallocate; `buf.bytes` still owns the captured
        // pointer and capacity.
        let backing = unsafe { std::slice::from_raw_parts(ptr, capacity) };
        assert!(backing.iter().all(|&b| b == 0), "backing allocation was not fully zeroed");
    }

    /// A lock screen keeps the buffer live while typing, so deleted characters must not remain
    /// readable from the heap. This is why [`SecureBuffer::pop_char`] is not bare `truncate`.
    #[test]
    fn pop_char_zeroizes_the_bytes_it_removes() {
        let mut buf = SecureBuffer::new();
        buf.push_str("hunter2");
        let ptr = buf.bytes.as_ptr();

        assert!(buf.pop_char());

        assert_eq!(buf.expose_secret(), b"hunter");
        // SAFETY: `pop_char` truncates without deallocating, so `buf.bytes` still owns this
        // allocation and the byte past the new length is valid to read.
        assert_eq!(unsafe { *ptr.add(6) }, 0, "the removed byte was left in the backing allocation");
    }

    /// A multi-byte character is one Backspace, not one byte; `expose_secret` sends it straight to
    /// the wire without a second decode to catch a split scalar (ADR-0005).
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
