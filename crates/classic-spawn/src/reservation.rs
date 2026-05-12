//! Per-node placement-group reservation table.
//!
//! Node-side state machine for Phase-1 (`GroupReserve`) and Phase-2
//! (`GroupCommit` / `GroupAbort`) of the plan-07 group-2PC. Holds
//! caps + slot snapshots between Phase 1 and Phase 2 so the commit
//! path can drive plan-04's local-spawn without re-resolving the
//! request.
//!
//! Concurrency: every mutation is taken under a single `Mutex`. This
//! is sufficient for v1; reservations are small (a few entries per
//! coord, sub-millisecond hold times for the lock) so contention
//! isn't load-bearing yet.
//!
//! The TTL sweeper is the leak-prevention net: a coordinator that
//! crashes between Reserve and Commit leaves caps held; the sweeper
//! reclaims them after `reserve_ttl_ms` regardless of any further
//! coord traffic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::group_proto::{GroupId, GroupReserveFrame, ReservedMember};

/// How often the TTL sweeper wakes. Sweeping is cheap (one mutex
/// acquire + HashMap walk) so this can stay tight.
pub const TTL_SWEEP_PERIOD: Duration = Duration::from_secs(1);

/// Per-member snapshot carried from Phase-1 ack through to Phase-2
/// commit. v1 doesn't actually carry resolved caps (the plan-04
/// reservation machinery is a follow-up); the slot exists so the
/// commit handler has a stable handle to identify which member it's
/// promoting and to round-trip the original request shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedMemberSlot {
    pub label: String,
    pub token: u64,
    /// The member's original request — preserved so the commit
    /// handler can drive a plan-04 local-spawn without going back
    /// across the wire to fetch the argv/env.
    pub member: ReservedMember,
}

/// Lifecycle states for a single group reservation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Held,
    Committing,
    Released,
}

#[derive(Debug)]
struct Reservation {
    state: State,
    members: Vec<ReservedMemberSlot>,
    deadline: Instant,
}

/// Outcome of `ReservationTable::reserve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReserveOutcome {
    Accepted(Vec<(String, u64)>),
    Denied(String),
}

/// Outcome of `ReservationTable::commit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Hand the slots back to the caller so it can drive plan-04
    /// local-spawn per member with the already-reserved caps.
    Proceed(Vec<ReservedMemberSlot>),
    /// Reservation no longer Held — typically TTL expired between
    /// the coord's Phase-1 ack and the Commit frame arriving.
    Failed(String),
}

/// Outcome of `ReservationTable::abort`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AbortOutcome {
    /// Reservation existed and was released (or was already
    /// Released — idempotent per FR-6).
    Acked,
    /// No reservation by that id ever existed on this node. Still
    /// idempotent from the coord's POV; surfaced separately for
    /// observability.
    Unknown,
}

#[derive(Default)]
pub struct ReservationTable {
    entries: Mutex<HashMap<GroupId, Reservation>>,
}

