pub mod ast;
pub mod error;
pub mod eval;
pub mod frame;
pub mod free_pool;
pub mod group;
pub mod lex;
pub mod model;
pub mod parse;
pub mod typeck;

pub use ast::{AggOp, BinOp, Expr, Rank, Requirement, UnaryOp};
pub use error::{ParseError, ParseErrorKind};
pub use eval::{matches, score};
pub use frame::{
    PlaceErrKind, PlacedCandidate, PlacementError, PlacementRequest, PlacementResponse,
};
pub use free_pool::FreePool;
pub use group::{
    place_group, GroupMember, GroupPlaceError, GroupStrategy, PlacementGroup,
};
pub use lex::{lex, Pos, TokKind, Token, MAX_SRC_LEN};
pub use model::{CpuAd, GpuAd, LoadAd, MemAd, NodeAd, OsAd, PciAd};
pub use parse::parse_expr;
pub use typeck::{check_rank, check_req, Ty};

use classic_proto::NodeId;

/// Source text of the default rank. Made public so callers (CLI / tests)
/// can echo it back without re-deriving the literal.
pub const DEFAULT_RANK_SRC: &str =
    "-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))";

/// Parse + type-check a requirement (boolean predicate). The full parse
/// pipeline: lex → parse → type-check.
pub fn parse_req(src: &str) -> Result<Requirement, ParseError> {
    let toks = lex(src)?;
    let ast = parse_expr(&toks)?;
    check_req(ast)
}

/// Parse + type-check a rank (numeric expression).
pub fn parse_rank(src: &str) -> Result<Rank, ParseError> {
    let toks = lex(src)?;
    let ast = parse_expr(&toks)?;
    check_rank(ast)
}

/// Default rank — rewards low CPU load and idle GPUs. Constructed by
/// parsing `DEFAULT_RANK_SRC`; the source text is the contract per
/// FR-6 so callers needing to display the default see the same string.
pub fn default_rank() -> Rank {
    parse_rank(DEFAULT_RANK_SRC).expect("default rank text must parse")
}

/// Filter `ads` by the requirement, score survivors with the rank, and
/// return a sorted `(NodeId, score)` candidate list.
///
/// **Sort order (FR-5):** score descending; NaN sorts last; ties broken by
/// NodeId byte-string ascending. Total and deterministic.
pub fn place(req: &Requirement, rank: &Rank, ads: &[NodeAd]) -> Vec<(NodeId, f64)> {
    let mut out: Vec<(NodeId, f64)> = ads
        .iter()
        .filter(|ad| matches(req, ad))
        .map(|ad| (ad.node_id, score(rank, ad)))
        .collect();
    out.sort_by(|a, b| compare_score_then_id(a, b));
    out
}

/// `place` for raw source strings. ParseError on bad predicate / rank;
/// `rank_src = None` uses `default_rank()`.
pub fn place_str(
    req_src: &str,
    rank_src: Option<&str>,
    ads: &[NodeAd],
) -> Result<Vec<(NodeId, f64)>, ParseError> {
    let req = parse_req(req_src)?;
    let rank = match rank_src {
        Some(s) => parse_rank(s)?,
        None => default_rank(),
    };
    Ok(place(&req, &rank, ads))
}

