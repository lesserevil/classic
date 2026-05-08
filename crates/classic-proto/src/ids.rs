use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// 128-bit non-recurring per-node identity. Generated on daemon first start
/// and persisted under `$CLASSIC_STATE_DIR/node_id`.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize, Encode, Decode)]
pub struct NodeId(pub [u8; 16]);

impl NodeId {
    pub const ZERO: NodeId = NodeId([0u8; 16]);

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId(")?;
        for b in self.0.iter() {
            write!(f, "{:02x}", b)?;
        }
        write!(f, ")")
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0.iter() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// Per-task mailbox id, allocated by the local node. Non-recurring within the
/// node's lifetime; reset on daemon restart. `MboxId(0)` is reserved for the
/// per-node kernel/control mailbox.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct MboxId(pub u64);

impl MboxId {
    pub const KERNEL: MboxId = MboxId(0);
}

impl std::fmt::Display for MboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Cluster-wide address: a `(NodeId, MboxId)` pair.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct NetId {
    pub node: NodeId,
    pub mbox: MboxId,
}

impl std::fmt::Display for NetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.node, self.mbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn node_id_display_is_lowercase_hex() {
        let id = NodeId([0xAB, 0xCD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xEF, 0x12]);
        assert_eq!(format!("{}", id), "abcd000000000000000000000000ef12");
    }

    #[test]
    fn ids_hash_and_eq() {
        let a = NetId { node: NodeId([1; 16]), mbox: MboxId(7) };
        let b = NetId { node: NodeId([1; 16]), mbox: MboxId(7) };
        let c = NetId { node: NodeId([2; 16]), mbox: MboxId(7) };
        let mut s = HashSet::new();
        s.insert(a);
        assert!(s.contains(&b));
        assert!(!s.contains(&c));
    }
}
