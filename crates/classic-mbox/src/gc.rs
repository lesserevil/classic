//! Task lifecycle GC. classic-spawn (plan 04) calls `on_task_start` /
//! `on_task_exit`; on exit we synthesize a `service_forget` for every
//! service the task declared and evict every mailbox it owned.
//!
//! The mailbox-owner back-index is populated by `Mailbox::new` when a
//! task context is active (set via `set_current_task`). Without an
//! active task context, mailboxes aren't tracked by task (they still
//! work normally — they just won't be reaped on `on_task_exit`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use classic_proto::MboxId;

use crate::directory::TaskId;

struct Tracker {
    mboxes: Mutex<HashMap<TaskId, Vec<MboxId>>>,
}

impl Tracker {
    fn new() -> Self {
        Self { mboxes: Mutex::new(HashMap::new()) }
    }
}

static TRACKER: OnceLock<Tracker> = OnceLock::new();

fn tracker() -> &'static Tracker {
    TRACKER.get_or_init(Tracker::new)
}

/// Register a task's existence. Idempotent — re-calling on an already-
/// tracked task is a no-op.
pub fn on_task_start(task_id: TaskId) {
    tracker()
        .mboxes
        .lock()
        .expect("gc tracker poisoned")
        .entry(task_id)
        .or_default();
}

/// Reap everything owned by `task_id`. Idempotent — subsequent calls
/// see an empty entry and emit nothing.
pub fn on_task_exit(task_id: TaskId) {
    // Step 1: synthesize service_forget for every declared service of
    // this task. We pull the names from the directory's gossip-side
    // registry (DECLARED in gossip.rs); since that's also process-local,
    // walk it under its own mutex.
    let names = crate::gossip::drain_declared_for_task(task_id);
    for (name, net_id, _lamport) in names {
        if let Ok((lamport, _)) = crate::directory::apply_local_forget(task_id, &name, net_id) {
            let frame = crate::frames::ServiceForget {
                name: name.clone(),
                net_id,
                lamport,
            };
            if let Ok(encoded) = crate::frames::encode_service_forget(&frame) {
                crate::gossip_emit::emit(encoded);
                crate::gossip::GOSSIP_OUT
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    // Step 2: evict every mailbox the task owned.
    let mboxes = {
        tracker()
            .mboxes
            .lock()
            .expect("gc tracker poisoned")
            .remove(&task_id)
            .unwrap_or_default()
    };
    for id in mboxes {
        crate::mbox::evict_for_gc(id);
    }
}

/// Mailbox::new calls this (when a task context is active) to record
/// the new MboxId for the owning task. Returns the count of mailboxes
/// the task now owns; used by tests to assert tracking.
pub(crate) fn record_owned_mbox(task_id: TaskId, mbox: MboxId) {
    tracker()
        .mboxes
        .lock()
        .expect("gc tracker poisoned")
        .entry(task_id)
        .or_default()
        .push(mbox);
}

#[cfg(test)]
pub(crate) fn test_clear() {
    tracker().mboxes.lock().unwrap().clear();
}

#[cfg(test)]
pub(crate) fn owned_count(task_id: TaskId) -> usize {
    tracker()
        .mboxes
        .lock()
        .unwrap()
        .get(&task_id)
        .map(|v| v.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::test_clear as dir_clear;
    use crate::gossip::{clear_current_task, service_declare, set_current_task};
    use crate::mbox::{Mailbox, MBOX_CAPACITY};
    use crate::send;
    use classic_proto::{MboxId, NodeId};

    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::mbox::TEST_MUTEX.lock().unwrap();
        dir_clear();
        test_clear();
        crate::gossip_emit::clear_sink();
        clear_current_task();
        g
    }

    #[test]
    fn on_task_exit_forgets_declared_services_and_evicts_mboxes() {
        let _g = fresh();
        send::init(NodeId([1; 16]));
        set_current_task(7, MboxId(42));
        on_task_start(7);

        // Allocate two mailboxes the task owns.
        let (m1, _r1) = Mailbox::new();
        let (m2, _r2) = Mailbox::new();
        // Track them — production wires this from Mailbox::new when
        // there's an active task; tests record explicitly here so the
        // test doesn't depend on that wiring.
        record_owned_mbox(7, m1);
        record_owned_mbox(7, m2);
        assert_eq!(owned_count(7), 2);

        // Declare two services.
        let _h1 = service_declare("alpha").unwrap();
        let _h2 = service_declare("beta").unwrap();
        assert_eq!(crate::directory::service_lookup("alpha").len(), 1);
        assert_eq!(crate::directory::service_lookup("beta").len(), 1);

        // Hold ServiceHandles past on_task_exit — the exit path should
        // forget independently, so ServiceHandle::Drop becoming a
        // tombstone-second-time is OK (idempotent forget).
        on_task_exit(7);

        assert!(crate::directory::service_lookup("alpha").is_empty());
        assert!(crate::directory::service_lookup("beta").is_empty());
        assert_eq!(owned_count(7), 0);
    }

    #[test]
    fn on_task_exit_idempotent() {
        let _g = fresh();
        send::init(NodeId([1; 16]));
        on_task_start(11);
        on_task_exit(11);
        on_task_exit(11);
        on_task_exit(11);
        // No panic, no negative counts.
    }

    #[test]
    fn service_handle_drop_then_task_exit_is_safe() {
        let _g = fresh();
        send::init(NodeId([1; 16]));
        set_current_task(12, MboxId(1));
        on_task_start(12);
        {
            let _h = service_declare("svc-once").unwrap();
        } // Drop forgets here.
        assert!(crate::directory::service_lookup("svc-once").is_empty());
        on_task_exit(12); // must not panic, must not double-forget noisily
    }

    #[test]
    fn mbox_evicted_after_task_exit() {
        let _g = fresh();
        send::init(NodeId([1; 16]));
        on_task_start(20);
        let (id, _r) = Mailbox::new();
        record_owned_mbox(20, id);
        assert!(crate::mbox::lookup(id).is_some());
        on_task_exit(20);
        // Force the original receiver to drop too (it'd also evict on
        // Drop, but on_task_exit is supposed to be the authoritative
        // teardown path).
        assert!(crate::mbox::lookup(id).is_none());
    }

    #[test]
    fn mbox_capacity_constant_is_exposed() {
        // Sanity: just so changing the constant fails this test loudly.
        assert_eq!(MBOX_CAPACITY, 1024);
    }
}
