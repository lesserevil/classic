//! Control-frame payload types for the proto range (`0x0001..=0x0004`),
//! plus bincode-v2 encode / decode helpers used by every payload in this
//! workspace. The same `bincode::config::legacy()` (fixed-int, little-endian)
//! is used everywhere so frames are byte-for-byte stable across crates.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::frame::CodecError;
use crate::ids::NodeId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloPayload {
    pub proto_version: u32,
    pub node_id: NodeId,
    /// `host:port` the peer can dial back on. Empty if the sender does not
    /// accept inbound connections (CLI clients, tests, etc.).
    pub listen_addr: String,
    /// Reserved for v2 capability negotiation. MUST be `0` in v1; non-zero
    /// values may be rejected by future versions.
    pub capabilities: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub seq: u64,
    /// Sender's monotonic clock at send time, in nanoseconds. Receiver-side
    /// usage is informational (RTT estimation); MUST NOT be relied on for
    /// ordering.
    pub send_time_ns: u64,
}

/// `Bye` frames carry no payload. Encoding produces an empty byte string;
/// decoding rejects any non-empty payload.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ByePayload;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Peer announced an incompatible `proto_version` in its Hello.
    ProtoVersionMismatch,
    /// Peer sent a frame that violates the protocol grammar (wrong frame in
    /// the handshake, malformed payload, kind in a range without a
    /// registered handler at handshake time, etc.).
    ProtocolViolation,
    /// Catch-all for non-protocol failures the peer should know about.
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
}

fn bincode_config() -> bincode::config::Configuration<
    bincode::config::LittleEndian,
    bincode::config::Fixint,
    bincode::config::NoLimit,
> {
    bincode::config::legacy()
}

/// Encode a control-frame payload into bytes using the canonical
/// (fixed-int, little-endian) bincode config.
pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    bincode::serde::encode_to_vec(value, bincode_config())
        .map_err(|e| CodecError::Decode(format!("encode_payload: {e}")))
}

/// Decode a control-frame payload from bytes. The encoded length is required
/// to match `bytes.len()` exactly — trailing bytes are an error rather than
/// silently accepted, so a mismatched payload type cannot be papered over.
pub fn decode_payload<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    let (value, consumed) = bincode::serde::decode_from_slice::<T, _>(bytes, bincode_config())
        .map_err(|e| CodecError::Decode(format!("decode_payload: {e}")))?;
    if consumed != bytes.len() {
        return Err(CodecError::Decode(format!(
            "decode_payload: trailing {} byte(s) after value",
            bytes.len() - consumed
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::PROTO_VERSION;

    #[test]
    fn proto_version_is_one() {
        const _: () = assert!(PROTO_VERSION == 1);
        assert_eq!(PROTO_VERSION, 1);
    }

    #[test]
    fn hello_roundtrip() {
        let h = HelloPayload {
            proto_version: PROTO_VERSION,
            node_id: NodeId([7u8; 16]),
            listen_addr: "10.0.0.1:9100".to_string(),
            capabilities: 0,
        };
        let bytes = encode_payload(&h).unwrap();
        let back: HelloPayload = decode_payload(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn heartbeat_roundtrip() {
        let h = HeartbeatPayload { seq: 42, send_time_ns: 1_700_000_000_000_000_000 };
        let bytes = encode_payload(&h).unwrap();
        let back: HeartbeatPayload = decode_payload(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn bye_roundtrip_is_zero_bytes() {
        let bytes = encode_payload(&ByePayload).unwrap();
        assert!(bytes.is_empty(), "ByePayload must encode to zero bytes");
        let _back: ByePayload = decode_payload(&bytes).unwrap();
    }

    #[test]
    fn error_roundtrip_all_codes() {
        for code in [ErrorCode::ProtoVersionMismatch, ErrorCode::ProtocolViolation, ErrorCode::Internal] {
            let e = ErrorPayload { code, message: format!("{:?}", code) };
            let bytes = encode_payload(&e).unwrap();
            let back: ErrorPayload = decode_payload(&bytes).unwrap();
            assert_eq!(back, e);
        }
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let h = HeartbeatPayload { seq: 1, send_time_ns: 0 };
        let mut bytes = encode_payload(&h).unwrap();
        bytes.push(0xFF);
        let err = decode_payload::<HeartbeatPayload>(&bytes).expect_err("should error");
        assert!(matches!(err, CodecError::Decode(_)));
    }
}
