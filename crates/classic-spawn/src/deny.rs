//! User-visible rendering for `SpawnDeny` and the per-candidate denial
//! aggregator the originator uses to build a terminal denial's `detail`
//! string. Pure formatting; no I/O. The CLI calls `render_deny` to write
//! the stderr message it ultimately exits with.

use classic_proto::{DenyReason, NodeId, SpawnDeny};

/// One candidate's refusal observed by the originator. Carried in
/// `SpawnError::AllCandidatesRefused` so the terminal denial can report
/// per-node detail rather than a flat list of reasons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateDenial {
    pub node: NodeId,
    pub reason: DenyReason,
    pub detail: String,
}

/// Render a SpawnDeny into the stderr message the CLI prints. The
/// prefixes are stable user-visible contract — pinned by golden tests
/// in this module.
pub fn render_deny(deny: &SpawnDeny) -> String {
    match deny.reason {
        DenyReason::NoCandidates => {
            format!("no node matches predicate: {}", deny.detail)
        }
        DenyReason::AllCandidatesRefused => {
            format!("all candidates refused: {}", deny.detail)
        }
        DenyReason::PredicateNotSatisfied => {
            format!("predicate no longer satisfied: {}", deny.detail)
        }
        DenyReason::DeviceTaken => {
            format!("device unavailable: {}", deny.detail)
        }
        DenyReason::CgroupSetupFailed => {
            format!("cgroup setup failed: {}", deny.detail)
        }
        DenyReason::ExecFailed => {
            format!("exec failed: {}", deny.detail)
        }
        DenyReason::HopExceeded => {
            format!("spawn loop guard tripped (hop > 2): {}", deny.detail)
        }
        DenyReason::Internal => {
            format!("spawn refused (internal): {}", deny.detail)
        }
    }
}

/// Format per-candidate denials into the human-readable detail string
/// the originator places on the terminal SpawnDeny. Format:
///
/// ```text
/// node=<id-hex>: <reason> [(<detail>)]; node=<id-hex>: <reason>; ...
/// ```
pub fn format_candidate_denials(denials: &[CandidateDenial]) -> String {
    if denials.is_empty() {
        return "(no candidates were tried)".into();
    }
    let mut out = String::new();
    for (i, d) in denials.iter().enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        out.push_str(&format!("node={}: {:?}", d.node, d.reason));
        if !d.detail.is_empty() {
            out.push_str(&format!(" ({})", d.detail));
        }
    }
    out
}

/// Pick the most informative *terminal* DenyReason from a list of
/// per-candidate denials. If any denial is non-retryable (per the plan's
/// retry table), surface that as the headline reason; otherwise use
/// `AllCandidatesRefused`.
pub fn terminal_reason(denials: &[CandidateDenial]) -> DenyReason {
    if denials.is_empty() {
        return DenyReason::NoCandidates;
    }
    for d in denials {
        match d.reason {
            DenyReason::ExecFailed | DenyReason::HopExceeded => return d.reason,
            _ => {}
        }
    }
    DenyReason::AllCandidatesRefused
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_proto::{NodeId, SpawnDeny};

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    fn deny(reason: DenyReason, detail: &str) -> SpawnDeny {
        SpawnDeny {
            req_id: 1,
            reason,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn render_no_candidates_uses_documented_prefix() {
        let m = render_deny(&deny(DenyReason::NoCandidates, "any(gpu, ...)"));
        assert!(m.starts_with("no node matches predicate:"), "msg: {m}");
    }

    #[test]
    fn render_all_candidates_refused() {
        let m = render_deny(&deny(
            DenyReason::AllCandidatesRefused,
            "node=00...01: DeviceTaken; node=00...02: DeviceTaken",
        ));
        assert!(m.starts_with("all candidates refused:"), "msg: {m}");
    }

    #[test]
    fn render_exec_failed_per_plan_wording() {
        let m = render_deny(&deny(DenyReason::ExecFailed, "ENOENT"));
        assert!(m.starts_with("exec failed:"), "msg: {m}");
    }

    #[test]
    fn render_each_reason_has_a_distinct_prefix() {
        // Every variant should produce a different leading word so the
        // CLI rendering can be skim-read by users.
        let prefixes: Vec<_> = [
            DenyReason::NoCandidates,
            DenyReason::AllCandidatesRefused,
            DenyReason::PredicateNotSatisfied,
            DenyReason::DeviceTaken,
            DenyReason::CgroupSetupFailed,
            DenyReason::ExecFailed,
            DenyReason::HopExceeded,
            DenyReason::Internal,
        ]
        .into_iter()
        .map(|r| {
            let m = render_deny(&deny(r, "x"));
            m.split(':').next().unwrap_or("").to_string()
        })
        .collect();
        let unique: std::collections::HashSet<_> = prefixes.iter().collect();
        assert_eq!(unique.len(), prefixes.len(), "prefixes: {:?}", prefixes);
    }

    #[test]
    fn format_denials_empty_list() {
        assert_eq!(
            format_candidate_denials(&[]),
            "(no candidates were tried)"
        );
    }

    #[test]
    fn format_denials_includes_node_and_reason() {
        let s = format_candidate_denials(&[
            CandidateDenial {
                node: id(1),
                reason: DenyReason::DeviceTaken,
                detail: "GpuMinor(0)".into(),
            },
            CandidateDenial {
                node: id(2),
                reason: DenyReason::PredicateNotSatisfied,
                detail: "".into(),
            },
        ]);
        assert!(s.contains("node="), "{s}");
        assert!(s.contains("DeviceTaken"), "{s}");
        assert!(s.contains("(GpuMinor(0))"), "{s}");
        assert!(s.contains("PredicateNotSatisfied"), "{s}");
    }

    #[test]
    fn terminal_reason_promotes_non_retryable_denials() {
        let denials = vec![
            CandidateDenial { node: id(1), reason: DenyReason::DeviceTaken, detail: "".into() },
            CandidateDenial { node: id(2), reason: DenyReason::ExecFailed, detail: "".into() },
        ];
        assert_eq!(terminal_reason(&denials), DenyReason::ExecFailed);
    }

    #[test]
    fn terminal_reason_collapses_to_all_refused_when_only_retryable() {
        let denials = vec![
            CandidateDenial { node: id(1), reason: DenyReason::DeviceTaken, detail: "".into() },
            CandidateDenial { node: id(2), reason: DenyReason::PredicateNotSatisfied, detail: "".into() },
        ];
        assert_eq!(terminal_reason(&denials), DenyReason::AllCandidatesRefused);
    }

    #[test]
    fn terminal_reason_empty_is_no_candidates() {
        assert_eq!(terminal_reason(&[]), DenyReason::NoCandidates);
    }
}
