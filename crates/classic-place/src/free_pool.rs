//! Mutable per-node free-pool used by PACK to deduct already-picked
//! resources as it walks group members. SPREAD doesn't need this
//! (at most one member per node) but PACK does — two members both
//! claiming the lone 80 GB GPU must not both succeed against the
//! unmodified ad.
//!
//! Constructed from a `NodeAd` via `from_ad`; deductions go through
//! `take_gpu` / `take_cpu` / `take_ram`. Accessors expose the current
//! free state so a member-level placer can test predicates against a
//! pool rather than the raw ad.

use crate::model::NodeAd;
use classic_proto::NodeId;

/// In-use bitmap + remaining counts for one node. Cheap to clone; PACK
/// snapshots the pool before each candidate node so a failed branch
/// can roll back without affecting siblings.
#[derive(Clone, Debug)]
pub struct FreePool {
    pub node_id: NodeId,
    /// Per-GPU `in_use` flag. Length matches `NodeAd::gpu`. Indexed by
    /// `GpuAd::index` directly when indices are 0..N; the constructor
    /// preserves the ad's slot order so callers can map back.
    gpu_in_use: Vec<bool>,
    /// Free CPU slots. v1 treats one "slot" = one core. Decremented by
    /// `take_cpu`; never goes negative (saturating).
    free_cpu_slots: u32,
    /// Free RAM in MiB. Decremented by `take_ram`; saturating.
    free_ram_mb: u64,
}

impl FreePool {
    /// Project a NodeAd into a fresh free pool. The ad's `gpu` vector
    /// order is preserved verbatim; `gpu_in_use[i]` mirrors
    /// `ad.gpu[i].in_use` at construction time.
    pub fn from_ad(ad: &NodeAd) -> Self {
        let gpu_in_use = ad.gpu.iter().map(|g| g.in_use).collect();
        let free_cpu_slots = ad.cpu.cores;
        // We don't currently track per-process RAM usage in the ad;
        // model the whole free pool as a single counter so deductions
        // round-trip without inventing data we don't have. Defaults to
        // mem.free_mb so a pool starts at the same place the predicate
        // would see in a stateless check.
        let free_ram_mb = ad.mem.free_mb;
        FreePool {
            node_id: ad.node_id,
            gpu_in_use,
            free_cpu_slots,
            free_ram_mb,
        }
    }

    /// Total GPU slot count (in-use plus free). Identical to
    /// `ad.gpu.len()` at construction.
    pub fn gpu_total(&self) -> usize {
        self.gpu_in_use.len()
    }

    /// Count of GPU slots currently marked free.
    pub fn gpu_free_count(&self) -> usize {
        self.gpu_in_use.iter().filter(|&&b| !b).count()
    }

    /// Is the GPU at slot `idx` currently free? `false` when `idx` is
    /// out of range so callers can probe blindly.
    pub fn is_gpu_free(&self, idx: usize) -> bool {
        self.gpu_in_use.get(idx).copied().map(|b| !b).unwrap_or(false)
    }

    /// Mark the GPU at `idx` as in-use. Returns `false` if `idx` is
    /// out of range or the slot was already taken — caller treats that
    /// as a feasibility miss, not a panic.
    pub fn take_gpu(&mut self, idx: usize) -> bool {
        match self.gpu_in_use.get_mut(idx) {
            Some(b) if !*b => {
                *b = true;
                true
            }
            _ => false,
        }
    }

    /// Free CPU slots remaining.
    pub fn free_cpu_slots(&self) -> u32 {
        self.free_cpu_slots
    }

    /// Deduct `n` CPU slots; returns `false` if the pool would go
    /// negative (no partial deduction).
    pub fn take_cpu(&mut self, n: u32) -> bool {
        if n <= self.free_cpu_slots {
            self.free_cpu_slots -= n;
            true
        } else {
            false
        }
    }

    /// Free RAM remaining (MiB).
    pub fn free_ram_mb(&self) -> u64 {
        self.free_ram_mb
    }

    /// Deduct `mb` MiB of RAM; returns `false` if the pool would go
    /// negative (no partial deduction).
    pub fn take_ram(&mut self, mb: u64) -> bool {
        if mb <= self.free_ram_mb {
            self.free_ram_mb -= mb;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GpuAd, NodeAd};

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    fn ad_with(cores: u32, free_mb: u64, gpus: Vec<bool>) -> NodeAd {
        let mut a = NodeAd::default();
        a.node_id = id(1);
        a.cpu.cores = cores;
        a.mem.free_mb = free_mb;
        a.mem.total_mb = free_mb;
        a.gpu = gpus
            .into_iter()
            .enumerate()
            .map(|(i, in_use)| GpuAd {
                index: i as u32,
                vendor: 0x10de,
                device: 0x2330,
                model: "h100".into(),
                vram_mb: 80_000,
                vram_free_mb: 80_000,
                sm_count: 132,
                in_use,
                mig: false,
            })
            .collect();
        a
    }

    #[test]
    fn from_ad_mirrors_initial_state() {
        let ad = ad_with(32, 65_536, vec![false, true, false]);
        let pool = FreePool::from_ad(&ad);
        assert_eq!(pool.gpu_total(), 3);
        assert_eq!(pool.gpu_free_count(), 2);
        assert!(pool.is_gpu_free(0));
        assert!(!pool.is_gpu_free(1));
        assert!(pool.is_gpu_free(2));
        assert_eq!(pool.free_cpu_slots(), 32);
        assert_eq!(pool.free_ram_mb(), 65_536);
    }

    #[test]
    fn take_gpu_succeeds_then_blocks() {
        let ad = ad_with(4, 1024, vec![false]);
        let mut pool = FreePool::from_ad(&ad);
        assert!(pool.take_gpu(0));
        assert!(!pool.is_gpu_free(0));
        assert!(!pool.take_gpu(0));
        assert!(!pool.take_gpu(99)); // out of range never panics
    }

    #[test]
    fn take_cpu_and_ram_saturate() {
        let ad = ad_with(4, 1024, vec![]);
        let mut pool = FreePool::from_ad(&ad);
        assert!(pool.take_cpu(3));
        assert_eq!(pool.free_cpu_slots(), 1);
        assert!(!pool.take_cpu(2));
        assert_eq!(pool.free_cpu_slots(), 1);
        assert!(pool.take_ram(1000));
        assert_eq!(pool.free_ram_mb(), 24);
        assert!(!pool.take_ram(25));
        assert_eq!(pool.free_ram_mb(), 24);
    }
}
