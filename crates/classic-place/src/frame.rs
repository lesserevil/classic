//! Wire payloads for placement RPCs in the classic-place range
//! (`0x0500..=0x05FF`). v1 wires these through the same dispatch path the
//! v2 networked placement RPC will use, so the carrier swap is boring.
//!
//! Encoding is bincode-v2 `legacy()` (fixed-int, little-endian) for byte
//! compatibility with classic-proto.

use serde::{Deserialize, Serialize};

use classic_proto::NodeId;

/// A placement query: a predicate that filters node ads, optionally a rank
/// expression that orders survivors, plus a result cap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementRequest {
    /// Caller-supplied correlation id. Echoed in the matching response.
    pub req_id: u64,
    /// Predicate source — DSL text (`any(gpu, gpu.vram_mb >= 80000)` etc.).
    pub req_src: String,
    /// Optional rank expression (DSL numeric). `None` means "default rank".
    pub rank_src: Option<String>,
    /// Maximum candidates to return. The matcher may return fewer.
    pub max_results: u16,
}

/// One placement result: a node and its rank score under the supplied
/// rank expression. Higher score wins. NaN sorts last on the receiver.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct PlacedCandidate {
    pub node: NodeId,
    pub score: f64,
}

impl PartialEq for PlacedCandidate {
    /// f64 NaN inequality bites round-trip tests — compare bit patterns
    /// so a `PlacementResponse` with NaN scores can round-trip cleanly.
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.score.to_bits() == other.score.to_bits()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlacementResponse {
    pub req_id: u64,
    pub candidates: Vec<PlacedCandidate>,
}

impl PartialEq for PlacementResponse {
    fn eq(&self, other: &Self) -> bool {
        self.req_id == other.req_id && self.candidates == other.candidates
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaceErrKind {
    /// DSL source could not be parsed (lex or grammar error).
    ParseError,
    /// DSL parsed but the type checker rejected it (e.g. arithmetic on a
    /// boolean, predicate where rank expected).
    TypeError,
    /// Predicate parsed and type-checked, but no node satisfied it.
    NoCandidates,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementError {
    pub req_id: u64,
    pub kind: PlaceErrKind,
    pub message: String,
    /// 1-indexed source line, when `kind` is `ParseError` or `TypeError`.
    pub line: Option<u32>,
    /// 1-indexed source column.
    pub col: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_proto::FrameKind;

    fn config() -> bincode::config::Configuration<
        bincode::config::LittleEndian,
        bincode::config::Fixint,
        bincode::config::NoLimit,
    > {
        bincode::config::legacy()
    }

    fn roundtrip<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let bytes = bincode::serde::encode_to_vec(v, config()).unwrap();
        let (back, consumed) = bincode::serde::decode_from_slice::<T, _>(&bytes, config()).unwrap();
        assert_eq!(consumed, bytes.len());
        back
    }

    #[test]
    fn frame_kind_discriminants_match_spec() {
        assert_eq!(FrameKind::PlaceRequest as u16, 0x0501);
        assert_eq!(FrameKind::PlaceResponse as u16, 0x0502);
        assert_eq!(FrameKind::PlaceError as u16, 0x0503);
    }

    #[test]
    fn frame_kinds_lie_within_owned_range() {
        for k in [
            FrameKind::PlaceRequest as u16,
            FrameKind::PlaceResponse as u16,
            FrameKind::PlaceError as u16,
        ] {
            assert!((0x0500..=0x05FF).contains(&k), "{k:#06x} outside place range");
        }
    }

    #[test]
    fn placement_request_roundtrip_with_rank() {
        let req = PlacementRequest {
            req_id: 42,
            req_src: "any(gpu, gpu.vram_mb >= 80000)".into(),
            rank_src: Some("-load.cpu_pct".into()),
            max_results: 10,
        };
        assert_eq!(roundtrip(&req), req);
    }

    #[test]
    fn placement_request_roundtrip_without_rank() {
        let req = PlacementRequest {
            req_id: 1,
            req_src: "true".into(),
            rank_src: None,
            max_results: 1,
        };
        assert_eq!(roundtrip(&req), req);
    }

    #[test]
    fn placement_response_roundtrip_with_finite_scores() {
        let resp = PlacementResponse {
            req_id: 7,
            candidates: vec![
                PlacedCandidate { node: NodeId([1; 16]), score: 12.5 },
                PlacedCandidate { node: NodeId([2; 16]), score: -4.0 },
                PlacedCandidate { node: NodeId([3; 16]), score: 0.0 },
            ],
        };
        assert_eq!(roundtrip(&resp), resp);
    }

    #[test]
    fn placement_response_roundtrip_preserves_special_floats() {
        let weird = PlacementResponse {
            req_id: 1,
            candidates: vec![
                PlacedCandidate { node: NodeId([1; 16]), score: 0.0 },
                PlacedCandidate { node: NodeId([2; 16]), score: -0.0 },
                PlacedCandidate { node: NodeId([3; 16]), score: f64::INFINITY },
                PlacedCandidate { node: NodeId([4; 16]), score: f64::NEG_INFINITY },
                PlacedCandidate { node: NodeId([5; 16]), score: f64::NAN },
            ],
        };
        let back = roundtrip(&weird);
        // Bit-exact comparison — NaN != NaN under PartialEq, but the
        // PlacedCandidate impl above compares to_bits, so this works.
        assert_eq!(back, weird);
    }

    #[test]
    fn placement_error_roundtrip_all_variants() {
        for (kind, with_pos) in [
            (PlaceErrKind::ParseError, true),
            (PlaceErrKind::TypeError, true),
            (PlaceErrKind::NoCandidates, false),
        ] {
            let err = PlacementError {
                req_id: 99,
                kind,
                message: format!("{:?}", kind),
                line: if with_pos { Some(3) } else { None },
                col: if with_pos { Some(15) } else { None },
            };
            assert_eq!(roundtrip(&err), err);
        }
    }
}