impl ReservationTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Phase-1 handler. The caller supplies an `ad_validator` that
    /// returns `true` iff the predicate currently matches the local
    /// node's ad — racing ad updates may invalidate a member that
    /// the placer thought satisfied.
    ///
    /// Returns `Denied` on:
    /// - duplicate group_id (F7)
    /// - any member's predicate failing local revalidation
    /// - (future) cap exhaustion
    pub fn reserve(
        &self,
        frame: &GroupReserveFrame,
        now: Instant,
        ad_validator: impl Fn(&str) -> bool,
    ) -> ReserveOutcome {
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&frame.group_id) {
            return ReserveOutcome::Denied("duplicate group_id".into());
        }
        // Per-member revalidation against current ad.
        for m in &frame.members {
            if !ad_validator(&m.requires_src) {
                return ReserveOutcome::Denied(format!(
                    "member {} predicate no longer satisfied",
                    m.label
                ));
            }
        }
        let mut tokens = Vec::with_capacity(frame.members.len());
        let mut slots = Vec::with_capacity(frame.members.len());
        for (i, m) in frame.members.iter().enumerate() {
            // Token is deterministic within a frame for testability;
            // production can swap to random. The (group_id, label)
            // pair already identifies a member uniquely.
            let token = (i as u64) ^ 0xA5A5_A5A5;
            tokens.push((m.label.clone(), token));
            slots.push(ReservedMemberSlot {
                label: m.label.clone(),
                token,
                member: m.clone(),
            });
        }
        let deadline = now + Duration::from_millis(frame.reserve_ttl_ms as u64);
        entries.insert(
            frame.group_id,
            Reservation {
                state: State::Held,
                members: slots,
                deadline,
            },
        );
        ReserveOutcome::Accepted(tokens)
    }

    /// Phase-2 handler. Transitions Held -> Committing under the
    /// mutex (so a concurrent TTL sweep can't race the commit). On
    /// success returns the held member slots so the caller can drive
    /// plan-04 local-spawn per member.
    ///
    /// `tokens` must match the Phase-1 ack tokens; mismatched or
    /// missing tokens fail commit (defense against a bug-coord
    /// sending stale tokens).
    pub fn commit(&self, group_id: GroupId, tokens: &[(String, u64)]) -> CommitOutcome {
        let mut entries = self.entries.lock().unwrap();
        let r = match entries.get_mut(&group_id) {
            Some(r) => r,
            None => return CommitOutcome::Failed("unknown group_id".into()),
        };
        match r.state {
            State::Released => return CommitOutcome::Failed("ttl expired".into()),
            State::Committing => return CommitOutcome::Failed("already committing".into()),
            State::Held => {}
        }
        // Verify tokens match held slots.
        if tokens.len() != r.members.len() {
            return CommitOutcome::Failed("token count mismatch".into());
        }
        for ((tlabel, ttok), slot) in tokens.iter().zip(r.members.iter()) {
            if tlabel != &slot.label || *ttok != slot.token {
                return CommitOutcome::Failed(format!("token mismatch for {tlabel}"));
            }
        }
        r.state = State::Committing;
        let slots = r.members.clone();
        // Entry stays in the table during the spawn; callers should
        // call `commit_done` when all members have spawned (or use
        // `abort` if a member fails partway through).
        CommitOutcome::Proceed(slots)
    }

    /// Caller-facing hook: drop the entry once all members for a
    /// committed reservation have been spawned. Returns `true` iff
    /// the entry was present in `Committing` state.
    pub fn commit_done(&self, group_id: GroupId) -> bool {
        let mut entries = self.entries.lock().unwrap();
        match entries.get(&group_id).map(|r| r.state) {
            Some(State::Committing) => {
                entries.remove(&group_id);
                true
            }
            _ => false,
        }
    }

    /// Idempotent abort. Released state is treated as success — the
    /// coord's POV is "the reservation is gone" either way.
    pub fn abort(&self, group_id: GroupId) -> AbortOutcome {
        let mut entries = self.entries.lock().unwrap();
        match entries.remove(&group_id) {
            Some(_) => AbortOutcome::Acked,
            None => AbortOutcome::Unknown,
        }
    }

    /// Sweep entries past their deadline. Returns the list of
    /// released GroupIds (typically for tracing). Held entries
    /// transition to Released and are removed; Committing entries
    /// are left alone (the commit driver is in flight and will call
    /// `commit_done` or `abort`).
    pub fn tick_ttl(&self, now: Instant) -> Vec<GroupId> {
        let mut entries = self.entries.lock().unwrap();
        let mut released = Vec::new();
        let stale: Vec<GroupId> = entries
            .iter()
            .filter_map(|(id, r)| {
                if r.state == State::Held && r.deadline <= now {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        for id in stale {
            entries.remove(&id);
            released.push(id);
        }
        released
    }

    /// Snapshot of currently held/committing group ids — for
    /// observability and tests.
    pub fn live_ids(&self) -> Vec<GroupId> {
        self.entries.lock().unwrap().keys().copied().collect()
    }
}

/// Spawn a tokio task that wakes every `TTL_SWEEP_PERIOD` and reclaims
/// expired reservations. Returns the JoinHandle so the daemon can
/// abort it on shutdown.
pub fn spawn_ttl_sweeper(table: Arc<ReservationTable>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TTL_SWEEP_PERIOD);
        loop {
            ticker.tick().await;
            let released = table.tick_ttl(Instant::now());
            for id in released {
                tracing::info!(?id, "reservation TTL expired");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group_proto::ReservedMember;

    fn gid(b: u8) -> GroupId {
        GroupId([b; 16])
    }

    fn frame(group_id: GroupId, members: Vec<&str>, ttl_ms: u32) -> GroupReserveFrame {
        GroupReserveFrame {
            group_id,
            members: members
                .into_iter()
                .map(|label| ReservedMember {
                    label: label.into(),
                    requires_src: "true".into(),
                    argv: vec![],
                    env: vec![],
                })
                .collect(),
            reserve_ttl_ms: ttl_ms,
        }
    }

    fn always_match(_src: &str) -> bool {
        true
    }
    fn never_match(_src: &str) -> bool {
        false
    }

    #[test]
    fn reserve_accepts_then_lookups_succeed() {
        let t = ReservationTable::new();
        let now = Instant::now();
        let out = t.reserve(&frame(gid(1), vec!["a", "b"], 5000), now, always_match);
        match out {
            ReserveOutcome::Accepted(tokens) => assert_eq!(tokens.len(), 2),
            other => panic!("expected accepted, got {other:?}"),
        }
        assert_eq!(t.live_ids(), vec![gid(1)]);
    }

    #[test]
    fn duplicate_reserve_rejected() {
        let t = ReservationTable::new();
        let now = Instant::now();
        let _ = t.reserve(&frame(gid(1), vec!["a"], 5000), now, always_match);
        let out = t.reserve(&frame(gid(1), vec!["a"], 5000), now, always_match);
        assert_eq!(
            out,
            ReserveOutcome::Denied("duplicate group_id".into()),
        );
    }

    #[test]
    fn reserve_revalidates_against_current_ad() {
        let t = ReservationTable::new();
        let out = t.reserve(
            &frame(gid(1), vec!["a"], 5000),
            Instant::now(),
            never_match,
        );
        match out {
            ReserveOutcome::Denied(reason) => assert!(reason.contains("predicate")),
            other => panic!("expected denied, got {other:?}"),
        }
        assert!(t.live_ids().is_empty());
    }

    #[test]
    fn ttl_sweep_releases_held_caps() {
        let t = ReservationTable::new();
        let now = Instant::now();
        let _ = t.reserve(&frame(gid(1), vec!["a"], 100), now, always_match);
        // Just under deadline: nothing released.
        let released = t.tick_ttl(now + Duration::from_millis(50));
        assert!(released.is_empty());
        assert_eq!(t.live_ids().len(), 1);
        // Past deadline: released.
        let released = t.tick_ttl(now + Duration::from_millis(101));
        assert_eq!(released, vec![gid(1)]);
        assert!(t.live_ids().is_empty());
        // Re-reserving the same id is now allowed.
        let out = t.reserve(
            &frame(gid(1), vec!["a"], 100),
            now + Duration::from_millis(200),
            always_match,
        );
        assert!(matches!(out, ReserveOutcome::Accepted(_)));
    }

    #[test]
    fn commit_after_ttl_returns_failed() {
        let t = ReservationTable::new();
        let now = Instant::now();
        let tokens = match t.reserve(&frame(gid(1), vec!["a"], 100), now, always_match) {
            ReserveOutcome::Accepted(toks) => toks,
            other => panic!("{other:?}"),
        };
        let _ = t.tick_ttl(now + Duration::from_millis(200));
        let out = t.commit(gid(1), &tokens);
        assert_eq!(out, CommitOutcome::Failed("unknown group_id".into()));
    }

    #[test]
    fn abort_idempotent() {
        let t = ReservationTable::new();
        let _ = t.reserve(
            &frame(gid(1), vec!["a"], 5000),
            Instant::now(),
            always_match,
        );
        assert_eq!(t.abort(gid(1)), AbortOutcome::Acked);
        assert_eq!(t.abort(gid(1)), AbortOutcome::Unknown);
    }

    #[test]
    fn commit_proceeds_with_slots() {
        let t = ReservationTable::new();
        let now = Instant::now();
        let tokens = match t.reserve(
            &frame(gid(1), vec!["a", "b"], 5000),
            now,
            always_match,
        ) {
            ReserveOutcome::Accepted(toks) => toks,
            other => panic!("{other:?}"),
        };
        let out = t.commit(gid(1), &tokens);
        match out {
            CommitOutcome::Proceed(slots) => {
                assert_eq!(slots.len(), 2);
                assert_eq!(slots[0].label, "a");
                assert_eq!(slots[1].label, "b");
                assert_eq!(slots[0].token, tokens[0].1);
                assert_eq!(slots[1].token, tokens[1].1);
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
        // Entry stays in Committing until commit_done.
        assert_eq!(t.live_ids(), vec![gid(1)]);
        assert!(t.commit_done(gid(1)));
        assert!(t.live_ids().is_empty());
    }

    #[test]
    fn commit_with_wrong_token_fails() {
        let t = ReservationTable::new();
        let _ = t.reserve(
            &frame(gid(1), vec!["a"], 5000),
            Instant::now(),
            always_match,
        );
        let out = t.commit(gid(1), &[("a".into(), 999_999)]);
        match out {
            CommitOutcome::Failed(reason) => assert!(reason.contains("token mismatch")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn ttl_sweep_skips_committing_entries() {
        // Once commit transitions Held -> Committing, the sweeper must
        // leave the entry alone — the commit driver is in flight.
        let t = ReservationTable::new();
        let now = Instant::now();
        let tokens = match t.reserve(&frame(gid(1), vec!["a"], 100), now, always_match) {
            ReserveOutcome::Accepted(toks) => toks,
            other => panic!("{other:?}"),
        };
        let _ = t.commit(gid(1), &tokens); // -> Committing
        let released = t.tick_ttl(now + Duration::from_millis(500));
        assert!(released.is_empty());
        assert_eq!(t.live_ids(), vec![gid(1)]);
    }
}
