//! 4-byte big-endian length-prefixed wire framing (build-steps.md Phase 9).
//!
//! Generic over `AsyncRead`/`AsyncWrite` so both a real `UnixStream` and an in-memory duplex pipe
//! drive the same code in tests -- no filesystem needed to exercise framing correctness.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Ceiling on a single frame's payload length, checked against the 4-byte length prefix before
/// any payload bytes are read, so a malformed or hostile prefix can't force an unbounded
/// allocation (up to 4 GiB from a `u32` alone) -- this socket carries secure textfield submissions
/// (ADR-0005).
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

/// Writes `payload` as one frame: a 4-byte big-endian length prefix, then the bytes.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<(), FramingError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FramingError::FrameTooLarge { len: payload.len() });
    }
    let len = payload.len() as u32; // safe: bounded by MAX_FRAME_LEN above, which fits in u32.
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    Ok(())
}

/// Reads one frame: a 4-byte big-endian length prefix, then exactly that many bytes. Rejects an
/// oversized declared length before allocating the payload buffer.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FramingError::FrameTooLarge { len });
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Serializes `value` as JSON and writes it as one frame.
pub async fn write_json_frame<W: AsyncWrite + Unpin, T: Serialize>(writer: &mut W, value: &T) -> Result<(), FramingError> {
    let payload = serde_json::to_vec(value)?;
    write_frame(writer, &payload).await
}

/// Reads one frame and deserializes its payload as JSON.
pub async fn read_json_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> Result<T, FramingError> {
    let payload = read_frame(reader).await?;
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
    async fn write_frame_rejects_a_payload_over_the_max_frame_len_without_writing_anything() {
        let (mut a, _b) = tokio::io::duplex(64);
        let oversized = vec![0u8; MAX_FRAME_LEN + 1];
        let err = write_frame(&mut a, &oversized).await.unwrap_err();
        assert!(matches!(err, FramingError::FrameTooLarge { len } if len == MAX_FRAME_LEN + 1));
    }

    #[tokio::test]
    async fn read_frame_rejects_an_oversized_declared_length_before_reading_any_payload() {
        // A buffer smaller than the claimed length: if read_frame allocated first and then tried
        // to fill it, this would hang waiting for bytes that never arrive.
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
