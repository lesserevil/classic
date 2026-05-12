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
pub mod mbox;
pub mod send;

pub use error::MailError;
pub use mbox::{lookup, select_mboxes, try_deliver_local, Mailbox, MailboxRecv, MBOX_CAPACITY};
pub use send::{
    init, mail_send, self_node_id, LOCAL_FULL_DROPS, LOCAL_MISSING_DROPS, MAX_MAIL_BYTES,
};
