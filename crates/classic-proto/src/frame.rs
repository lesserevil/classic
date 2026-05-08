use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum on-wire frame size in bytes. Counts `kind + payload` (i.e. the
/// length-prefix value), not the 4-byte length prefix itself, so the largest
/// well-formed frame is `4 + MAX_FRAME_SIZE` bytes on the wire.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Named frame-kind discriminants the workspace cares about. `Frame` itself
/// stores the kind as a raw `u16` so unknown kinds round-trip without loss;
/// this enum just names the kinds used by classic-proto and downstream
/// crates so call sites can match on the variant rather than on a magic
/// hex literal.
///
/// Range allocations come from ARCHITECTURE.md § "Frame-kind allocation":
/// `0x0000..=0x00FF` proto, `0x0100..=0x01FF` ad, and so on. Each crate
/// owns its range; values here are not exhaustive of the on-wire space.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[repr(u16)]
pub enum FrameKind {
    // proto range (0x0000..=0x00FF)
    Hello = 0x0001,
    Heartbeat = 0x0002,
    Bye = 0x0003,
    Error = 0x0004,
    // ad range (0x0100..=0x01FF)
    NodeAd = 0x0100,
    AdGossip = 0x0101,
    AdRequest = 0x0102,
    // spawn range (0x0300..=0x03FF) — owned by classic-spawn
    SpawnRequest = 0x0300,
    SpawnAck = 0x0301,
    SpawnDeny = 0x0302,
    ChildStdio = 0x0303,
    ChildExit = 0x0304,
    // place range (0x0500..=0x05FF) — owned by classic-place
    PlaceRequest = 0x0501,
    PlaceResponse = 0x0502,
    PlaceError = 0x0503,
}

impl From<FrameKind> for u16 {
    fn from(k: FrameKind) -> u16 {
        k as u16
    }
}

/// A single wire frame: a `u16` kind plus a payload byte string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u16,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(kind: u16, payload: Bytes) -> Self {
        Self { kind, payload }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(String),
}

/// Encode `f` onto `w` as `[u32 LE length][u16 LE kind][payload]`. Errors with
/// `FrameTooLarge` if `2 + payload.len()` exceeds `MAX_FRAME_SIZE`. The check
/// happens before any bytes are written, so an over-sized frame leaves the
/// stream unchanged.
pub async fn encode_frame<W: AsyncWrite + Unpin>(w: &mut W, f: &Frame) -> Result<(), CodecError> {
    let payload_len = f.payload.len();
    let body_len_usize = payload_len.checked_add(2).ok_or(CodecError::FrameTooLarge(u32::MAX))?;
    if body_len_usize > MAX_FRAME_SIZE as usize {
        let reported = u32::try_from(body_len_usize).unwrap_or(u32::MAX);
        return Err(CodecError::FrameTooLarge(reported));
    }
    let body_len = body_len_usize as u32;
    w.write_all(&body_len.to_le_bytes()).await?;
    w.write_all(&f.kind.to_le_bytes()).await?;
    w.write_all(&f.payload).await?;
    Ok(())
}

