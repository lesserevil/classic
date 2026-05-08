//! Originator state machine. Drives a `SpawnRequest` through placement,
//! candidate-iteration, and the running phase. Pure logic — the actual
//! network / control-socket wiring lives at the daemon level (Task 7,
//! classic-vo5). This module exposes a `Placer` trait and a `PeerSpawn`
//! trait so the state machine can be tested end-to-end with mocks.

use classic_proto::{DenyReason, NodeId, SpawnRequest, MAX_HOPS};

use crate::error::SpawnError;

/// Originator state per plan-04 diagram:
///
/// ```text
/// Submitted -> Placing -> Trying(idx) -> Running(NodeId)
///                       \           |
///                        `--------> Denied(reason)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OriginatorState {
    Submitted,
    Placing,
    Trying { idx: usize },
    Running { peer: NodeId },
    Denied(DenyReason),
    Done,
}

/// Trait the originator calls to compute a ranked candidate list. Plan-03
/// fills this in with the real `place()` API; tests pass a closure-backed
/// mock.
pub trait Placer: Send + Sync {
    fn place(&self, requires: &str, rank: &str) -> Result<Vec<NodeId>, SpawnError>;
}

/// Outcome of a single peer attempt. The originator's state-machine loop
/// folds this into `Running` / next `Trying` / terminal `Denied`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Ack,
    Deny { reason: DenyReason },
}

/// Trait the originator calls to attempt a spawn against one peer. The
/// daemon-level impl dials the peer, forwards `SpawnRequest`, and waits
/// for `SpawnAck` or `SpawnDeny`. Tests substitute a deterministic mock.
pub trait PeerSpawn: Send + Sync {
    fn try_spawn(&self, peer: NodeId, req: &SpawnRequest) -> Result<AttemptOutcome, SpawnError>;
}

/// Drive the originator state machine to a terminal state. Returns the
/// chosen `NodeId` on success, or a `SpawnError` describing why the spawn
/// could not be placed.
pub fn run_originator(
    req: &SpawnRequest,
    placer: &dyn Placer,
    peer_spawn: &dyn PeerSpawn,
) -> Result<NodeId, SpawnError> {
    if req.hop > MAX_HOPS {
        return Err(SpawnError::HopExceeded(req.hop));
    }
    if req.requires.trim().is_empty() {
        return Err(SpawnError::Parse(
            "requires predicate must be non-empty (use \"true\")".into(),
        ));
    }

    // Submitted -> Placing
    let candidates = placer.place(&req.requires, &req.rank)?;
    if candidates.is_empty() {
        return Err(SpawnError::NoCandidates);
    }

    // Placing -> Trying(0)
    let mut denials = Vec::with_capacity(candidates.len());
    for (idx, peer) in candidates.iter().enumerate() {
        let _state = OriginatorState::Trying { idx };
        match peer_spawn.try_spawn(*peer, req) {
            Ok(AttemptOutcome::Ack) => {
                // Trying(idx) -> Running(peer). Caller now relays
                // stdio / ChildExit via its own pump; this function's
                // job ends at Running.
                return Ok(*peer);
            }
            Ok(AttemptOutcome::Deny { reason }) => {
                denials.push(reason);
                // Per plan: terminal denials short-circuit the loop.
                if !is_retryable_reason(reason) {
                    return Err(SpawnError::AllCandidatesRefused(denials));
                }
                // Else continue to next candidate.
            }
            Err(e) if e.is_retryable() => {
                denials.push(e.deny_reason());
                // Try the next candidate.
            }
            Err(e) => return Err(e),
        }
    }
    Err(SpawnError::AllCandidatesRefused(denials))
}

