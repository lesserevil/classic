//! Frame-kind constants for the ad range (`0x0100..=0x01FF`) plus
//! encode/decode glue between `classic-proto::Frame` and the schema types.
//!
//! Wire format reminder (ARCHITECTURE.md § "Wire transport"):
//! `[length:u32 LE][kind:u16 LE][payload]`. Payload is bincode-v2
//! `legacy()` (fixed-int, little-endian) for byte-compat with classic-proto.

use bytes::Bytes;
use serde::{de::DeserializeOwned, Serialize};

use classic_proto::{CodecError, Frame, FrameKind};

use crate::schema::{AdGossip, AdRequest, NodeAd};

pub const FRAME_NODE_AD: u16 = FrameKind::NodeAd as u16; // 0x0100
pub const FRAME_AD_GOSSIP: u16 = FrameKind::AdGossip as u16; // 0x0101
pub const FRAME_AD_REQUEST: u16 = FrameKind::AdRequest as u16; // 0x0102

/// Inclusive range of ad-owned frame kinds.
pub const AD_RANGE: std::ops::RangeInclusive<u16> = 0x0100..=0x01FF;

#[derive(Debug, thiserror::Error)]
pub enum AdFrameError {
    #[error("unknown frame kind {kind:#06x} in ad range")]
    UnknownFrameKind { kind: u16 },
    #[error("frame kind {kind:#06x} is not in the ad range 0x0100..=0x01FF")]
    OutOfRange { kind: u16 },
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
}

fn config() -> bincode::config::Configuration<
    bincode::config::LittleEndian,
    bincode::config::Fixint,
    bincode::config::NoLimit,
> {
    bincode::config::legacy()
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    bincode::serde::encode_to_vec(value, config())
        .map_err(|e| CodecError::Decode(format!("encode: {e}")))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    let (v, consumed) = bincode::serde::decode_from_slice::<T, _>(bytes, config())
        .map_err(|e| CodecError::Decode(format!("decode: {e}")))?;
    if consumed != bytes.len() {
        return Err(CodecError::Decode(format!(
            "trailing {} byte(s) after value",
            bytes.len() - consumed
        )));
    }
    Ok(v)
}

pub fn encode_node_ad(ad: &NodeAd) -> Result<Frame, AdFrameError> {
    Ok(Frame::new(FRAME_NODE_AD, Bytes::from(encode(ad)?)))
}

pub fn encode_ad_gossip(g: &AdGossip) -> Result<Frame, AdFrameError> {
    Ok(Frame::new(FRAME_AD_GOSSIP, Bytes::from(encode(g)?)))
}

pub fn encode_ad_request(r: &AdRequest) -> Result<Frame, AdFrameError> {
    Ok(Frame::new(FRAME_AD_REQUEST, Bytes::from(encode(r)?)))
}

/// Receiver-side decoder. NodeAd (0x0100) and AdGossip::Full (0x0101) are
/// both folded to a unified `AdInbound::Ad(NodeAd)` so handlers don't need
/// to special-case the two equivalent representations.
pub fn decode_ad_frame(frame: &Frame) -> Result<AdInbound, AdFrameError> {
    if !AD_RANGE.contains(&frame.kind) {
        return Err(AdFrameError::OutOfRange { kind: frame.kind });
    }
    match frame.kind {
        FRAME_NODE_AD => Ok(AdInbound::Ad(decode::<NodeAd>(&frame.payload)?)),
        FRAME_AD_GOSSIP => match decode::<AdGossip>(&frame.payload)? {
            AdGossip::Full(ad) => Ok(AdInbound::Ad(ad)),
            AdGossip::Delta { node_id, generation } => {
                Ok(AdInbound::Delta { node_id: node_id.into(), generation })
            }
        },
        FRAME_AD_REQUEST => Ok(AdInbound::Request(decode::<AdRequest>(&frame.payload)?)),
        other => Err(AdFrameError::UnknownFrameKind { kind: other }),
    }
}

