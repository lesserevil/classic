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
//!
//! [`place_one_in_pool`] is the per-member picker used by PACK: it
//! projects the pool back into a synthetic `NodeAd` (in-use bits
//! reflect the pool state), checks the member's requirement against
//! that view, and — when the requirement references GPU state —
//! identifies *which* GPU slot is being claimed so the caller can
//! deduct it before placing the next member.

use crate::ast::Requirement;
use crate::eval::matches;
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

    /// Apply the deductions in `picked` (best-effort; out-of-range
    /// indices or over-deductions are ignored — they shouldn't happen
    /// in practice because [`place_one_in_pool`] only returns indices
    /// it has just validated as free).
    pub fn deduct(&mut self, picked: &Picked) {
        if let Some(idx) = picked.gpu_idx {
            let _ = self.take_gpu(idx);
        }
        let _ = self.take_cpu(picked.cpu_slots);
        let _ = self.take_ram(picked.ram_mb);
    }
}

/// What a single member of a placement group claims from a node's
/// free pool. v1 tracks an optional GPU slot index (whichever one the
/// member's requirement uniquely identifies) plus CPU slot and RAM
/// counts. v1 doesn't infer CPU/RAM costs from predicates — those
/// fields are zero until callers wire explicit reservations into
/// member specs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub gpu_idx: Option<usize>,
    pub cpu_slots: u32,
    pub ram_mb: u64,
}

/// Project the original `ad` through the live state of `pool`. The
/// returned NodeAd is a clone of `ad` with `gpu[i].in_use` rewritten
/// to mirror `pool.gpu_in_use[i]`. Predicate evaluation against the
/// view reflects ongoing PACK deductions.
pub fn pool_view(ad: &NodeAd, pool: &FreePool) -> NodeAd {
    let mut view = ad.clone();
    for (i, g) in view.gpu.iter_mut().enumerate() {
        g.in_use = !pool.is_gpu_free(i);
    }
    view
}

/// Like `pool_view` but with every GPU forced to `in_use=true`. Used
/// to detect requirements that don't depend on any specific GPU
/// being free — if the requirement still matches under "everything
/// busy", no GPU needs to be claimed.
fn pool_view_all_busy(ad: &NodeAd) -> NodeAd {
    let mut view = ad.clone();
    for g in view.gpu.iter_mut() {
        g.in_use = true;
    }
    view
}

/// Hypothetical view where only `keep_idx` is free; every other GPU
/// is marked in_use. Used by the per-member picker to identify which
/// specific GPU slot the requirement uniquely depends on.
fn pool_view_only_free(ad: &NodeAd, keep_idx: usize) -> NodeAd {
    let mut view = ad.clone();
    for (i, g) in view.gpu.iter_mut().enumerate() {
        g.in_use = i != keep_idx;
    }
    view
}

/// Per-member picker. Returns `Some(Picked)` if the member's
/// requirement is satisfiable against the current pool state, with
/// the `gpu_idx` field naming the slot the member claims (if any).
///
/// Algorithm:
/// 1. Build a view of `ad` reflecting `pool` state. If the
///    requirement doesn't match, the member can't be placed here.
/// 2. Build an "all-busy" view (every GPU `in_use=true`). If the
///    requirement still matches, it doesn't depend on free GPUs —
///    return a Picked with `gpu_idx=None`.
/// 3. Otherwise the requirement does depend on a free GPU. Sort the
///    currently-free GPU slots by `vram_mb` descending (tiebreak by
///    index ascending) and pick the first whose "this-slot-is-the-only-
///    free-one" hypothetical view still satisfies the requirement.
///
/// Member declaration order (FR-9) decides who wins contention,
/// because the caller invokes this in order and `pool.deduct`s after
/// each pick.
pub fn place_one_in_pool(req: &Requirement, ad: &NodeAd, pool: &FreePool) -> Option<Picked> {
    let cur = pool_view(ad, pool);
    if !matches(req, &cur) {
        return None;
    }
    let all_busy = pool_view_all_busy(ad);
    if matches(req, &all_busy) {
        // Requirement doesn't need any free GPU.
        return Some(Picked::default());
    }
    // Identify the GPU slot to claim. Prefer the richest free GPU so
    // earlier members in a contention scenario grab the strongest
    // hardware (matches FR-9 first-listed-wins semantics).
    let mut candidates: Vec<usize> = (0..pool.gpu_total())
        .filter(|&i| pool.is_gpu_free(i))
        .collect();
    candidates.sort_by(|&a, &b| {
        ad.gpu[b]
            .vram_mb
            .cmp(&ad.gpu[a].vram_mb)
            .then(a.cmp(&b))
    });
    for &idx in &candidates {
        let view = pool_view_only_free(ad, idx);
        if matches(req, &view) {
            return Some(Picked {
                gpu_idx: Some(idx),
                cpu_slots: 0,
                ram_mb: 0,
            });
        }
    }
    // The current view matches but no single free GPU is sufficient on
    // its own — the requirement aggregates over multiple free GPUs and
    // there isn't enough redundancy left to claim just one. Treat as
    // unplaceable for v1.
    None
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
