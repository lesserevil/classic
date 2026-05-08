//! Device-capability broker. Pure in-memory accounting — Tasks 2 (cgroup
//! hierarchy) and 3 (BPF device controller) attach to `acquire` /
//! `release` events to push allowlist updates to the kernel.
//!
//! Linked from plan 04 §"DeviceCap and CapBroker".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use classic_proto::MboxId;

/// Identifier for a kind of device the broker tracks. Adding new variants
/// is a breaking change for downstream consumers (the BPF program in
/// Task 3 matches on these).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    /// `/dev/nvidia<N>`. The Task-3 BPF program will allow `(c, 195, N)`
    /// per held minor plus the NVIDIA control nodes (195, 254 / 195, 255)
    /// when any GpuMinor cap is held by the cgroup.
    GpuMinor(u32),
    /// Non-NVIDIA passthrough. Task 3 resolves the BDF to a `(major,
    /// minor)` via `/sys/bus/pci/devices/<bdf>/uevent`.
    PciSlot(BdfAddr),
}

/// PCI bus-device-function address. Standard Linux `DDDD:BB:DD.F`
/// rendering supplied via `Display`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct BdfAddr {
    pub domain: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl std::fmt::Display for BdfAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{}",
            self.domain, self.bus, self.device, self.function & 0x7
        )
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// Exclusive acquire failed because the kind is currently held by
    /// any other holder (exclusive OR shared).
    #[error("device {kind:?} is already held")]
    Taken { kind: DeviceKind },
    /// Shared acquire failed because the kind is currently held
    /// exclusively. `n` is the exclusive holder count (always 1 — the
    /// field is kept for API compatibility with the plan's enum spec).
    #[error("device {kind:?} is held exclusively (by {n} holder(s))")]
    SharedConflict { kind: DeviceKind, n: usize },
}

impl AcquireError {
    pub fn kind(&self) -> DeviceKind {
        match self {
            AcquireError::Taken { kind } => *kind,
            AcquireError::SharedConflict { kind, .. } => *kind,
        }
    }
}

/// Diagnostic snapshot — what the broker knows right now. Cheap to
/// produce; safe to log.
#[derive(Clone, Debug)]
pub struct CapSnapshot {
    pub kind: DeviceKind,
    pub holder: MboxId,
    pub exclusive: bool,
}

/// Per-cap unique id used internally to find a cap on Drop without
/// tracking it by `(holder, kind)` (which isn't unique under shared).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct CapId(u64);

#[derive(Debug)]
struct CapEntry {
    kind: DeviceKind,
    holder: MboxId,
    exclusive: bool,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: u64,
    caps: HashMap<CapId, CapEntry>,
}

/// In-memory device-capability broker. Cheap to clone (`Arc` internally);
/// callers are expected to thread one broker through the daemon.
#[derive(Clone, Default)]
pub struct CapBroker {
    inner: Arc<Mutex<Inner>>,
}

impl CapBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically acquire all `kinds` for `holder`. All-or-nothing: if
    /// even one kind cannot be held under the requested `exclusive`
    /// flag, no caps are issued. Duplicate kinds in `kinds` are merged
    /// (treated as a single request per kind).
    pub fn acquire(
        &self,
        holder: MboxId,
        kinds: &[DeviceKind],
        exclusive: bool,
    ) -> Result<Vec<DeviceCap>, AcquireError> {
        let mut inner = self.inner.lock().expect("broker poisoned");

        // De-dup kinds while preserving order, so a request like
        // [Gpu(0), Gpu(0)] doesn't double-count.
        let mut deduped: Vec<DeviceKind> = Vec::with_capacity(kinds.len());
        for k in kinds {
            if !deduped.contains(k) {
                deduped.push(*k);
            }
        }

        // Pre-flight: check conflicts for each kind without mutating.
        for kind in &deduped {
            if let Some(err) = conflict(&inner, *kind, exclusive) {
                return Err(err);
            }
        }

        // Allocate cap_ids and insert. After this point the operation
        // is committed — the caller gets its caps and the inner state
        // contains the entries.
        let mut out = Vec::with_capacity(deduped.len());
        for kind in deduped {
            inner.next_id += 1;
            let cap_id = CapId(inner.next_id);
            inner.caps.insert(
                cap_id,
                CapEntry { kind, holder, exclusive },
            );
            out.push(DeviceCap {
                kind,
                exclusive,
                holder,
                cap_id,
                handle: BrokerHandle { inner: self.inner.clone(), cap_id },
            });
        }
        Ok(out)
    }

    /// Force-release every cap belonging to `holder`. Subsequent `Drop`
    /// of any DeviceCap previously held by `holder` is a no-op.
    pub fn release_all(&self, holder: MboxId) {
        let mut inner = self.inner.lock().expect("broker poisoned");
        inner.caps.retain(|_, entry| entry.holder != holder);
    }

    /// Cheap diagnostic dump. Order is unspecified.
    pub fn snapshot(&self) -> Vec<CapSnapshot> {
        let inner = self.inner.lock().expect("broker poisoned");
        inner
            .caps
            .values()
            .map(|e| CapSnapshot { kind: e.kind, holder: e.holder, exclusive: e.exclusive })
            .collect()
    }
}