/// Normalized inbound message, hiding the redundancy between `NodeAd` and
/// `AdGossip::Full`. Subsystems (gossip RX, ad store) match on this.
#[derive(Clone, Debug, PartialEq)]
pub enum AdInbound {
    Ad(NodeAd),
    Delta { node_id: classic_proto::NodeId, generation: u64 },
    Request(AdRequest),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{CpuInfo, LoadSample, MemInfo};
    use classic_proto::NodeId;

    fn tiny_ad() -> NodeAd {
        NodeAd {
            node_id: NodeId([1u8; 16]),
            hostname: "h".into(),
            proto_version: 1,
            generation: 1,
            boot_time: 1,
            cpu: CpuInfo {
                cores_online: 1,
                cores_physical: 1,
                sockets: 1,
                model: "m".into(),
                vendor: "v".into(),
                arch: "x86_64".into(),
                mhz: 1,
            },
            mem: MemInfo { total_mb: 1, available_mb: 1 },
            gpus: vec![],
            pci: vec![],
            numa: vec![],
            load: LoadSample {
                loadavg_1m: 0,
                loadavg_5m: 0,
                loadavg_15m: 0,
                cpu_pct: 0,
                mem_pct: 0,
                task_count: 0,
            },
        }
    }

    #[test]
    fn frame_kind_constants_match_spec() {
        assert_eq!(FRAME_NODE_AD, 0x0100);
        assert_eq!(FRAME_AD_GOSSIP, 0x0101);
        assert_eq!(FRAME_AD_REQUEST, 0x0102);
    }

    #[test]
    fn node_ad_roundtrip_via_frame() {
        let ad = tiny_ad();
        let frame = encode_node_ad(&ad).unwrap();
        assert_eq!(frame.kind, FRAME_NODE_AD);
        match decode_ad_frame(&frame).unwrap() {
            AdInbound::Ad(back) => assert_eq!(back, ad),
            other => panic!("expected AdInbound::Ad, got {other:?}"),
        }
    }

    #[test]
    fn ad_gossip_full_decodes_as_ad_inbound_ad() {
        let ad = tiny_ad();
        let frame = encode_ad_gossip(&AdGossip::Full(ad.clone())).unwrap();
        assert_eq!(frame.kind, FRAME_AD_GOSSIP);
        match decode_ad_frame(&frame).unwrap() {
            AdInbound::Ad(back) => assert_eq!(back, ad),
            other => panic!("expected AdInbound::Ad, got {other:?}"),
        }
    }

    #[test]
    fn ad_gossip_delta_decodes_as_delta() {
        let g = AdGossip::Delta { node_id: NodeId([7; 16]), generation: 42 };
        let frame = encode_ad_gossip(&g).unwrap();
        let inbound = decode_ad_frame(&frame).unwrap();
        assert_eq!(
            inbound,
            AdInbound::Delta { node_id: NodeId([7; 16]), generation: 42 }
        );
    }

    #[test]
    fn ad_request_roundtrip_via_frame() {
        let r = AdRequest { from: NodeId([9; 16]) };
        let frame = encode_ad_request(&r).unwrap();
        assert_eq!(frame.kind, FRAME_AD_REQUEST);
        match decode_ad_frame(&frame).unwrap() {
            AdInbound::Request(back) => assert_eq!(back, r),
            other => panic!("expected AdInbound::Request, got {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_in_ad_range_rejected() {
        let f = Frame::new(0x0142, Bytes::from_static(b""));
        let err = decode_ad_frame(&f).unwrap_err();
        match err {
            AdFrameError::UnknownFrameKind { kind } => assert_eq!(kind, 0x0142),
            other => panic!("expected UnknownFrameKind, got {other:?}"),
        }
    }

    #[test]
    fn out_of_range_kind_rejected() {
        let f = Frame::new(0x0042, Bytes::from_static(b""));
        let err = decode_ad_frame(&f).unwrap_err();
        match err {
            AdFrameError::OutOfRange { kind } => assert_eq!(kind, 0x0042),
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }
}
