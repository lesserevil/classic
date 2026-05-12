//! Local mailbox primitives. `Mailbox::new()` returns a fresh `MboxId`
//! and the receive half; the send side dispatches through the registry
//! by id. RAII: `MailboxRecv`'s `Drop` evicts its registry slot.
//!
//! The registry is a process-singleton — one daemon, one mailbox table.
//! Plan-05 Task 2 (`classic-hlt`) adds the `mail_send` dispatch that
//! looks up the sender on this same registry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;
use tracing::debug;

use classic_proto::MboxId;

/// Bounded capacity on every per-mailbox channel. Per plan-05: 1024.
/// `mail_send` is fire-and-forget — full mailboxes drop silently rather
/// than block the sender.
pub const MBOX_CAPACITY: usize = 1024;

type Slot = mpsc::Sender<Vec<u8>>;

struct Registry {
    next_id: AtomicU64,
    slots: Mutex<HashMap<MboxId, Slot>>,
}

impl Registry {
    fn new() -> Self {
        Self {
            // MboxId 0 is reserved per ARCHITECTURE.md §"Identity types"
            // for the per-node kernel/control mailbox; start at 1.
            next_id: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
        }
    }
    fn alloc(&self, sender: Slot) -> MboxId {
        let id = MboxId(self.next_id.fetch_add(1, Ordering::Relaxed));
        debug_assert!(id.0 != 0, "MboxId 0 must remain reserved");
        self.slots
            .lock()
            .expect("mbox registry poisoned")
            .insert(id, sender);
        id
    }
    fn evict(&self, id: MboxId) {
        self.slots
            .lock()
            .expect("mbox registry poisoned")
            .remove(&id);
    }
    fn sender(&self, id: MboxId) -> Option<Slot> {
        self.slots
            .lock()
            .expect("mbox registry poisoned")
            .get(&id)
            .cloned()
    }
    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.slots.lock().expect("mbox registry poisoned").len()
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}

/// Look up the send side of `mbox`. `None` if the receiver has been
/// dropped (and therefore the slot evicted). Used by the in-process
/// delivery path in plan-05 Task 2.
pub fn lookup(mbox: MboxId) -> Option<mpsc::Sender<Vec<u8>>> {
    registry().sender(mbox)
}

/// Force-evict `mbox` from the registry without consulting the
/// `MailboxRecv`'s Drop. Used by `on_task_exit` for authoritative
/// teardown — any still-live MailboxRecv whose owning task has exited
/// can no longer be the target of `try_deliver_local`.
pub fn evict_for_gc(mbox: MboxId) {
    registry().evict(mbox);
}

/// Fire-and-forget local delivery. Full or missing mailboxes drop
/// silently with a debug log. There is no end-to-end ack — `mail_send`'s
/// contract is best-effort.
pub fn try_deliver_local(mbox: MboxId, payload: Vec<u8>) {
    let Some(tx) = lookup(mbox) else {
        debug!(?mbox, "drop: no receiver");
        return;
    };
    if let Err(e) = tx.try_send(payload) {
        debug!(?mbox, error = %e, "drop: mailbox full or closed");
    }
}

/// Factory + identity tag for a fresh mailbox.
pub struct Mailbox;

impl Mailbox {
    /// Allocate a fresh `MboxId` and return it together with the
    /// receive half. The id is monotonically increasing and never `0`.
    /// Dropping the returned `MailboxRecv` evicts the registry entry;
    /// future local sends to this id silently drop.
    pub fn new() -> (MboxId, MailboxRecv) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(MBOX_CAPACITY);
        let id = registry().alloc(tx);
        (id, MailboxRecv { id, rx })
    }
}

/// Receive half of a mailbox. Holds the registry entry alive — Drop
/// evicts.
pub struct MailboxRecv {
    id: MboxId,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl MailboxRecv {
    pub fn id(&self) -> MboxId {
        self.id
    }
    /// Await the next message. `None` once every sender has dropped.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
    /// Non-blocking poll. `None` if the channel is empty (or closed).
    pub fn try_recv(&mut self) -> Option<Vec<u8>> {
        self.rx.try_recv().ok()
    }
}

impl Drop for MailboxRecv {
    fn drop(&mut self) {
        registry().evict(self.id);
    }
}

impl std::fmt::Debug for MailboxRecv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailboxRecv")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Wait for the first of `mboxes` to be ready and return its index +
/// the message that woke it. `None` if every mailbox's channel is
/// closed (no senders left).
///
/// Polling order is biased low-index-first, so ties resolve
/// deterministically — same tie-break convention the rest of the
/// workspace uses (NodeId ascending, lowest-NodeId-wins, etc.).
pub async fn select_mboxes(
    mboxes: &mut [&mut MailboxRecv],
) -> Option<(usize, Vec<u8>)> {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    if mboxes.is_empty() {
        return None;
    }

    struct Sel<'a, 'b> {
        mboxes: &'b mut [&'a mut MailboxRecv],
    }

