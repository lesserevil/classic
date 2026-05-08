//! `AdStore` — central read-mostly map of `NodeAd`s, plus a lossy buffered
//! `watch()` stream of `AdUpdate` events.
//!
//! Conflict resolution is last-writer-wins on the `(generation, boot_time)`
//! tuple — `generation` is the primary key, `boot_time` breaks ties so a
//! daemon restart cannot reset its ad to an older state.
//!
//! Single-writer-per-key: each peer's slot is only written by Gossip RX, the
//! own-node slot only by Discovery. The store does not enforce that
//! discipline; it just makes LWW correct given the discipline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::Stream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use classic_proto::NodeId;

use crate::schema::{AdUpdate, NodeAd};

/// Default broadcast channel depth for `watch()`. Lossy beyond this — old
/// events drop. Sized to absorb a burst (~256 nodes × a couple of events
/// each) without blocking writers.
const WATCH_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct AdStore {
    inner: Arc<Inner>,
}

struct Inner {
    self_id: NodeId,
    self_ad: RwLock<NodeAd>,
    peers: RwLock<HashMap<NodeId, NodeAd>>,
    /// Outstanding eviction timers keyed by peer NodeId. Aborted when a
    /// fresh ad arrives for that peer.
    eviction_handles: Mutex<HashMap<NodeId, JoinHandle<()>>>,
    events_tx: broadcast::Sender<AdUpdate>,
}

