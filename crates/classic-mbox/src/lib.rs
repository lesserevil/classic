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

pub mod error;
pub mod frames;
pub mod handler;
pub mod mbox;
pub mod send;

pub use error::MailError;
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