    impl<'a, 'b> Future for Sel<'a, 'b> {
        type Output = Option<(usize, Vec<u8>)>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mboxes = &mut self.get_mut().mboxes;
            let mut all_closed = true;
            for (i, m) in mboxes.iter_mut().enumerate() {
                match m.rx.poll_recv(cx) {
                    Poll::Ready(Some(msg)) => return Poll::Ready(Some((i, msg))),
                    Poll::Ready(None) => {}      // channel closed
                    Poll::Pending => all_closed = false,
                }
            }
            if all_closed { Poll::Ready(None) } else { Poll::Pending }
        }
    }

    Sel { mboxes }.await
}

/// Test-only: serialize tests that touch the process-singleton state.
/// `cargo test` runs targets in parallel; without this serialization
/// the global counters race in confusing ways.
#[cfg(test)]
pub(crate) static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn alloc_is_monotonic_and_nonzero() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (a, _ra) = Mailbox::new();
        let (b, _rb) = Mailbox::new();
        let (c, _rc) = Mailbox::new();
        assert_ne!(a.0, 0);
        assert!(a.0 < b.0);
        assert!(b.0 < c.0);
    }

    #[tokio::test]
    async fn drop_evicts_registry_entry() {
        let _g = TEST_MUTEX.lock().unwrap();
        let count_before = registry().live_count();
        let (id, recv) = Mailbox::new();
        assert_eq!(registry().live_count(), count_before + 1);
        assert!(lookup(id).is_some());
        drop(recv);
        assert!(lookup(id).is_none());
        assert_eq!(registry().live_count(), count_before);
    }

    #[tokio::test]
    async fn local_delivery_round_trips() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (id, mut rx) = Mailbox::new();
        try_deliver_local(id, b"hello".to_vec());
        let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg.as_deref(), Some(&b"hello"[..]));
    }

    #[tokio::test]
    async fn try_recv_returns_message_then_none() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (id, mut rx) = Mailbox::new();
        try_deliver_local(id, b"x".to_vec());
        // Give the dispatch a tick to land — try_send is synchronous
        // here but we yield once to be safe.
        tokio::task::yield_now().await;
        let got = rx.try_recv();
        assert_eq!(got.as_deref(), Some(&b"x"[..]));
        assert!(rx.try_recv().is_none());
    }

    #[tokio::test]
    async fn full_channel_drops_silently_no_panic() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (id, _rx) = Mailbox::new();
        // Send well past capacity. The bounded channel rejects overflow;
        // try_deliver_local swallows the Err.
        for i in 0..MBOX_CAPACITY + 100 {
            try_deliver_local(id, vec![i as u8]);
        }
    }

    #[tokio::test]
    async fn missing_mailbox_drop_is_silent() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (id, recv) = Mailbox::new();
        drop(recv);
        try_deliver_local(id, b"into-the-void".to_vec()); // must not panic
    }

    #[tokio::test]
    async fn select_returns_first_ready_index_with_message() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (a, mut ra) = Mailbox::new();
        let (_b, mut rb) = Mailbox::new();
        let (_c, mut rc) = Mailbox::new();
        try_deliver_local(a, b"x".to_vec());
        let (idx, msg) = select_mboxes(&mut [&mut ra, &mut rb, &mut rc])
            .await
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(&msg, b"x");
    }

    #[tokio::test]
    async fn select_picks_only_ready_mailbox_among_three() {
        let _g = TEST_MUTEX.lock().unwrap();
        let (_a, mut ra) = Mailbox::new();
        let (_b, mut rb) = Mailbox::new();
        let (c, mut rc) = Mailbox::new();
        try_deliver_local(c, b"hi".to_vec());
        let (idx, msg) = select_mboxes(&mut [&mut ra, &mut rb, &mut rc])
            .await
            .unwrap();
        assert_eq!(idx, 2);
        assert_eq!(&msg, b"hi");
    }

    #[tokio::test]
    async fn select_empty_slice_returns_none() {
        let _g = TEST_MUTEX.lock().unwrap();
        let result = select_mboxes(&mut []).await;
        assert!(result.is_none());
    }
}