impl AdStore {
    pub fn new(self_ad: NodeAd) -> Self {
        let (events_tx, _) = broadcast::channel(WATCH_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                self_id: self_ad.node_id,
                self_ad: RwLock::new(self_ad),
                peers: RwLock::new(HashMap::new()),
                eviction_handles: Mutex::new(HashMap::new()),
                events_tx,
            }),
        }
    }

    pub fn self_ad(&self) -> NodeAd {
        self.inner
            .self_ad
            .read()
            .expect("self_ad poisoned")
            .clone()
    }

    /// Replace the own-node ad. Emits `Updated` only when the generation
    /// actually changes — re-publishing the same generation is a no-op for
    /// watchers (FR-10).
    pub fn update_self(&self, ad: NodeAd) {
        assert_eq!(
            ad.node_id, self.inner.self_id,
            "update_self must be called with the daemon's own NodeId"
        );
        let mut slot = self.inner.self_ad.write().expect("self_ad poisoned");
        let prev_gen = slot.generation;
        if prev_gen == ad.generation && slot.boot_time == ad.boot_time {
            *slot = ad;
            return;
        }
        *slot = ad.clone();
        drop(slot);
        let _ = self.inner.events_tx.send(AdUpdate::Updated(ad));
    }

    /// Insert / update a peer ad. Last-writer-wins on `(generation,
    /// boot_time)` — older entries are silently dropped. Cancels any
    /// pending eviction timer for this peer.
    pub fn upsert(&self, ad: NodeAd) -> UpsertOutcome {
        if ad.node_id == self.inner.self_id {
            return UpsertOutcome::IgnoredSelf;
        }
        let outcome = {
            let mut peers = self.inner.peers.write().expect("peers poisoned");
            match peers.get(&ad.node_id) {
                None => {
                    peers.insert(ad.node_id, ad.clone());
                    UpsertOutcome::Inserted
                }
                Some(existing) => {
                    let existing_key = (existing.generation, existing.boot_time);
                    let new_key = (ad.generation, ad.boot_time);
                    if new_key > existing_key {
                        peers.insert(ad.node_id, ad.clone());
                        UpsertOutcome::Updated
                    } else if new_key == existing_key {
                        // Same generation + boot_time — refresh stored copy
                        // in case body fields (load) shifted, but do not
                        // emit an event (FR-10).
                        peers.insert(ad.node_id, ad.clone());
                        UpsertOutcome::Unchanged
                    } else {
                        UpsertOutcome::Stale
                    }
                }
            }
        };
        // Cancel any pending eviction; the peer is alive again.
        self.cancel_eviction(ad.node_id);
        match outcome {
            UpsertOutcome::Inserted => {
                let _ = self.inner.events_tx.send(AdUpdate::Inserted(ad));
            }
            UpsertOutcome::Updated => {
                let _ = self.inner.events_tx.send(AdUpdate::Updated(ad));
            }
            UpsertOutcome::Unchanged | UpsertOutcome::Stale | UpsertOutcome::IgnoredSelf => {}
        }
        outcome
    }

    pub fn peer(&self, id: NodeId) -> Option<NodeAd> {
        self.inner
            .peers
            .read()
            .expect("peers poisoned")
            .get(&id)
            .cloned()
    }

    /// Returns own ad first, then peer ads sorted by NodeId bytes.
    pub fn all_ads(&self) -> Vec<NodeAd> {
        let mut out = Vec::with_capacity(1 + self.peer_count());
        out.push(self.self_ad());
        let peers = self.inner.peers.read().expect("peers poisoned");
        let mut ids: Vec<NodeId> = peers.keys().copied().collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        for id in ids {
            if let Some(ad) = peers.get(&id) {
                out.push(ad.clone());
            }
        }
        out
    }

    pub fn peer_count(&self) -> usize {
        self.inner.peers.read().expect("peers poisoned").len()
    }

    /// Schedule eviction of `peer` after `ttl`. If a fresh ad arrives for
    /// the peer before `ttl` elapses, the timer is cancelled. Replaces any
    /// outstanding timer for the same peer.
    pub fn mark_stale(&self, peer: NodeId, ttl: Duration) {
        if peer == self.inner.self_id {
            return; // never evict own slot
        }
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            do_evict(&inner, peer);
        });
        let mut handles = self.inner.eviction_handles.lock().expect("eviction lock poisoned");
        if let Some(prev) = handles.insert(peer, handle) {
            prev.abort();
        }
    }

    /// Immediate eviction. Idempotent.
    pub fn evict(&self, peer: NodeId) {
        self.cancel_eviction(peer);
        do_evict(&self.inner, peer);
    }

    fn cancel_eviction(&self, peer: NodeId) {
        if let Some(h) = self
            .inner
            .eviction_handles
            .lock()
            .expect("eviction lock poisoned")
            .remove(&peer)
        {
            h.abort();
        }
    }

    /// Subscribe to ad updates. Lossy — slow subscribers see the
    /// most-recent `WATCH_CAPACITY` events; older events drop without
    /// blocking writers.
    pub fn watch(&self) -> impl Stream<Item = AdUpdate> + Send + 'static {
        let rx = self.inner.events_tx.subscribe();
        BroadcastStream::new(rx).filter_map(|res| res.ok())
    }
}

/// Result of a single `upsert` call. Tests inspect this; in production it's
/// also useful for the gossip RX path to log "stale ad ignored" without
/// double-fetching the existing entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
    /// Same `(generation, boot_time)` as existing — refreshed quietly.
    Unchanged,
    /// New ad has older `(generation, boot_time)` than existing — discarded.
    Stale,
    /// Caller tried to upsert into the own-node slot via `upsert` (must use
    /// `update_self` instead).
    IgnoredSelf,
}

