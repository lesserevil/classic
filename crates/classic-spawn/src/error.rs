//! Errors surfaced by the spawn pipeline. Map cleanly to wire
//! `DenyReason` variants per plan §"Failure handling".

use classic_proto::DenyReason;

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// Placement returned an empty candidate list.
    #[error("no candidates matched the requirement")]
    NoCandidates,
    /// Originator tried every ranked candidate; all denied.
    #[error("every candidate denied: {0:?}")]
    AllCandidatesRefused(Vec<DenyReason>),
    /// Executor re-evaluated the predicate on its current ad and it
    /// no longer matched.
    #[error("predicate not satisfied at executor: {0}")]
    PredicateNotSatisfied(String),
    /// One or more requested devices were taken when the executor
    /// tried to acquire them.
    #[error("device taken: {0}")]
    DeviceTaken(String),
    /// cgroup setup failed before fork.
    #[error("cgroup setup: {0}")]
    CgroupSetupFailed(String),
    /// `execve` failed in the helper child.
    #[error("exec failed: {0}")]
    ExecFailed(#[from] crate::exec::ExecError),
    /// Hop counter exceeded `MAX_HOPS`.
    #[error("hop counter exceeded: {0}")]
    HopExceeded(u8),
    /// Catch-all.
    #[error("internal: {0}")]
    Internal(String),
    /// DSL parse / type error on `requires` or `rank`.
    #[error("predicate parse: {0}")]
    Parse(String),
}

impl SpawnError {
    /// Wire-protocol mapping. Used by the executor when it needs to
    /// reply to the originator with a SpawnDeny frame.
    pub fn deny_reason(&self) -> DenyReason {
        match self {
            SpawnError::NoCandidates => DenyReason::NoCandidates,
            SpawnError::AllCandidatesRefused(_) => DenyReason::AllCandidatesRefused,
            SpawnError::PredicateNotSatisfied(_) => DenyReason::PredicateNotSatisfied,
            SpawnError::DeviceTaken(_) => DenyReason::DeviceTaken,
            SpawnError::CgroupSetupFailed(_) => DenyReason::CgroupSetupFailed,
            SpawnError::ExecFailed(_) => DenyReason::ExecFailed,
            SpawnError::HopExceeded(_) => DenyReason::HopExceeded,
            SpawnError::Internal(_) | SpawnError::Parse(_) => DenyReason::Internal,
        }
    }

    /// Per plan §"Failure handling": retry on `PredicateNotSatisfied`,
    /// `DeviceTaken`, `CgroupSetupFailed`, and connect failures (which
    /// surface as `Internal`); never on `ExecFailed` or `HopExceeded`.
    /// `NoCandidates` and `AllCandidatesRefused` are terminal by
    /// definition. `Parse` is terminal — a malformed predicate won't
    /// improve on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SpawnError::PredicateNotSatisfied(_)
                | SpawnError::DeviceTaken(_)
                | SpawnError::CgroupSetupFailed(_)
                | SpawnError::Internal(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_reason_mapping_matches_plan_table() {
        assert_eq!(
            SpawnError::NoCandidates.deny_reason(),
            DenyReason::NoCandidates
        );
        assert_eq!(
            SpawnError::AllCandidatesRefused(vec![]).deny_reason(),
            DenyReason::AllCandidatesRefused
        );
        assert_eq!(
            SpawnError::PredicateNotSatisfied("x".into()).deny_reason(),
            DenyReason::PredicateNotSatisfied
        );
        assert_eq!(
            SpawnError::DeviceTaken("x".into()).deny_reason(),
            DenyReason::DeviceTaken
        );
        assert_eq!(
            SpawnError::HopExceeded(3).deny_reason(),
            DenyReason::HopExceeded
        );
    }

    #[test]
    fn retry_classification_per_plan_table() {
        assert!(SpawnError::PredicateNotSatisfied("x".into()).is_retryable());
        assert!(SpawnError::DeviceTaken("x".into()).is_retryable());
        assert!(SpawnError::CgroupSetupFailed("x".into()).is_retryable());
        assert!(SpawnError::Internal("x".into()).is_retryable());
        assert!(!SpawnError::HopExceeded(3).is_retryable());
        assert!(!SpawnError::NoCandidates.is_retryable());
        assert!(!SpawnError::AllCandidatesRefused(vec![]).is_retryable());
        assert!(!SpawnError::Parse("x".into()).is_retryable());
    }
}
