//! Shared helpers for plan-05 integration tests. Each test grabs the
//! process-wide mailbox mutex (re-exported below) before touching the
//! mailbox / directory / gossip singletons.

#![allow(dead_code)]

use std::sync::Mutex;

use classic_proto::{MboxId, NetId, NodeId};

/// Re-export for the integration tests. classic_mbox::mbox::TEST_MUTEX
/// is `pub(crate)` so we mirror it here via this stand-in mutex,
/// understanding that only this test binary runs against it. Tests
/// inside the lib still use the crate-internal mutex; we use this one.
pub static IT_MUTEX: Mutex<()> = Mutex::new(());

pub fn nid(byte: u8) -> NodeId {
    let mut bytes = [0u8; 16];
    bytes[0] = byte;
    NodeId(bytes)
}

pub fn netid(node: u8, mbox: u64) -> NetId {
    NetId { node: nid(node), mbox: MboxId(mbox) }
}
