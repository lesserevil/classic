//! Mailbox primitives + (later in plan 05) cluster-wide service
//! directory. Plan-05 (`plans/05-mailbox-service-directory.md`) is the
//! design source.
//!
//! Public surface this task lands:
//! - `Mailbox::new()` → `(MboxId, MailboxRecv)`
//! - `MailboxRecv::{recv, try_recv}`
//! - `select_mboxes(&mut [...])` returns `(idx, payload)` for the first
//!   ready mailbox
//! - `lookup(mbox)` / `try_deliver_local(mbox, payload)` for the
//!   in-process delivery path (used by Task 2's `mail_send`).
//!
//! `MBOX_CAPACITY = 1024` per the plan; fire-and-forget — full
//! mailboxes drop silently. `MboxId(0)` is reserved per ARCHITECTURE.md.

pub mod directory;
pub mod error;
pub mod frames;
pub mod gossip;
pub mod gossip_emit;
pub mod handler;
pub mod mbox;
pub mod send;

pub use directory::{
    apply_local_declare, apply_local_forget, apply_remote_ad, apply_remote_forget,
    bump_local_clock, gc_expired_tombstones, service_lookup, service_lookup_one, snapshot,
    Lamport, ServiceEntry, SnapshotEntry, TaskId, MAX_SVC_NAME, TOMBSTONE_TTL,
};
pub use error::{MailError, ServiceError};
pub use gossip::{
    build_sync_response, clear_current_task, on_inbound_ad, on_inbound_forget,
    on_inbound_sync_response, service_declare, service_forget, set_current_task,
    ServiceHandle, GOSSIP_IN_APPLIED, GOSSIP_OUT,
};
pub use gossip_emit::{clear_sink as clear_gossip_sink, set_sink as set_gossip_sink};
pub use frames::{
    decode_mbox_frame, encode_mail_delivery_failure, encode_mail_send, encode_service_ad,
    encode_service_forget, encode_service_sync, encode_service_sync_response,
    DeliveryFailureReason, MailDeliveryFailure, MailSend, MboxFrameError, MboxInbound,
    ServiceAd, ServiceForget, ServiceSync, ServiceSyncEntry, ServiceSyncResponse, MBOX_RANGE,
};
pub use handler::MboxHandler;
pub use mbox::{lookup, select_mboxes, try_deliver_local, Mailbox, MailboxRecv, MBOX_CAPACITY};
pub use send::{
    init, mail_send, self_node_id, set_peers, Peers, LOCAL_FULL_DROPS, LOCAL_MISSING_DROPS,
    MAX_MAIL_BYTES, REMOTE_NO_PEER_DROPS,
};