fn do_evict(inner: &Arc<Inner>, peer: NodeId) {
    let removed = inner
        .peers
        .write()
        .expect("peers poisoned")
        .remove(&peer)
        .is_some();
    if removed {
        let _ = inner.events_tx.send(AdUpdate::Removed(peer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{CpuInfo, LoadSample, MemInfo};

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    fn ad(node: NodeId, gen: u64, boot: u64) -> NodeAd {
        NodeAd {
            node_id: node,
            hostname: "h".into(),
            proto_version: 1,
            generation: gen,
            boot_time: boot,
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

    #[tokio::test]
    async fn lww_higher_generation_wins() {
        let store = AdStore::new(ad(id(0), 1, 0));
        assert_eq!(store.upsert(ad(id(1), 5, 100)), UpsertOutcome::Inserted);
        assert_eq!(store.upsert(ad(id(1), 3, 100)), UpsertOutcome::Stale);
        assert_eq!(store.upsert(ad(id(1), 2, 100)), UpsertOutcome::Stale);
        assert_eq!(store.peer(id(1)).unwrap().generation, 5);
    }

    #[tokio::test]
    async fn lww_breaks_tie_on_boot_time() {
        let store = AdStore::new(ad(id(0), 1, 0));
        assert_eq!(store.upsert(ad(id(1), 5, 100)), UpsertOutcome::Inserted);
        // Same generation, newer boot_time — accepted (restart case).
        assert_eq!(store.upsert(ad(id(1), 5, 101)), UpsertOutcome::Updated);
        // Same generation + boot_time — quietly refreshes, no event.
        assert_eq!(store.upsert(ad(id(1), 5, 101)), UpsertOutcome::Unchanged);
        assert_eq!(store.peer(id(1)).unwrap().boot_time, 101);
    }

    #[tokio::test]
    async fn upsert_into_self_slot_ignored() {
        let me = id(0);
        let store = AdStore::new(ad(me, 1, 0));
        assert_eq!(store.upsert(ad(me, 99, 99)), UpsertOutcome::IgnoredSelf);
        assert_eq!(store.self_ad().generation, 1);
    }

    #[tokio::test]
    async fn watch_emits_inserted_then_updated_but_not_unchanged() {
        let store = AdStore::new(ad(id(0), 1, 0));
        let mut rx = store.watch();
        store.upsert(ad(id(1), 1, 0));
        store.upsert(ad(id(1), 1, 0)); // unchanged
        store.upsert(ad(id(1), 2, 0)); // updated
        store.upsert(ad(id(1), 1, 0)); // stale, no event

        let mut events = Vec::new();
        let collect = async {
            while let Some(evt) = rx.next().await {
                events.push(evt);
                if events.len() == 2 {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(1), collect)
            .await
            .expect("watch did not emit two events in time");
        assert!(matches!(events[0], AdUpdate::Inserted(_)));
        match &events[1] {
            AdUpdate::Updated(a) => assert_eq!(a.generation, 2),
            other => panic!("expected Updated(gen=2), got {other:?}"),
        }
    }

    /// Uses real time with a short TTL. Paused-time orchestration of a
    /// spawned task adds a flake where the eviction may not be polled
    /// before the assertion — real time keeps the test deterministic in
    /// exchange for a few extra ms per run.
    #[tokio::test(flavor = "multi_thread")]
    async fn mark_stale_evicts_after_ttl() {
        let store = AdStore::new(ad(id(0), 1, 0));
        store.upsert(ad(id(1), 1, 0));
        let mut rx = store.watch();

        store.mark_stale(id(1), Duration::from_millis(50));
        assert!(store.peer(id(1)).is_some());

        let evt = tokio::time::timeout(Duration::from_millis(500), rx.next())
            .await
            .expect("watch did not emit Removed in time")
            .expect("watch closed");
        assert_eq!(evt, AdUpdate::Removed(id(1)));
        assert!(store.peer(id(1)).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_ad_cancels_eviction_timer() {
        let store = AdStore::new(ad(id(0), 1, 0));
        store.upsert(ad(id(1), 1, 0));
        store.mark_stale(id(1), Duration::from_millis(200));
        // Fresh ad arrives well before TTL.
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.upsert(ad(id(1), 2, 0));
        // Past the original TTL — peer must still be present.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(store.peer(id(1)).is_some());
    }

    #[tokio::test]
    async fn all_ads_returns_self_first_then_peers_sorted() {
        let store = AdStore::new(ad(id(5), 1, 0));
        store.upsert(ad(id(9), 1, 0));
        store.upsert(ad(id(1), 1, 0));
        store.upsert(ad(id(7), 1, 0));
        let all = store.all_ads();
        assert_eq!(all[0].node_id, id(5)); // self first
        assert_eq!(all[1].node_id, id(1));
        assert_eq!(all[2].node_id, id(7));
        assert_eq!(all[3].node_id, id(9));
    }
}
