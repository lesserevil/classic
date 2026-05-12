//! Wire payloads for the mbox range (`0x0200..=0x02FF`). All bincode-v2
//! `legacy()` (fixed-int, little-endian) — same config classic-proto
//! and classic-ad use, so these payloads piggy-back on
//! `classic_proto::{encode_payload, decode_payload}`.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use classic_proto::{decode_payload, encode_payload, CodecError, Frame, FrameKind, NetId};

/// Frame-kind constants for the mbox range. Exposed as `u16` so call
/// sites can match against the raw `frame.kind` without an extra cast.
pub const FRAME_MAIL_SEND: u16 = FrameKind::MailSend as u16;
pub const FRAME_MAIL_DELIVERY_FAILURE: u16 = FrameKind::MailDeliveryFailure as u16;
pub const FRAME_SERVICE_AD: u16 = FrameKind::ServiceAd as u16;
pub const FRAME_SERVICE_FORGET: u16 = FrameKind::ServiceForget as u16;
pub const FRAME_SERVICE_SYNC: u16 = FrameKind::ServiceSync as u16;
pub const FRAME_SERVICE_SYNC_RESPONSE: u16 = FrameKind::ServiceSyncResponse as u16;

/// Inclusive range of mbox-owned frame kinds.
pub const MBOX_RANGE: std::ops::RangeInclusive<u16> = 0x0200..=0x02FF;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSend {
    pub from: NetId,
    pub to: NetId,
    /// Payload bytes. Cap is enforced at `mail_send` time in send.rs
    /// (MAX_MAIL_BYTES = 8 MiB); decoders also verify on receipt.
    pub payload: Vec<u8>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryFailureReason {
    UnknownMbox,
    MboxFull,
    NodeUnreachable,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailDeliveryFailure {
    pub to: NetId,
    pub reason: DeliveryFailureReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAd {
    pub name: String,
    pub net_id: NetId,
    pub lamport: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceForget {
    pub name: String,
    pub net_id: NetId,
    pub lamport: u64,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSync;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSyncEntry {
    pub name: String,
    pub net_id: NetId,
    pub lamport: u64,
    pub tombstone: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSyncResponse {
    pub entries: Vec<ServiceSyncEntry>,
}

/// Errors specific to mbox-range frame decoding.
#[derive(Debug, thiserror::Error)]
pub enum MboxFrameError {
    #[error("unknown frame kind {kind:#06x} in mbox range")]
    UnknownFrameKind { kind: u16 },
    #[error("frame kind {kind:#06x} is not in the mbox range 0x0200..=0x02FF")]
    OutOfRange { kind: u16 },
    #[error("MailSend payload exceeds 8 MiB ({0} bytes)")]
    PayloadTooLarge(usize),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
}

const MAX_MAIL_PAYLOAD: usize = 8 * 1024 * 1024;

/// Normalized inbound view of a mbox-range frame. Subsystems (delivery,
/// service directory) match on this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MboxInbound {
    MailSend(MailSend),
    MailDeliveryFailure(MailDeliveryFailure),
    ServiceAd(ServiceAd),
    ServiceForget(ServiceForget),
    ServiceSync,
    ServiceSyncResponse(ServiceSyncResponse),
}

pub fn encode_mail_send(m: &MailSend) -> Result<Frame, MboxFrameError> {
    if m.payload.len() > MAX_MAIL_PAYLOAD {
        return Err(MboxFrameError::PayloadTooLarge(m.payload.len()));
    }
    let bytes = encode_payload(m)?;
    Ok(Frame::new(FRAME_MAIL_SEND, Bytes::from(bytes)))
}

pub fn encode_mail_delivery_failure(m: &MailDeliveryFailure) -> Result<Frame, MboxFrameError> {
    Ok(Frame::new(
        FRAME_MAIL_DELIVERY_FAILURE,
        Bytes::from(encode_payload(m)?),
    ))
}

pub fn encode_service_ad(m: &ServiceAd) -> Result<Frame, MboxFrameError> {
    Ok(Frame::new(FRAME_SERVICE_AD, Bytes::from(encode_payload(m)?)))
}

pub fn encode_service_forget(m: &ServiceForget) -> Result<Frame, MboxFrameError> {
    Ok(Frame::new(
        FRAME_SERVICE_FORGET,
        Bytes::from(encode_payload(m)?),
    ))
}

pub fn encode_service_sync() -> Result<Frame, MboxFrameError> {
    Ok(Frame::new(
        FRAME_SERVICE_SYNC,
        Bytes::from(encode_payload(&ServiceSync)?),
    ))
}

pub fn encode_service_sync_response(m: &ServiceSyncResponse) -> Result<Frame, MboxFrameError> {
    Ok(Frame::new(
        FRAME_SERVICE_SYNC_RESPONSE,
        Bytes::from(encode_payload(m)?),
    ))
}

/// Decode any mbox-range frame to its typed `MboxInbound`. Unknown
/// kinds in the range produce `UnknownFrameKind`; out-of-range kinds
/// produce `OutOfRange`.
pub fn decode_mbox_frame(frame: &Frame) -> Result<MboxInbound, MboxFrameError> {
    if !MBOX_RANGE.contains(&frame.kind) {
        return Err(MboxFrameError::OutOfRange { kind: frame.kind });
    }
    match frame.kind {
        FRAME_MAIL_SEND => {
            let m: MailSend = decode_payload(&frame.payload)?;
            if m.payload.len() > MAX_MAIL_PAYLOAD {
                return Err(MboxFrameError::PayloadTooLarge(m.payload.len()));
            }
            Ok(MboxInbound::MailSend(m))
        }
        FRAME_MAIL_DELIVERY_FAILURE => {
            Ok(MboxInbound::MailDeliveryFailure(decode_payload(&frame.payload)?))
        }
        FRAME_SERVICE_AD => Ok(MboxInbound::ServiceAd(decode_payload(&frame.payload)?)),
        FRAME_SERVICE_FORGET => Ok(MboxInbound::ServiceForget(decode_payload(&frame.payload)?)),
        FRAME_SERVICE_SYNC => {
            let _: ServiceSync = decode_payload(&frame.payload)?;
            Ok(MboxInbound::ServiceSync)
        }
        FRAME_SERVICE_SYNC_RESPONSE => Ok(MboxInbound::ServiceSyncResponse(decode_payload(
            &frame.payload,
        )?)),
        other => Err(MboxFrameError::UnknownFrameKind { kind: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_proto::{MboxId, NodeId};

    fn nid(n: u8) -> NodeId {
        NodeId([n; 16])
    }
    fn netid(node: u8, mbox: u64) -> NetId {
        NetId { node: nid(node), mbox: MboxId(mbox) }
    }

    #[test]
    fn frame_kind_discriminants_match_spec() {
        assert_eq!(FRAME_MAIL_SEND, 0x0200);
        assert_eq!(FRAME_MAIL_DELIVERY_FAILURE, 0x0201);
        assert_eq!(FRAME_SERVICE_AD, 0x0210);
        assert_eq!(FRAME_SERVICE_FORGET, 0x0211);
        assert_eq!(FRAME_SERVICE_SYNC, 0x0212);
        assert_eq!(FRAME_SERVICE_SYNC_RESPONSE, 0x0213);
    }

    #[test]
    fn mail_send_roundtrip() {
        let m = MailSend {
            from: netid(1, 7),
            to: netid(2, 9),
            payload: b"hello classic".to_vec(),
        };
        let frame = encode_mail_send(&m).unwrap();
        assert_eq!(frame.kind, FRAME_MAIL_SEND);
        match decode_mbox_frame(&frame).unwrap() {
            MboxInbound::MailSend(back) => assert_eq!(back, m),
            other => panic!("expected MailSend, got {other:?}"),
        }
    }

    #[test]
    fn mail_send_oversize_rejected_on_encode() {
        let m = MailSend {
            from: netid(1, 1),
            to: netid(2, 2),
            payload: vec![0u8; MAX_MAIL_PAYLOAD + 1],
        };
        let err = encode_mail_send(&m).unwrap_err();
        assert!(matches!(err, MboxFrameError::PayloadTooLarge(_)));
    }

    #[test]
    fn mail_delivery_failure_roundtrip_all_reasons() {
        for reason in [
            DeliveryFailureReason::UnknownMbox,
            DeliveryFailureReason::MboxFull,
            DeliveryFailureReason::NodeUnreachable,
        ] {
            let m = MailDeliveryFailure { to: netid(1, 1), reason };
            let frame = encode_mail_delivery_failure(&m).unwrap();
            match decode_mbox_frame(&frame).unwrap() {
                MboxInbound::MailDeliveryFailure(back) => assert_eq!(back, m),
                other => panic!("expected MailDeliveryFailure, got {other:?}"),
            }
        }
    }

    #[test]
    fn service_ad_and_forget_roundtrip() {
        let ad = ServiceAd { name: "registry".into(), net_id: netid(1, 5), lamport: 42 };
        let frame = encode_service_ad(&ad).unwrap();
        match decode_mbox_frame(&frame).unwrap() {
            MboxInbound::ServiceAd(back) => assert_eq!(back, ad),
            other => panic!("got {other:?}"),
        }
        let f = ServiceForget { name: "registry".into(), net_id: netid(1, 5), lamport: 43 };
        let frame = encode_service_forget(&f).unwrap();
        match decode_mbox_frame(&frame).unwrap() {
            MboxInbound::ServiceForget(back) => assert_eq!(back, f),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn service_sync_and_response_roundtrip() {
        let frame = encode_service_sync().unwrap();
        match decode_mbox_frame(&frame).unwrap() {
            MboxInbound::ServiceSync => {}
            other => panic!("got {other:?}"),
        }
        let r = ServiceSyncResponse {
            entries: vec![
                ServiceSyncEntry { name: "a".into(), net_id: netid(1, 1), lamport: 1, tombstone: false },
                ServiceSyncEntry { name: "b".into(), net_id: netid(2, 2), lamport: 3, tombstone: true },
            ],
        };
        let frame = encode_service_sync_response(&r).unwrap();
        match decode_mbox_frame(&frame).unwrap() {
            MboxInbound::ServiceSyncResponse(back) => assert_eq!(back, r),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_in_mbox_range_rejected() {
        let f = Frame::new(0x0242, Bytes::from_static(b""));
        let err = decode_mbox_frame(&f).unwrap_err();
        assert!(matches!(err, MboxFrameError::UnknownFrameKind { kind: 0x0242 }));
    }

    #[test]
    fn out_of_range_kind_rejected() {
        let f = Frame::new(0x0042, Bytes::from_static(b""));
        let err = decode_mbox_frame(&f).unwrap_err();
        assert!(matches!(err, MboxFrameError::OutOfRange { kind: 0x0042 }));
    }

    #[test]
    fn all_frame_kinds_in_owned_range() {
        for k in [
            FRAME_MAIL_SEND,
            FRAME_MAIL_DELIVERY_FAILURE,
            FRAME_SERVICE_AD,
            FRAME_SERVICE_FORGET,
            FRAME_SERVICE_SYNC,
            FRAME_SERVICE_SYNC_RESPONSE,
        ] {
            assert!(MBOX_RANGE.contains(&k), "{k:#06x} outside mbox range");
        }
    }
}
