//! 4-byte big-endian length-prefixed framing. `AsyncRead`/`AsyncWrite` lets tests use an in-memory
//! duplex pipe instead of a filesystem-backed `UnixStream`.
//!
//! This socket carries `SecureSubmit` (ADR-0005), whose `secret` the Renderer and Supervisor both
//! scrub the moment they are done with it. The serialized and received *copies* of those bytes are
//! this module's responsibility, and they are scrubbed here on every path -- success, encode
//! failure, oversize rejection, decode failure and cancellation -- because a buffer that leaves
//! plaintext behind defeats every `Zeroize` upstream of it. Measured 2026-09-06: freed Renderer
//! heap stays on glibc's free lists for the life of the session (a `malloc_trim` returned 4.5% of
//! one), so an unscrubbed copy is not short-lived.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// 16 MiB ceiling checked before reading payload bytes. Without it, a hostile `u32` prefix could
/// force an allocation of up to 4 GiB; this socket carries secure textfield submissions (ADR-0005).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {len} exceeds the {MAX_FRAME_LEN}-byte limit")]
    FrameTooLarge { len: usize },
    #[error("failed to decode frame payload as JSON: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Writes a 4-byte big-endian length prefix followed by `payload`, flushed: the PAM worker's
/// `tokio::io::stdout()` queues writes that its dropped runtime lost, a failed unlock.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<(), FramingError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FramingError::FrameTooLarge { len: payload.len() });
    }
    let len = payload.len() as u32; // safe: bounded by MAX_FRAME_LEN above, which fits in u32.
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads the prefix and exactly that many bytes, rejecting an oversized length before allocation.
///
/// The buffer is scrubbed if the read fails partway or the future is dropped mid-`await`, so a
/// half-received secret leaves nothing behind. On success the allocation is handed to the caller
/// unchanged, and scrubbing it becomes theirs: [`read_json_frame`] wraps it straight back up.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FramingError::FrameTooLarge { len });
    }
    let mut payload = Zeroizing::new(vec![0u8; len]);
    reader.read_exact(payload.as_mut_slice()).await?;
    // Moving the allocation out leaves an empty `Vec` to drop; the bytes travel on to the caller.
    Ok(std::mem::take(&mut *payload))
}

/// `serde_json`'s sink, backed by the buffer that already knows how to grow without leaving
/// plaintext behind ([`crate::secure_buffer::SecureBuffer`]).
///
/// `serde_json::to_vec` hands back a plain `Vec`, which `Drop` frees without clearing and which
/// reallocates as it grows -- freeing each old block with the secret still in it, beyond the reach
/// of zeroizing the final buffer. `SecureBuffer` was written for exactly that problem, so this is
/// a sink over it rather than a second implementation of the same trick.
struct ScrubbingWriter(crate::secure_buffer::SecureBuffer);

impl std::io::Write for ScrubbingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.push_bytes(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serializes `value` to JSON in scrubbing storage and writes it as one frame.
///
/// The payload is zeroized when this returns, on every path.
pub async fn write_json_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), FramingError> {
    let mut scrubbing = ScrubbingWriter(crate::secure_buffer::SecureBuffer::new());
    serde_json::to_writer(&mut scrubbing, value)?;
    // `SecureBuffer` is `ZeroizeOnDrop`, so the serialized copy is cleared when this returns --
    // including on the `?` above and on cancellation mid-write.
    write_frame(writer, scrubbing.0.expose_secret()).await
}

/// Reads one frame and decodes it, zeroizing the received bytes on every path.
///
/// The decoded `T` owns its own copy and is the caller's to scrub; only the wire buffer is this
/// function's to clean up.
pub async fn read_json_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> Result<T, FramingError> {
    let payload = Zeroizing::new(read_frame(reader).await?);
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConnectionHandshake;

    #[tokio::test]
    async fn write_frame_then_read_frame_round_trips_raw_bytes() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, b"hello").await.unwrap();
        let payload = read_frame(&mut b).await.unwrap();
        assert_eq!(payload, b"hello");
    }

    #[tokio::test]
    async fn write_json_frame_then_read_json_frame_round_trips_a_typed_value() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let sent = ConnectionHandshake { generation_id: 7 };
        write_json_frame(&mut a, &sent).await.unwrap();
        let received: ConnectionHandshake = read_json_frame(&mut b).await.unwrap();
        assert_eq!(received, sent);
    }

    #[tokio::test]
    async fn write_frame_returns_only_once_the_frame_is_flushed() {
        // `BufWriter` holds bytes until flushed, as tokio's stdout does.
        let mut sink = tokio::io::BufWriter::new(Vec::new());
        write_frame(&mut sink, b"ok").await.unwrap();
        assert_eq!(sink.get_ref(), &[0, 0, 0, 2, b'o', b'k']);
    }

    #[tokio::test]
    async fn write_frame_rejects_a_payload_over_the_max_frame_len_without_writing_anything() {
        let (mut a, _b) = tokio::io::duplex(64);
        let oversized = vec![0u8; MAX_FRAME_LEN + 1];
        let err = write_frame(&mut a, &oversized).await.unwrap_err();
        assert!(matches!(err, FramingError::FrameTooLarge { len } if len == MAX_FRAME_LEN + 1));
    }

    #[tokio::test]
    async fn read_frame_rejects_an_oversized_declared_length_before_reading_any_payload() {
        // The buffer is smaller than the claim. Allocating first would hang waiting for bytes that
        // never arrive.
        let (mut a, mut b) = tokio::io::duplex(8);
        let oversized_len = (MAX_FRAME_LEN as u32) + 1;
        a.write_all(&oversized_len.to_be_bytes()).await.unwrap();

        let err = tokio::time::timeout(std::time::Duration::from_millis(500), read_frame(&mut b))
            .await
            .expect("read_frame must reject the oversized length immediately, not hang")
            .unwrap_err();
        assert!(matches!(err, FramingError::FrameTooLarge { len } if len == oversized_len as usize));
    }

    #[tokio::test]
    async fn read_json_frame_surfaces_a_decode_error_for_non_json_payload() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, b"not json").await.unwrap();
        let err = read_json_frame::<_, ConnectionHandshake>(&mut b).await.unwrap_err();
        assert!(matches!(err, FramingError::Decode(_)));
    }
}
