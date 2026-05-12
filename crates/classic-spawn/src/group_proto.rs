//! Wire payloads for the group-2PC frame range (0x0320..=0x0327).
//!
//! Same encoding contract as plan-04's single-spawn frames: bincode v2
//! legacy (fixed-int, little-endian). The structs piggy-back on
//! classic-proto's `encode_payload` / `decode_payload` helpers.
//!
//! The Requirement type from classic-place is NOT serializable directly,
//! so `ReservedMember` carries the predicate's *source string*; the
//! receiving node parses it locally before revalidating against its
//! current ad.

use classic_proto::NetId;
use serde::{Deserialize, Serialize};

/// 128-bit opaque identifier for a single group submission. The
/// coordinator generates one per `submit_group` invocation and echoes
/// it in every group-2PC frame so receivers can correlate.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub [u8; 16]);

impl GroupId {
    /// Generate a fresh random `GroupId` via OS entropy.
    pub fn random() -> Self {
        let mut buf = [0u8; 16];
        getrandom::getrandom(&mut buf).expect("getrandom failed");
        GroupId(buf)
    }
}

/// One member assigned to a specific node by the placer. The
/// receiving node re-parses `requires_src` and revalidates against
/// its current ad — placer decisions can race ad updates and the
/// node has the last word.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedMember {
    pub label: String,
    /// Plan-03 predicate source. Same shape as `SpawnRequest::requires`.
    pub requires_src: String,
    pub argv: Vec<String>,
    /// `KEY=VALUE` pairs. Caller-supplied only; CLI environment is NOT
    /// forwarded. Order is preserved; later wins on duplicate key.
    pub env: Vec<String>,
}

/// Phase-1 coordinator -> node. Asks the node to reserve resources
/// for every member that the placer assigned to it. Reservations
/// hold caps + slots until `GroupCommit` promotes them or
/// `GroupAbort` / TTL releases them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupReserveFrame {
    pub group_id: GroupId,
    pub members: Vec<ReservedMember>,
    /// Node-side timer; the coordinator sets `(reserve_timeout + slack).as_millis()`
    /// so a node never holds a reservation longer than the coordinator
    /// will wait for `Ack`.
    pub reserve_ttl_ms: u32,
}

/// Phase-1 node -> coordinator (success). Carries opaque
/// `reservation_token`s the coordinator echoes back in `GroupCommit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupReserveAck {
    pub group_id: GroupId,
    /// `(member_label, reservation_token)` per accepted member.
    pub tokens: Vec<(String, u64)>,
}

/// Phase-1 node -> coordinator (failure). Cap exhausted, ad changed,
/// member predicate now unsatisfiable, etc.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupReserveDeny {
    pub group_id: GroupId,
    pub reason: String,
}

/// Phase-2 coordinator -> node. Promote reservations to live spawns.
/// `tokens` echo the Phase-1 ack so a node refuses anything it
/// didn't promise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCommit {
    pub group_id: GroupId,
    pub tokens: Vec<(String, u64)>,
}

/// Phase-2 node -> coordinator (success, per member). One frame per
/// successfully spawned member; the coordinator joins on labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpawned {
    pub group_id: GroupId,
    pub label: String,
    pub net_id: NetId,
}

/// Phase-2 node -> coordinator (failure). One per node; carries the
/// reason. The coordinator must Kill any already-Spawned siblings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCommitFailed {
    pub group_id: GroupId,
    pub reason: String,
}

/// Coordinator -> node. Idempotent: a node that already TTL-released
/// is expected to still respond with `GroupAbortAck`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAbort {
    pub group_id: GroupId,
}

/// Node -> coordinator. Confirms the reservation is dropped. The
/// coordinator treats absence of this frame as soft-failed and moves
/// on (the node's TTL sweeper handles any stragglers).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAbortAck {
    pub group_id: GroupId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_proto::{decode_payload, encode_payload, MboxId, NodeId};

    fn rt<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let bytes = encode_payload(v).unwrap();
        decode_payload::<T>(&bytes).unwrap()
    }

    fn gid() -> GroupId {
        GroupId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    }

    #[test]
    fn group_reserve_roundtrip() {
        let f = GroupReserveFrame {
            group_id: gid(),
            members: vec![
                ReservedMember {
                    label: "trainer".into(),
                    requires_src: "any(gpu, gpu.vram_mb >= 80000)".into(),
                    argv: vec!["python".into(), "train.py".into()],
                    env: vec!["RANK=0".into()],
                },
                ReservedMember {
                    label: "eval".into(),
                    requires_src: "true".into(),
                    argv: vec!["python".into(), "eval.py".into()],
                    env: vec![],
                },
            ],
            reserve_ttl_ms: 30_000,
        };
        assert_eq!(rt(&f), f);
    }

    #[test]
    fn group_reserve_ack_and_deny_roundtrip() {
        let ack = GroupReserveAck {
            group_id: gid(),
            tokens: vec![("a".into(), 42), ("b".into(), 99)],
        };
        assert_eq!(rt(&ack), ack);
        let deny = GroupReserveDeny {
            group_id: gid(),
            reason: "cap exhausted".into(),
        };
        assert_eq!(rt(&deny), deny);
    }

    #[test]
    fn group_commit_and_spawned_roundtrip() {
        let c = GroupCommit {
            group_id: gid(),
            tokens: vec![("a".into(), 42)],
        };
        assert_eq!(rt(&c), c);
        let s = GroupSpawned {
            group_id: gid(),
            label: "a".into(),
            net_id: NetId {
                node: NodeId([7; 16]),
                mbox: MboxId(123),
            },
        };
        assert_eq!(rt(&s), s);
    }

    #[test]
    fn group_commit_failed_and_abort_roundtrip() {
        let f = GroupCommitFailed {
            group_id: gid(),
            reason: "execve failed".into(),
        };
        assert_eq!(rt(&f), f);
        let abort = GroupAbort { group_id: gid() };
        assert_eq!(rt(&abort), abort);
        let ack = GroupAbortAck { group_id: gid() };
        assert_eq!(rt(&ack), ack);
    }

    #[test]
    fn group_id_random_returns_distinct_ids() {
        let a = GroupId::random();
        let b = GroupId::random();
        assert_ne!(a, b);
    }
}
