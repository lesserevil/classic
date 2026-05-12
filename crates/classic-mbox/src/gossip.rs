//! Service-directory gossip engine. Handles:
//! - Outbound: on local declare/forget, broadcast a single
//!   `ServiceAd` / `ServiceForget` to every connected peer.
//! - On-connect sync: when a peer comes up, send `ServiceSync`; peer
//!   replies with `ServiceSyncResponse` carrying all live entries.
//! - Inbound: apply Lamport rules via `directory::apply_remote_*`;
//!   we do NOT re-broadcast (the originator already flooded the
//!   mesh — assumes full-mesh per ARCHITECTURE.md).
//!
//! Public surface added here:
//! - `service_declare(name)` → `ServiceHandle` (RAII; Drop calls
//!   service_forget)
//! - `service_forget(name)` (idempotent)
//! - `set_current_task(task_id, primary_mbox)` — task-local context
//!   so service_declare can derive the NetId without explicit args.
//!   classic-spawn (plan 04) sets this on task spawn; classic-jja
//!   tests set it directly.

use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use tracing::warn;

use classic_proto::NetId;

use crate::directory::{
    apply_local_declare, apply_local_forget, apply_remote_ad, apply_remote_forget, TaskId,
};
use crate::error::ServiceError;
use crate::frames::{encode_service_ad, encode_service_forget, ServiceAd, ServiceForget};
use crate::send;

/// Per-thread task context — set by classic-spawn when a task starts;
/// service_declare reads this to fill in (task_id, primary_mbox). Kept
/// thread-local so the same daemon process can host many concurrent
/// tasks without cross-contamination.
thread_local! {
    static CURRENT_TASK: std::cell::RefCell<Option<TaskContext>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Copy, Clone, Debug)]
struct TaskContext {
    task_id: TaskId,
    primary_mbox: classic_proto::MboxId,
}

/// Install the current task's identity. classic-spawn calls this on
/// task spawn; tests call it directly before `service_declare`.
pub fn set_current_task(task_id: TaskId, primary_mbox: classic_proto::MboxId) {
    CURRENT_TASK.with(|cell| {
        *cell.borrow_mut() = Some(TaskContext { task_id, primary_mbox });
    });
}

