//! Cluster-wide service directory: Lamport-clock last-writer-wins CRDT
//! mapping service names to one or more `NetId` endpoints.
//!
//! In-process logic only — the gossip task (classic-jja) supplies the
//! wire I/O and calls the `apply_*` mutators below. `service_lookup` /
//! `service_lookup_one` are the live read API.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use classic_proto::NetId;

use crate::error::ServiceError;

/// Maximum service-name length in bytes (UTF-8).
pub const MAX_SVC_NAME: usize = 256;

/// How long tombstones live before the directory may GC them. After
/// TTL expiry, a later-arriving ad with a strictly-greater lamport is
/// accepted even though we no longer remember the tombstone.
pub const TOMBSTONE_TTL: Duration = Duration::from_secs(60);

/// Task identifier used as the back-index key for `by_task` so the
/// auto-GC task (classic-n7u) can `forget` everything a task declared
/// when it exits. Wire-format-wise this is opaque; classic-spawn
/// defines the actual TaskId in plan 04.
pub type TaskId = u64;

pub type Lamport = u64;

#[derive(Clone, Debug)]
pub struct ServiceEntry {
    pub net_id: NetId,
    pub lamport: Lamport,
    pub tombstone: bool,
    pub last_seen: Instant,
}

// Custom orderings: order entries by net_id so a BTreeSet keys by it,
// not by lamport (which can collide). Two entries with the same net_id
// are "the same key" and only one is held at a time — we replace on
// LWW. The BTreeSet enforces this via Ord on `net_id`.
impl PartialEq for ServiceEntry {
    fn eq(&self, other: &Self) -> bool {
        self.net_id == other.net_id
    }
}
impl Eq for ServiceEntry {}
impl PartialOrd for ServiceEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ServiceEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // node bytes ascending, then mbox ascending — same tie-break
        // convention as plan-04 §"Sort order".
        self.net_id
            .node
            .0
            .cmp(&other.net_id.node.0)
            .then_with(|| self.net_id.mbox.0.cmp(&other.net_id.mbox.0))
    }
}

struct Inner {
    entries: Mutex<HashMap<String, BTreeSet<ServiceEntry>>>,
    by_task: Mutex<HashMap<TaskId, Vec<(String, NetId)>>>,
    cursors: Mutex<HashMap<String, AtomicUsize>>,
    local_clock: AtomicU64,
}

impl Inner {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            by_task: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            local_clock: AtomicU64::new(0),
        }
    }
}

static DIR: OnceLock<Inner> = OnceLock::new();

fn dir() -> &'static Inner {
    DIR.get_or_init(Inner::new)
}

fn validate_name(name: &str) -> Result<(), ServiceError> {
    if name.len() > MAX_SVC_NAME {
        return Err(ServiceError::NameTooLong(name.len()));
    }
    Ok(())
}

/// Bump the local Lamport clock and return its new value.
pub fn bump_local_clock() -> Lamport {
    dir().local_clock.fetch_add(1, Ordering::Relaxed) + 1
}

