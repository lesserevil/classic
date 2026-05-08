//! Per-daemon `MboxId` allocator. Atomic counter starting at 1; mbox 0
//! is reserved for the per-node kernel/control mailbox per
//! ARCHITECTURE.md §"Identity types".

use std::sync::atomic::{AtomicU64, Ordering};

use classic_proto::MboxId;

#[derive(Debug)]
pub struct MboxAllocator {
    next: AtomicU64,
}

impl MboxAllocator {
    /// Caller-visible: starts at 1. mbox 0 reserved.
    pub fn new() -> Self {
        Self { next: AtomicU64::new(1) }
    }
    /// Allocate the next monotonically-increasing MboxId.
    pub fn alloc(&self) -> MboxId {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        MboxId(n)
    }
}

impl Default for MboxAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_starts_at_one() {
        let a = MboxAllocator::new();
        assert_eq!(a.alloc(), MboxId(1));
        assert_eq!(a.alloc(), MboxId(2));
        assert_eq!(a.alloc(), MboxId(3));
    }

    #[test]
    fn allocator_thread_safe() {
        use std::sync::Arc;
        let a = Arc::new(MboxAllocator::new());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let a = a.clone();
            handles.push(std::thread::spawn(move || {
                let mut v = Vec::with_capacity(64);
                for _ in 0..64 {
                    v.push(a.alloc());
                }
                v
            }));
        }
        let mut all: Vec<MboxId> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        // All distinct.
        let mut sorted: Vec<u64> = all.iter().map(|m| m.0).collect();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 16 * 64);
    }
}