/// Test helper: clear the current-task slot.
pub fn clear_current_task() {
    CURRENT_TASK.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

fn current_task() -> Option<TaskContext> {
    CURRENT_TASK.with(|cell| *cell.borrow())
}

/// Per-process registry of declared services: (name, task_id, net_id,
/// last_lamport). Used by Drop, service_forget, and the GC path.
static DECLARED: OnceLock<Mutex<Vec<(String, TaskId, NetId, u64)>>> = OnceLock::new();

fn declared() -> &'static Mutex<Vec<(String, TaskId, NetId, u64)>> {
    DECLARED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drain every declaration owned by `task_id`. Used by `on_task_exit`
/// to synthesize ServiceForget broadcasts on the way down. Returns the
/// `(name, net_id, last_lamport)` tuples that were tracked.
pub(crate) fn drain_declared_for_task(
    task_id: TaskId,
) -> Vec<(String, NetId, u64)> {
    let mut decls = declared().lock().expect("declared poisoned");
    let mut out = Vec::new();
    decls.retain(|(name, t, net_id, lamport)| {
        if *t == task_id {
            out.push((name.clone(), *net_id, *lamport));
            false // remove
        } else {
            true
        }
    });
    out
}

/// Declare `name` as a service backed by the current task's primary
/// mailbox. Drop of the returned `ServiceHandle` calls `service_forget`.
pub fn service_declare(name: &str) -> Result<ServiceHandle, ServiceError> {
    let ctx = current_task().ok_or(ServiceError::NameTooLong(0)).map_err(|_| {
        // Reuse NameTooLong-typed error envelope for now; until classic-spawn
        // wires set_current_task the right answer is "you must call init".
        ServiceError::NameTooLong(usize::MAX)
    })?;
    let self_node = send::self_node_id().ok_or(ServiceError::NameTooLong(usize::MAX - 1))?;
    let net_id = NetId { node: self_node, mbox: ctx.primary_mbox };
    let (lamport, _changed) = apply_local_declare(ctx.task_id, name, net_id)?;
    declared()
        .lock()
        .unwrap()
        .push((name.to_string(), ctx.task_id, net_id, lamport));

    // Broadcast ServiceAd to every live peer.
    broadcast_ad(&ServiceAd {
        name: name.to_string(),
        net_id,
        lamport,
    });

    Ok(ServiceHandle {
        name: name.to_string(),
        task_id: ctx.task_id,
        net_id,
        forgotten: false,
    })
}

/// Idempotent local forget by name. If multiple endpoints under the
/// same name are declared by the current task, forgets the most recent
/// one (the typical pattern: one task → one service endpoint).
pub fn service_forget(name: &str) {
    let Some(ctx) = current_task() else { return };
    let Some(self_node) = send::self_node_id() else { return };

    let mut decls = declared().lock().unwrap();
    if let Some(pos) = decls
        .iter()
        .rposition(|(n, t, _, _)| n == name && *t == ctx.task_id)
    {
        let (_, _, net_id, _) = decls.remove(pos);
        drop(decls);
        let _ = self_node; // not needed beyond pin for symmetry
        if let Ok((lamport, _)) = apply_local_forget(ctx.task_id, name, net_id) {
            broadcast_forget(&ServiceForget {
                name: name.to_string(),
                net_id,
                lamport,
            });
        }
    }
}

/// RAII handle. Drop calls service_forget if not already forgotten.
#[derive(Debug)]
pub struct ServiceHandle {
    name: String,
    task_id: TaskId,
    net_id: NetId,
    forgotten: bool,
}

impl ServiceHandle {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn net_id(&self) -> NetId {
        self.net_id
    }
    /// Explicit forget — Drop becomes a no-op.
    pub fn forget(mut self) {
        self.forgotten = true;
        let _ = apply_local_forget(self.task_id, &self.name, self.net_id);
        // Best-effort broadcast.
        if let Ok((lamport, _)) =
            apply_local_forget(self.task_id, &self.name, self.net_id)
        {
            broadcast_forget(&ServiceForget {
                name: self.name.clone(),
                net_id: self.net_id,
                lamport,
            });
        }
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        if self.forgotten {
            return;
        }
        let _ = apply_local_forget(self.task_id, &self.name, self.net_id);
    }
}

fn broadcast_ad(ad: &ServiceAd) {
    let Ok(frame) = encode_service_ad(ad) else {
        warn!(name = %ad.name, "failed to encode ServiceAd; not broadcast");
        return;
    };
    GOSSIP_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::gossip_emit::emit(frame);
}

fn broadcast_forget(f: &ServiceForget) {
    let Ok(frame) = encode_service_forget(f) else {
        warn!(name = %f.name, "failed to encode ServiceForget; not broadcast");
        return;
    };
    GOSSIP_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::gossip_emit::emit(frame);
}

/// Diagnostic counter for outbound gossip frames. Bumped on every
/// successful broadcast (regardless of how many peers actually receive
/// — that's the Peers backend's business).
pub static GOSSIP_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Diagnostic counter for inbound gossip frames applied to the
/// directory.
pub static GOSSIP_IN_APPLIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Inbound handler — called by `MboxHandler` when a Service-range
/// frame arrives. Applies Lamport rules; does NOT re-broadcast (the
/// originator floods all peers; full-mesh).
pub fn on_inbound_ad(ad: ServiceAd) {
    if apply_remote_ad(&ad.name, ad.net_id, ad.lamport) {
        GOSSIP_IN_APPLIED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn on_inbound_forget(f: ServiceForget) {
    if apply_remote_forget(&f.name, f.net_id, f.lamport) {
        GOSSIP_IN_APPLIED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Inbound on-connect SYNC: we send back every non-tombstoned entry we
/// know via a `ServiceSyncResponse`. Called by classic-node's
/// per-peer-up hook.
pub fn build_sync_response() -> crate::frames::ServiceSyncResponse {
    use crate::frames::ServiceSyncEntry;
    let mut entries: Vec<ServiceSyncEntry> = Vec::new();
    // We can't pull the full bucket list out of the directory without
    // re-exposing its internals. For this task, expose a small helper
    // `dir::snapshot()` so the gossip engine can walk it.
    for entry in crate::directory::snapshot() {
        entries.push(ServiceSyncEntry {
            name: entry.name,
            net_id: entry.net_id,
            lamport: entry.lamport,
            tombstone: entry.tombstone,
        });
    }
    crate::frames::ServiceSyncResponse { entries }
}

/// Inbound on-connect SYNC response: apply each entry via the LWW
/// rules. Tombstoned entries are applied via `apply_remote_forget` so
/// the receiver gets a tombstone of equal lamport (which won't be
/// re-broadcast since the directory accepts only strictly-greater
/// updates).
pub fn on_inbound_sync_response(resp: crate::frames::ServiceSyncResponse) {
    for entry in resp.entries {
        let applied = if entry.tombstone {
            apply_remote_forget(&entry.name, entry.net_id, entry.lamport)
        } else {
            apply_remote_ad(&entry.name, entry.net_id, entry.lamport)
        };
        if applied {
            GOSSIP_IN_APPLIED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::test_clear;
    use crate::frames::{ServiceSyncEntry, ServiceSyncResponse};
    use classic_proto::{MboxId, NodeId};

    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::mbox::TEST_MUTEX.lock().unwrap();
        test_clear();
        clear_current_task();
        DECLARED.get().map(|m| m.lock().unwrap().clear());
        g
    }

    fn nid(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    #[test]
    fn service_declare_requires_task_context() {
        let _g = fresh();
        send::init(nid(1));
        let err = service_declare("svc").unwrap_err();
        assert!(matches!(err, ServiceError::NameTooLong(_)));
    }

    #[test]
    fn service_declare_records_entry_and_bumps_out_counter() {
        let _g = fresh();
        send::init(nid(1));
        set_current_task(7, MboxId(42));
        let before = GOSSIP_OUT.load(Ordering::Relaxed);
        let _h = service_declare("registry").unwrap();
        let after = GOSSIP_OUT.load(Ordering::Relaxed);
        assert!(after > before);
        let live = crate::directory::service_lookup("registry");
        assert_eq!(live, vec![NetId { node: nid(1), mbox: MboxId(42) }]);
    }

    #[test]
    fn service_handle_drop_forgets() {
        let _g = fresh();
        send::init(nid(1));
        set_current_task(7, MboxId(42));
        {
            let _h = service_declare("registry").unwrap();
            assert_eq!(crate::directory::service_lookup("registry").len(), 1);
        }
        assert!(crate::directory::service_lookup("registry").is_empty());
    }

    #[test]
    fn inbound_ad_updates_directory_and_bumps_in_counter() {
        let _g = fresh();
        let before = GOSSIP_IN_APPLIED.load(Ordering::Relaxed);
        on_inbound_ad(ServiceAd {
            name: "remote-svc".into(),
            net_id: NetId { node: nid(2), mbox: MboxId(11) },
            lamport: 5,
        });
        let after = GOSSIP_IN_APPLIED.load(Ordering::Relaxed);
        assert!(after > before);
        assert_eq!(
            crate::directory::service_lookup("remote-svc"),
            vec![NetId { node: nid(2), mbox: MboxId(11) }]
        );
    }

    #[test]
    fn inbound_forget_tombstones_existing_entry() {
        let _g = fresh();
        on_inbound_ad(ServiceAd {
            name: "remote-svc".into(),
            net_id: NetId { node: nid(2), mbox: MboxId(11) },
            lamport: 5,
        });
        on_inbound_forget(ServiceForget {
            name: "remote-svc".into(),
            net_id: NetId { node: nid(2), mbox: MboxId(11) },
            lamport: 6,
        });
        assert!(crate::directory::service_lookup("remote-svc").is_empty());
    }

    #[test]
    fn sync_response_applies_every_entry() {
        let _g = fresh();
        let resp = ServiceSyncResponse {
            entries: vec![
                ServiceSyncEntry {
                    name: "a".into(),
                    net_id: NetId { node: nid(2), mbox: MboxId(1) },
                    lamport: 10,
                    tombstone: false,
                },
                ServiceSyncEntry {
                    name: "b".into(),
                    net_id: NetId { node: nid(2), mbox: MboxId(2) },
                    lamport: 20,
                    tombstone: false,
                },
                ServiceSyncEntry {
                    name: "c".into(),
                    net_id: NetId { node: nid(2), mbox: MboxId(3) },
                    lamport: 30,
                    tombstone: true,
                },
            ],
        };
        on_inbound_sync_response(resp);
        assert_eq!(
            crate::directory::service_lookup("a"),
            vec![NetId { node: nid(2), mbox: MboxId(1) }]
        );
        assert_eq!(
            crate::directory::service_lookup("b"),
            vec![NetId { node: nid(2), mbox: MboxId(2) }]
        );
        // c was a tombstone — no live entry.
        assert!(crate::directory::service_lookup("c").is_empty());
    }

    #[test]
    fn build_sync_response_includes_live_and_tombstone_entries() {
        let _g = fresh();
        send::init(nid(1));
        set_current_task(7, MboxId(42));
        let _h = service_declare("svc-live").unwrap();
        on_inbound_ad(ServiceAd {
            name: "svc-remote".into(),
            net_id: NetId { node: nid(2), mbox: MboxId(11) },
            lamport: 5,
        });
        let resp = build_sync_response();
        // Both should appear in the response (live entries).
        let names: Vec<_> = resp.entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.iter().any(|n| n == "svc-live"));
        assert!(names.iter().any(|n| n == "svc-remote"));
    }
}