/// Decode a single frame from `r`. The length prefix is validated before any
/// payload allocation, so an attacker-controlled `length > MAX_FRAME_SIZE`
/// returns `FrameTooLarge` without consuming arbitrary memory.
pub async fn decode_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, CodecError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let body_len = u32::from_le_bytes(len_buf);
    if body_len > MAX_FRAME_SIZE {
        return Err(CodecError::FrameTooLarge(body_len));
    }
    if body_len < 2 {
        return Err(CodecError::Decode(format!(
            "frame body length {} below minimum (2 bytes for kind)",
            body_len
        )));
    }
    let mut kind_buf = [0u8; 2];
    r.read_exact(&mut kind_buf).await?;
    let kind = u16::from_le_bytes(kind_buf);
    let payload_len = (body_len - 2) as usize;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload).await?;
    Ok(Frame { kind, payload: Bytes::from(payload) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn roundtrip(kind: u16, payload: Bytes) -> Frame {
        // Drive the writer on a separate task to avoid deadlocking when the
        // payload exceeds the duplex buffer.
        let (mut a, mut b) = duplex(64 * 1024);
        let f = Frame::new(kind, payload);
        let writer = tokio::spawn(async move {
            encode_frame(&mut a, &f).await.unwrap();
        });
        let got = decode_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        got
    }

    #[tokio::test]
    async fn empty_payload_roundtrip() {
        let got = roundtrip(FrameKind::Heartbeat as u16, Bytes::new()).await;
        assert_eq!(got.kind, 0x0002);
        assert!(got.payload.is_empty());
    }

    #[tokio::test]
    async fn small_payload_roundtrip() {
        let payload = Bytes::from_static(b"hello classic");
        let got = roundtrip(FrameKind::Hello as u16, payload.clone()).await;
        assert_eq!(got.kind, 0x0001);
        assert_eq!(got.payload, payload);
    }

    #[tokio::test]
    async fn large_payload_roundtrip() {
        let payload = Bytes::from(vec![0xA5u8; 1024 * 1024]);
        let got = roundtrip(0x00FF, payload.clone()).await;
        assert_eq!(got.kind, 0x00FF);
        assert_eq!(got.payload, payload);
    }

    #[tokio::test]
    async fn unknown_kind_roundtrips() {
        let got = roundtrip(0x0FFF, Bytes::from_static(b"x")).await;
        assert_eq!(got.kind, 0x0FFF);
        assert_eq!(&got.payload[..], b"x");
    }

    #[tokio::test]
    async fn encode_oversize_payload_errors_without_writing() {
        let payload = Bytes::from(vec![0u8; (MAX_FRAME_SIZE as usize) - 1]);
        let mut buf: Vec<u8> = Vec::new();
        let err = encode_frame(&mut buf, &Frame::new(0x0001, payload))
            .await
            .expect_err("should have errored");
        match err {
            CodecError::FrameTooLarge(_) => {}
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
        assert!(buf.is_empty(), "encoder must not write any bytes when over-sized");
    }

    #[tokio::test]
    async fn decode_oversize_length_prefix_errors_without_allocating() {
        let mut wire = Vec::new();
        let bogus_len: u32 = MAX_FRAME_SIZE + 1;
        wire.extend_from_slice(&bogus_len.to_le_bytes());
        let mut cursor = std::io::Cursor::new(wire);
        let err = decode_frame(&mut cursor).await.expect_err("should have errored");
        match err {
            CodecError::FrameTooLarge(n) => assert_eq!(n, bogus_len),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn boundary_max_frame_size_is_accepted() {
        let payload_len = (MAX_FRAME_SIZE as usize) - 2;
        let payload = Bytes::from(vec![0xC3u8; payload_len]);
        let (mut a, mut b) = duplex(MAX_FRAME_SIZE as usize + 64);
        let writer = tokio::spawn(async move {
            encode_frame(&mut a, &Frame::new(0x0001, payload)).await.unwrap();
        });
        let got = decode_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got.kind, 0x0001);
        assert_eq!(got.payload.len(), payload_len);
        assert!(got.payload.iter().all(|&x| x == 0xC3));
    }

    #[tokio::test]
    async fn decode_under_minimum_body_len_errors() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&1u32.to_le_bytes()); // body_len = 1, less than 2 (kind)
        wire.push(0u8);
        let mut cursor = std::io::Cursor::new(wire);
        let err = decode_frame(&mut cursor).await.expect_err("should have errored");
        match err {
            CodecError::Decode(_) => {}
            other => panic!("expected Decode error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn frame_kind_discriminants_match_spec() {
        assert_eq!(FrameKind::Hello as u16, 0x0001);
        assert_eq!(FrameKind::Heartbeat as u16, 0x0002);
        assert_eq!(FrameKind::Bye as u16, 0x0003);
        assert_eq!(FrameKind::Error as u16, 0x0004);
    }
}