fn is_retryable_reason(r: DenyReason) -> bool {
    matches!(
        r,
        DenyReason::PredicateNotSatisfied
            | DenyReason::DeviceTaken
            | DenyReason::CgroupSetupFailed
            | DenyReason::Internal
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn req() -> SpawnRequest {
        SpawnRequest {
            req_id: 1,
            requires: "true".into(),
            rank: "".into(),
            argv: vec!["/bin/echo".into(), "hi".into()],
            env: vec![],
            exclusive_device: false,
            stdin_kind: None,
            hop: 0,
        }
    }

    struct StaticPlacer(Vec<NodeId>);
    impl Placer for StaticPlacer {
        fn place(&self, _requires: &str, _rank: &str) -> Result<Vec<NodeId>, SpawnError> {
            Ok(self.0.clone())
        }
    }

    struct ScriptedPeerSpawn {
        outcomes: Mutex<Vec<AttemptOutcome>>,
        attempted: Mutex<Vec<NodeId>>,
    }
    impl ScriptedPeerSpawn {
        fn new(outcomes: Vec<AttemptOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                attempted: Mutex::new(Vec::new()),
            }
        }
    }
    impl PeerSpawn for ScriptedPeerSpawn {
        fn try_spawn(&self, peer: NodeId, _req: &SpawnRequest) -> Result<AttemptOutcome, SpawnError> {
            self.attempted.lock().unwrap().push(peer);
            self.outcomes
                .lock()
                .unwrap()
                .pop()
                .map(Ok)
                .unwrap_or_else(|| Err(SpawnError::Internal("scripted exhausted".into())))
        }
    }

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    #[test]
    fn happy_path_first_candidate_wins() {
        let placer = StaticPlacer(vec![id(1), id(2), id(3)]);
        // Pop returns the LAST element first; we want id(1) to be tried
        // first AND respond Ack — so put Ack at the end of the Vec.
        let peer_spawn = ScriptedPeerSpawn::new(vec![AttemptOutcome::Ack]);
        let chosen = run_originator(&req(), &placer, &peer_spawn).unwrap();
        assert_eq!(chosen, id(1));
        assert_eq!(*peer_spawn.attempted.lock().unwrap(), vec![id(1)]);
    }

    #[test]
    fn ranked_fallback_skips_retryable_denials() {
        // Three candidates; first two retryable-deny, third Acks.
        let placer = StaticPlacer(vec![id(1), id(2), id(3)]);
        let peer_spawn = ScriptedPeerSpawn::new(vec![
            AttemptOutcome::Ack, // popped third (id(3))
            AttemptOutcome::Deny { reason: DenyReason::DeviceTaken }, // popped second (id(2))
            AttemptOutcome::Deny { reason: DenyReason::PredicateNotSatisfied }, // popped first (id(1))
        ]);
        let chosen = run_originator(&req(), &placer, &peer_spawn).unwrap();
        assert_eq!(chosen, id(3));
        assert_eq!(
            *peer_spawn.attempted.lock().unwrap(),
            vec![id(1), id(2), id(3)]
        );
    }

    #[test]
    fn terminal_deny_short_circuits_fallback() {
        let placer = StaticPlacer(vec![id(1), id(2), id(3)]);
        let peer_spawn = ScriptedPeerSpawn::new(vec![
            AttemptOutcome::Ack,
            AttemptOutcome::Deny { reason: DenyReason::ExecFailed },
        ]);
        let err = run_originator(&req(), &placer, &peer_spawn).unwrap_err();
        match err {
            SpawnError::AllCandidatesRefused(reasons) => {
                assert_eq!(reasons.len(), 1);
                assert_eq!(reasons[0], DenyReason::ExecFailed);
            }
            other => panic!("expected AllCandidatesRefused, got {other:?}"),
        }
        // Only the first candidate was attempted before short-circuit.
        assert_eq!(*peer_spawn.attempted.lock().unwrap(), vec![id(1)]);
    }

    #[test]
    fn empty_candidates_yields_no_candidates() {
        let placer = StaticPlacer(vec![]);
        let peer_spawn = ScriptedPeerSpawn::new(vec![]);
        let err = run_originator(&req(), &placer, &peer_spawn).unwrap_err();
        assert!(matches!(err, SpawnError::NoCandidates));
    }

    #[test]
    fn empty_requires_predicate_rejected() {
        let mut r = req();
        r.requires = "".into();
        let placer = StaticPlacer(vec![id(1)]);
        let peer_spawn = ScriptedPeerSpawn::new(vec![AttemptOutcome::Ack]);
        let err = run_originator(&r, &placer, &peer_spawn).unwrap_err();
        assert!(matches!(err, SpawnError::Parse(_)));
    }

    #[test]
    fn hop_counter_exceeded_rejected() {
        let mut r = req();
        r.hop = MAX_HOPS + 1;
        let placer = StaticPlacer(vec![id(1)]);
        let peer_spawn = ScriptedPeerSpawn::new(vec![AttemptOutcome::Ack]);
        let err = run_originator(&r, &placer, &peer_spawn).unwrap_err();
        assert!(matches!(err, SpawnError::HopExceeded(_)));
    }

    #[test]
    fn placer_errors_propagate() {
        struct FailingPlacer;
        impl Placer for FailingPlacer {
            fn place(&self, _: &str, _: &str) -> Result<Vec<NodeId>, SpawnError> {
                Err(SpawnError::Parse("bad predicate".into()))
            }
        }
        let peer_spawn = ScriptedPeerSpawn::new(vec![]);
        let err = run_originator(&req(), &FailingPlacer, &peer_spawn).unwrap_err();
        assert!(matches!(err, SpawnError::Parse(_)));
    }

    #[test]
    fn all_candidates_denied_returns_aggregate() {
        let placer = StaticPlacer(vec![id(1), id(2)]);
        let peer_spawn = ScriptedPeerSpawn::new(vec![
            AttemptOutcome::Deny { reason: DenyReason::DeviceTaken },
            AttemptOutcome::Deny { reason: DenyReason::DeviceTaken },
        ]);
        let err = run_originator(&req(), &placer, &peer_spawn).unwrap_err();
        match err {
            SpawnError::AllCandidatesRefused(reasons) => {
                assert_eq!(reasons.len(), 2);
                assert!(reasons.iter().all(|r| *r == DenyReason::DeviceTaken));
            }
            other => panic!("expected AllCandidatesRefused, got {other:?}"),
        }
    }
}