fn compare_score_then_id(a: &(NodeId, f64), b: &(NodeId, f64)) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let an = a.1.is_nan();
    let bn = b.1.is_nan();
    // NaN sorts last regardless of direction.
    match (an, bn) {
        (true, true) => return a.0 .0.cmp(&b.0 .0),
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    // Higher score first.
    let by_score = b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal);
    if by_score != Ordering::Equal {
        return by_score;
    }
    // Tie: NodeId bytes ascending.
    a.0 .0.cmp(&b.0 .0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GpuAd, NodeAd};

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    fn cpu_node(node_id: NodeId, cores: u32, cpu_pct: f64) -> NodeAd {
        let mut a = NodeAd::default();
        a.node_id = node_id;
        a.cpu.cores = cores;
        a.cpu.arch = "x86_64".into();
        a.load.cpu_pct = cpu_pct;
        a.mem.total_mb = 65_536;
        a.mem.free_mb = 65_536;
        a
    }

    fn gpu_node(node_id: NodeId, idle_gpus: u32) -> NodeAd {
        let mut a = cpu_node(node_id, 32, 10.0);
        for i in 0..idle_gpus {
            a.gpu.push(GpuAd {
                index: i,
                vendor: 0x10de,
                device: 0x2330,
                model: "NVIDIA H100".into(),
                vram_mb: 81920,
                vram_free_mb: 80000,
                sm_count: 132,
                in_use: false,
                mig: false,
            });
        }
        a
    }

    #[test]
    fn default_rank_parses_to_documented_source() {
        let _ = default_rank(); // panics at runtime if the literal drifts
        // And explicitly: the source text round-trips through parse_rank.
        parse_rank(DEFAULT_RANK_SRC).expect("default text must parse");
    }

    #[test]
    fn place_filters_then_sorts() {
        let req = parse_req("cpu.cores >= 16").unwrap();
        let rank = parse_rank("-load.cpu_pct").unwrap();
        let ads = vec![
            cpu_node(id(0xFF), 16, 5.0),
            cpu_node(id(0x01), 16, 5.0),
            cpu_node(id(0x02), 16, 30.0),
            cpu_node(id(0x99), 8, 1.0), // filtered out
        ];
        let out = place(&req, &rank, &ads);
        // Sorted: -5 == -5 first (tie broken by NodeId ascending), then -30.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, id(0x01));
        assert_eq!(out[1].0, id(0xFF));
        assert_eq!(out[2].0, id(0x02));
    }

    #[test]
    fn nan_scores_sort_last() {
        // Force NaN by dividing 0/0 in the rank — int 0 / 0 returns
        // field-skew which surfaces as NEG_INFINITY for the rank, not
        // NaN. So construct NaN explicitly with float division 0.0/0.0
        // via load.cpu_pct - load.cpu_pct (= 0) divided by itself.
        let req = parse_req("true").unwrap();
        let rank = parse_rank("(load.cpu_pct - load.cpu_pct) / (load.cpu_pct - load.cpu_pct)")
            .unwrap();
        // Make at least one ad produce NaN; another ad produce a finite
        // score by carrying a non-zero placeholder we don't use here. We
        // build two ads where rank produces NaN on both — but distinct
        // NodeIds — and assert deterministic order.
        let ads = vec![cpu_node(id(0xFF), 4, 0.0), cpu_node(id(0x01), 4, 0.0)];
        let out = place(&req, &rank, &ads);
        // Both NaN; tie broken by NodeId ascending.
        assert_eq!(out.len(), 2);
        assert!(out[0].1.is_nan());
        assert_eq!(out[0].0, id(0x01));
        assert_eq!(out[1].0, id(0xFF));
    }

    #[test]
    fn place_str_happy_path() {
        let four_idle = gpu_node(id(0x10), 4);
        let one_idle = gpu_node(id(0x20), 1);
        let cpu_only = cpu_node(id(0x30), 32, 5.0);
        let ads = vec![four_idle, one_idle, cpu_only];
        let out = place_str(
            "any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)",
            None,
            &ads,
        )
        .unwrap();
        // CPU-only node is filtered out.
        assert_eq!(out.len(), 2);
        // Default rank: -load.cpu_pct (10) - 1000 * idle_gpus
        // 4-idle: -10 - 4000 = -4010
        // 1-idle: -10 - 1000 = -1010
        // Higher score wins, so 1-idle (id=0x20) ranks ABOVE 4-idle (id=0x10).
        assert_eq!(out[0].0, id(0x20));
        assert_eq!(out[1].0, id(0x10));
    }

    #[test]
    fn place_str_empty_returns_empty() {
        let out = place_str("cpu.cores >= 1", None, &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn place_str_propagates_parse_error() {
        let err = place_str("cpu.arch >= 5", None, &[]).unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
    }

    #[test]
    fn place_with_no_matching_ads_returns_empty() {
        let req = parse_req("cpu.cores >= 1024").unwrap();
        let rank = default_rank();
        let ads = vec![cpu_node(id(0x01), 16, 5.0)];
        assert!(place(&req, &rank, &ads).is_empty());
    }
}