/// Clamp the local clock against an observed remote lamport. After this
/// call, `local_clock >= max(prev, remote) + 1`.
fn observe_remote(lamport: Lamport) {
    let inner = dir();
    let mut current = inner.local_clock.load(Ordering::Relaxed);
    loop {
        let candidate = current.max(lamport).saturating_add(1);
        if candidate <= current {
            break;
        }
        match inner.local_clock.compare_exchange(
            current,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

/// Local `service_declare` mutation. Bumps the local clock, inserts /
/// replaces the entry for `(name, net_id)`. The gossip task is
/// responsible for emitting the resulting `ServiceAd` frame.
///
/// Returns `(lamport, was_changed)` — was_changed is true if the
/// directory's view of `(name, net_id)` actually moved (new entry or
/// strictly-newer state). Local declares always count as changed.
pub fn apply_local_declare(
    task_id: TaskId,
    name: &str,
    net_id: NetId,
) -> Result<(Lamport, bool), ServiceError> {
    validate_name(name)?;
    let lamport = bump_local_clock();
    let entry = ServiceEntry {
        net_id,
        lamport,
        tombstone: false,
        last_seen: Instant::now(),
    };
    {
        let mut entries = dir().entries.lock().expect("entries poisoned");
        let bucket = entries.entry(name.to_string()).or_default();
        bucket.replace(entry);
    }
    {
        let mut by_task = dir().by_task.lock().expect("by_task poisoned");
        by_task
            .entry(task_id)
            .or_default()
            .push((name.to_string(), net_id));
    }
    Ok((lamport, true))
}

/// Local `service_forget` — convert an existing live entry to a
/// tombstone, or no-op if no such entry exists.
pub fn apply_local_forget(
    task_id: TaskId,
    name: &str,
    net_id: NetId,
) -> Result<(Lamport, bool), ServiceError> {
    validate_name(name)?;
    let lamport = bump_local_clock();
    let entry = ServiceEntry {
        net_id,
        lamport,
        tombstone: true,
        last_seen: Instant::now(),
    };
    let mut changed = false;
    {
        let mut entries = dir().entries.lock().expect("entries poisoned");
        if let Some(bucket) = entries.get_mut(name) {
            bucket.replace(entry);
            changed = true;
        }
    }
    // Drop the back-index entry the live declare added (best-effort).
    {
        let mut by_task = dir().by_task.lock().expect("by_task poisoned");
        if let Some(list) = by_task.get_mut(&task_id) {
            list.retain(|(n, id)| !(n == name && *id == net_id));
        }
    }
    Ok((lamport, changed))
}

/// Inbound ServiceAd. Returns true if the directory was updated.
pub fn apply_remote_ad(name: &str, net_id: NetId, lamport: Lamport) -> bool {
    if name.len() > MAX_SVC_NAME {
        return false;
    }
    observe_remote(lamport);
    let mut entries = dir().entries.lock().expect("entries poisoned");
    let bucket = entries.entry(name.to_string()).or_default();
    let existing = bucket
        .iter()
        .find(|e| e.net_id == net_id)
        .cloned();
    match existing {
        Some(e) if e.lamport >= lamport => false, // older or same — drop
        _ => {
            bucket.replace(ServiceEntry {
                net_id,
                lamport,
                tombstone: false,
                last_seen: Instant::now(),
            });
            true
        }
    }
}

/// Inbound ServiceForget. Returns true if the directory was updated
/// (existing entry replaced with a tombstone).
pub fn apply_remote_forget(name: &str, net_id: NetId, lamport: Lamport) -> bool {
    if name.len() > MAX_SVC_NAME {
        return false;
    }
    observe_remote(lamport);
    let mut entries = dir().entries.lock().expect("entries poisoned");
    let bucket = entries.entry(name.to_string()).or_default();
    let existing = bucket.iter().find(|e| e.net_id == net_id).cloned();
    match existing {
        Some(e) if e.lamport >= lamport => false,
        _ => {
            bucket.replace(ServiceEntry {
                net_id,
                lamport,
                tombstone: true,
                last_seen: Instant::now(),
            });
            true
        }
    }
}

/// Return all live (non-tombstoned) NetIds registered for `name`.
/// Returns an empty Vec if the name is unknown or every entry is a
/// tombstone.
pub fn service_lookup(name: &str) -> Vec<NetId> {
    let entries = dir().entries.lock().expect("entries poisoned");
    match entries.get(name) {
        None => Vec::new(),
        Some(bucket) => bucket
            .iter()
            .filter(|e| !e.tombstone)
            .map(|e| e.net_id)
            .collect(),
    }
}

/// Pick one live `NetId` for `name`, round-robining across endpoints.
/// `None` if no live entry exists. The cursor is per-name and survives
/// the call.
pub fn service_lookup_one(name: &str) -> Option<NetId> {
    let live = service_lookup(name);
    if live.is_empty() {
        return None;
    }
    let mut cursors = dir().cursors.lock().expect("cursors poisoned");
    let cursor = cursors
        .entry(name.to_string())
        .or_insert_with(|| AtomicUsize::new(0));
    let idx = cursor.fetch_add(1, Ordering::Relaxed) % live.len();
    Some(live[idx])
}

/// GC tombstones whose `last_seen + TOMBSTONE_TTL` is in the past.
/// Returns the number of entries removed. Callable from a background
/// timer or on demand (the gossip task triggers it).
pub fn gc_expired_tombstones() -> usize {
    let now = Instant::now();
    let mut entries = dir().entries.lock().expect("entries poisoned");
    let mut removed = 0;
    for (_name, bucket) in entries.iter_mut() {
        let prev_len = bucket.len();
        let to_keep: BTreeSet<ServiceEntry> = bucket
            .iter()
            .filter(|e| !e.tombstone || now.saturating_duration_since(e.last_seen) < TOMBSTONE_TTL)
            .cloned()
            .collect();
        removed += prev_len - to_keep.len();
        *bucket = to_keep;
    }
    removed
}

/// Snapshot every entry (live and tombstoned) for the on-connect
/// `ServiceSync` response builder. Returns a cloned `Vec` so callers
/// don't hold the directory lock while crossing crate boundaries.
pub fn snapshot() -> Vec<SnapshotEntry> {
    let entries = dir().entries.lock().expect("entries poisoned");
    let mut out = Vec::new();
    for (name, bucket) in entries.iter() {
        for e in bucket {
            out.push(SnapshotEntry {
                name: name.clone(),
                net_id: e.net_id,
                lamport: e.lamport,
                tombstone: e.tombstone,
            });
        }
    }
    out
}

/// One row from `snapshot()`. Distinct from the wire's `ServiceSyncEntry`
/// so we can evolve the snapshot shape without breaking the wire.
#[derive(Clone, Debug)]
pub struct SnapshotEntry {
    pub name: String,
    pub net_id: NetId,
    pub lamport: Lamport,
    pub tombstone: bool,
}

/// Test-only: clear directory state. Call between tests that touch the
/// process-singleton directory so they don't observe stale entries.
#[doc(hidden)]
pub fn test_clear() {
    dir().entries.lock().unwrap().clear();
    dir().by_task.lock().unwrap().clear();
    dir().cursors.lock().unwrap().clear();
    dir().local_clock.store(0, Ordering::Relaxed);
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

    /// Hold this guard for the duration of any test that touches the
    /// process-singleton directory. Without it, parallel cargo-test
    /// invocations race on the global state.
    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::mbox::TEST_MUTEX.lock().unwrap();
        test_clear();
        g
    }

    // Each test gets a fresh directory. To avoid global-state collisions
    // we serialize by running on current_thread; the parent harness can
    // still parallelise across tests via a Mutex if needed, but for the
    // scope of CRDT semantics tests, sequential is fine.

    #[test]
    fn declare_inserts_live_entry() {
        let _g = fresh();
        let (l, ok) = apply_local_declare(1, "registry", netid(1, 5)).unwrap();
        assert!(ok);
        assert_eq!(l, 1);
        assert_eq!(service_lookup("registry"), vec![netid(1, 5)]);
    }

    #[test]
    fn name_too_long_rejected() {
        let _g = fresh();
        let long = "x".repeat(MAX_SVC_NAME + 1);
        let err = apply_local_declare(1, &long, netid(1, 1)).unwrap_err();
        assert!(matches!(err, ServiceError::NameTooLong(_)));
    }

    #[test]
    fn forget_tombstones_existing_entry() {
        let _g = fresh();
        apply_local_declare(1, "svc", netid(1, 1)).unwrap();
        assert_eq!(service_lookup("svc"), vec![netid(1, 1)]);
        let (_, ok) = apply_local_forget(1, "svc", netid(1, 1)).unwrap();
        assert!(ok);
        assert!(service_lookup("svc").is_empty());
    }

    #[test]
    fn remote_ad_with_older_lamport_dropped() {
        let _g = fresh();
        assert!(apply_remote_ad("svc", netid(1, 1), 10));
        assert!(!apply_remote_ad("svc", netid(1, 1), 5));
        // Same lamport — also dropped.
        assert!(!apply_remote_ad("svc", netid(1, 1), 10));
        // Greater lamport — accepted.
        assert!(apply_remote_ad("svc", netid(1, 1), 11));
    }

    #[test]
    fn distinct_net_ids_coexist_under_same_name() {
        let _g = fresh();
        apply_local_declare(1, "svc", netid(1, 1)).unwrap();
        apply_local_declare(2, "svc", netid(2, 1)).unwrap();
        let mut live = service_lookup("svc");
        live.sort_by(|a, b| a.node.0.cmp(&b.node.0));
        assert_eq!(live, vec![netid(1, 1), netid(2, 1)]);
    }

    #[test]
    fn tie_breaking_is_deterministic_by_net_id() {
        let _g = fresh();
        // Two entries with the SAME lamport but different net_ids — they
        // coexist, but BTreeSet ordering is deterministic.
        apply_remote_ad("svc", netid(5, 1), 7);
        apply_remote_ad("svc", netid(1, 1), 7);
        apply_remote_ad("svc", netid(3, 1), 7);
        let live = service_lookup("svc");
        // service_lookup iterates the BTreeSet in net_id-ascending
        // order — so we expect 1, 3, 5.
        assert_eq!(live, vec![netid(1, 1), netid(3, 1), netid(5, 1)]);
    }

    #[test]
    fn round_robin_one_picks_each_endpoint_in_turn() {
        let _g = fresh();
        apply_local_declare(1, "svc", netid(1, 1)).unwrap();
        apply_local_declare(2, "svc", netid(2, 1)).unwrap();
        apply_local_declare(3, "svc", netid(3, 1)).unwrap();
        let mut counts = std::collections::HashMap::new();
        for _ in 0..30 {
            let id = service_lookup_one("svc").unwrap();
            *counts.entry(id).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 3);
        // Each endpoint hit exactly 10 times since cursor is per-name
        // and we did 30 round-robin steps over 3 endpoints.
        for c in counts.values() {
            assert_eq!(*c, 10);
        }
    }

    #[test]
    fn lookup_one_returns_none_when_only_tombstones() {
        let _g = fresh();
        apply_remote_ad("svc", netid(1, 1), 1);
        apply_remote_forget("svc", netid(1, 1), 2);
        assert!(service_lookup_one("svc").is_none());
    }

    #[test]
    fn tombstone_hides_then_later_ad_revives() {
        let _g = fresh();
        apply_local_declare(1, "svc", netid(1, 1)).unwrap();
        apply_local_forget(1, "svc", netid(1, 1)).unwrap();
        assert!(service_lookup("svc").is_empty());
        // A strictly-later ad re-publishes.
        apply_remote_ad("svc", netid(1, 1), bump_local_clock() + 100);
        assert_eq!(service_lookup("svc"), vec![netid(1, 1)]);
    }

    #[test]
    fn observe_remote_advances_local_clock() {
        let _g = fresh();
        apply_remote_ad("svc", netid(1, 1), 1000);
        let next = bump_local_clock();
        assert!(next > 1000, "next={} should exceed 1000", next);
    }

    #[test]
    fn lookup_returns_empty_for_unknown_name() {
        let _g = fresh();
        assert!(service_lookup("nope").is_empty());
        assert!(service_lookup_one("nope").is_none());
    }
}