/// Capability token. There is intentionally NO public constructor — the
/// only way to obtain one is via `CapBroker::acquire`. RAII Drop releases
/// the cap unless `release_all` already removed it.
pub struct DeviceCap {
    pub kind: DeviceKind,
    pub exclusive: bool,
    pub holder: MboxId,
    cap_id: CapId,
    handle: BrokerHandle,
}

impl std::fmt::Debug for DeviceCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCap")
            .field("kind", &self.kind)
            .field("exclusive", &self.exclusive)
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

/// Internal handle whose Drop releases its cap_id. Keeps the broker
/// alive long enough for the release to land via Arc.
struct BrokerHandle {
    inner: Arc<Mutex<Inner>>,
    cap_id: CapId,
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        // If `release_all` already removed the entry, this is a no-op.
        if let Ok(mut g) = self.inner.lock() {
            g.caps.remove(&self.cap_id);
        }
    }
}

fn conflict(inner: &Inner, kind: DeviceKind, want_exclusive: bool) -> Option<AcquireError> {
    let mut exclusive_holders = 0usize;
    let mut total_holders = 0usize;
    for entry in inner.caps.values() {
        if entry.kind != kind {
            continue;
        }
        total_holders += 1;
        if entry.exclusive {
            exclusive_holders += 1;
        }
    }
    if want_exclusive {
        if total_holders > 0 {
            return Some(AcquireError::Taken { kind });
        }
        None
    } else {
        // Shared request: refused only if some holder is exclusive.
        if exclusive_holders > 0 {
            return Some(AcquireError::SharedConflict { kind, n: exclusive_holders });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb(n: u64) -> MboxId {
        MboxId(n)
    }

    #[test]
    fn exclusive_acquire_then_taken_on_second() {
        let b = CapBroker::new();
        let _a = b.acquire(mb(1), &[DeviceKind::GpuMinor(0)], true).unwrap();
        let err = b.acquire(mb(2), &[DeviceKind::GpuMinor(0)], true).unwrap_err();
        assert!(matches!(err, AcquireError::Taken { kind: DeviceKind::GpuMinor(0) }));
    }

    #[test]
    fn shared_co_acquire_ok() {
        let b = CapBroker::new();
        let _a = b.acquire(mb(1), &[DeviceKind::GpuMinor(0)], false).unwrap();
        let _b = b.acquire(mb(2), &[DeviceKind::GpuMinor(0)], false).unwrap();
        assert_eq!(b.snapshot().len(), 2);
    }

    #[test]
    fn exclusive_blocked_by_shared() {
        let b = CapBroker::new();
        let _shared = b.acquire(mb(1), &[DeviceKind::GpuMinor(0)], false).unwrap();
        let err = b.acquire(mb(2), &[DeviceKind::GpuMinor(0)], true).unwrap_err();
        assert!(matches!(err, AcquireError::Taken { .. }));
    }

    #[test]
    fn shared_blocked_by_exclusive() {
        let b = CapBroker::new();
        let _excl = b.acquire(mb(1), &[DeviceKind::GpuMinor(0)], true).unwrap();
        let err = b.acquire(mb(2), &[DeviceKind::GpuMinor(0)], false).unwrap_err();
        assert!(matches!(err, AcquireError::SharedConflict { kind: DeviceKind::GpuMinor(0), n: 1 }));
    }

    #[test]
    fn drop_releases_one_cap() {
        let b = CapBroker::new();
        let caps = b.acquire(mb(1), &[DeviceKind::GpuMinor(0)], true).unwrap();
        assert_eq!(b.snapshot().len(), 1);
        drop(caps);
        assert!(b.snapshot().is_empty());
        // Now C can grab it.
        let _c = b.acquire(mb(2), &[DeviceKind::GpuMinor(0)], true).unwrap();
    }

    #[test]
    fn release_all_removes_every_cap_for_holder() {
        let b = CapBroker::new();
        let caps = b
            .acquire(
                mb(1),
                &[DeviceKind::GpuMinor(0), DeviceKind::GpuMinor(1)],
                true,
            )
            .unwrap();
        // Also grab a shared cap.
        let _shared = b.acquire(mb(1), &[DeviceKind::GpuMinor(2)], false).unwrap();
        assert_eq!(b.snapshot().len(), 3);
        b.release_all(mb(1));
        assert!(b.snapshot().is_empty());
        // The held caps are still owned by the test — their Drop must not
        // panic and must not re-touch broker state.
        drop(caps);
        assert!(b.snapshot().is_empty());
    }

    #[test]
    fn acquire_is_atomic_all_or_nothing() {
        let b = CapBroker::new();
        // Pre-take GpuMinor(1) so the multi-kind request fails partway.
        let _other = b.acquire(mb(99), &[DeviceKind::GpuMinor(1)], true).unwrap();
        let err = b
            .acquire(
                mb(1),
                &[DeviceKind::GpuMinor(0), DeviceKind::GpuMinor(1)],
                true,
            )
            .unwrap_err();
        assert!(matches!(err, AcquireError::Taken { .. }));
        // Crucially: GpuMinor(0) must NOT be held by mb(1) afterward.
        let snap = b.snapshot();
        assert!(!snap.iter().any(|s| s.holder == mb(1)));
    }

    #[test]
    fn duplicate_kinds_in_one_request_dedupe() {
        let b = CapBroker::new();
        let caps = b
            .acquire(
                mb(1),
                &[DeviceKind::GpuMinor(0), DeviceKind::GpuMinor(0)],
                true,
            )
            .unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(b.snapshot().len(), 1);
    }

    #[test]
    fn pci_slot_acquired_independently_from_gpu_minor() {
        let b = CapBroker::new();
        let bdf = BdfAddr { domain: 0, bus: 0xc0, device: 0, function: 0 };
        let _gpu = b.acquire(mb(1), &[DeviceKind::GpuMinor(0)], true).unwrap();
        let _pci = b.acquire(mb(1), &[DeviceKind::PciSlot(bdf)], true).unwrap();
        assert_eq!(b.snapshot().len(), 2);
    }

    /// Cross-thread contention: 100 threads each pick one of 8 GPU
    /// minors at random and try to hold it exclusively. At any moment
    /// at most 8 caps exist (one per minor); no panics, no deadlock.
    #[test]
    fn cross_thread_contention_caps_at_pool_size() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let b = CapBroker::new();
        let max_observed = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for tid in 0..100u32 {
                let b = b.clone();
                let max = max_observed.clone();
                s.spawn(move || {
                    let kind = DeviceKind::GpuMinor(tid % 8);
                    if let Ok(caps) = b.acquire(MboxId(tid as u64), &[kind], true) {
                        let now = b.snapshot().len();
                        max.fetch_max(now, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_micros(50));
                        drop(caps);
                    }
                });
            }
        });

        let observed = max_observed.load(Ordering::Relaxed);
        assert!(observed <= 8, "saw {observed} caps at once, > pool size 8");
        // After everyone releases, broker must be empty.
        assert!(b.snapshot().is_empty());
    }

    #[test]
    fn bdf_displays_canonical_format() {
        let b = BdfAddr { domain: 0, bus: 0x1b, device: 0, function: 0 };
        assert_eq!(format!("{}", b), "0000:1b:00.0");
    }
}
