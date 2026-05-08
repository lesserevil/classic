//! Device-controller surface. The kernel-side enforcement is a
//! `BPF_PROG_TYPE_CGROUP_DEVICE` program attached to the per-task scope
//! cgroup fd; that BPF program lives behind a `hw-bpf` feature flag in
//! this crate (deferred — clang + libbpf-dev are required to compile it,
//! and the kernel ABI requires root to attach).
//!
//! Everything else — the allowlist data model, the `DeviceController`
//! trait used by `classic-spawn` to wire scope creation, and a
//! `NoOpDeviceController` for unprivileged tests — lives here so the
//! daemon can be built and tested without those build-time deps.

use std::sync::{Arc, Mutex};

use crate::broker::{BdfAddr, CapSnapshot, DeviceKind};

/// Logical contents of the BPF allowlist map. `(c, major, minor)` triples
/// — `Some(minor)` for a single minor, `None` for the wildcard.
///
/// The base set (`/dev/null`, `/dev/zero`, `/dev/random`, `/dev/urandom`,
/// `/dev/tty`) is always allowed under `(c, 1, *)`. Additional entries
/// arrive from `CapBroker` snapshots: every `GpuMinor(N)` adds
/// `(c, 195, N)`, and any `GpuMinor` cap also unlocks the NVIDIA control
/// nodes `(c, 195, 254)` and `(c, 195, 255)`. `PciSlot(bdf)` adds the
/// `(major, minor)` resolved from `/sys/bus/pci/devices/<bdf>/uevent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowlist {
    pub entries: Vec<DeviceRule>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRule {
    pub class: DeviceClass,
    pub major: u32,
    /// `None` = wildcard ("any minor in this major").
    pub minor: Option<u32>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceClass {
    Char,
    Block,
}

const NVIDIA_CTL_MAJOR: u32 = 195;
const NVIDIA_CTL_MINORS: &[u32] = &[254, 255];

/// Build the allowlist for a snapshot of `CapSnapshot` entries belonging
/// to ONE holder. PCI minors are resolved via the `pci_resolver` callback
/// — usually `classic_cap::pci::resolve_bdf` paired with the daemon's
/// sysroot — so this function stays pure and testable.
pub fn build_allowlist<F>(holder_caps: &[CapSnapshot], pci_resolver: F) -> Allowlist
where
    F: Fn(BdfAddr) -> Option<(u32, u32)>,
{
    let mut entries = base_set();

    let any_gpu = holder_caps
        .iter()
        .any(|c| matches!(c.kind, DeviceKind::GpuMinor(_)));
    if any_gpu {
        for minor in NVIDIA_CTL_MINORS {
            entries.push(DeviceRule {
                class: DeviceClass::Char,
                major: NVIDIA_CTL_MAJOR,
                minor: Some(*minor),
            });
        }
    }

    for cap in holder_caps {
        match cap.kind {
            DeviceKind::GpuMinor(n) => entries.push(DeviceRule {
                class: DeviceClass::Char,
                major: NVIDIA_CTL_MAJOR,
                minor: Some(n),
            }),
            DeviceKind::PciSlot(bdf) => {
                if let Some((major, minor)) = pci_resolver(bdf) {
                    entries.push(DeviceRule {
                        class: DeviceClass::Char,
                        major,
                        minor: Some(minor),
                    });
                }
            }
        }
    }

    Allowlist { entries }
}

/// Standard char-device wildcards that every scope keeps allowed:
/// `(c, 1, *)` covers `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`,
/// `/dev/urandom`, etc. The bare entry uses `minor = None` (wildcard).
fn base_set() -> Vec<DeviceRule> {
    vec![DeviceRule {
        class: DeviceClass::Char,
        major: 1,
        minor: None,
    }]
}

/// Trait the spawn pipeline uses to attach a per-scope device controller.
/// Production wires this to the BPF loader (behind `hw-bpf`); tests use
/// `NoOpDeviceController` which records calls in-memory.
pub trait DeviceController: Send + Sync {
    /// Push the latest allowlist for this scope. Idempotent — safe to
    /// call repeatedly with the same allowlist.
    fn sync(&self, allowlist: &Allowlist);
}

/// Test / unprivileged stand-in. Records the most-recent sync.
#[derive(Default)]
pub struct NoOpDeviceController {
    last: Mutex<Option<Allowlist>>,
}

impl NoOpDeviceController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn last(&self) -> Option<Allowlist> {
        self.last.lock().expect("poisoned").clone()
    }
}

impl DeviceController for NoOpDeviceController {
    fn sync(&self, allowlist: &Allowlist) {
        *self.last.lock().expect("poisoned") = Some(allowlist.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::DeviceKind;
    use classic_proto::MboxId;

    fn snap(kind: DeviceKind) -> CapSnapshot {
        CapSnapshot { kind, holder: MboxId(1), exclusive: true }
    }

    #[test]
    fn empty_caps_yields_just_base_set() {
        let a = build_allowlist(&[], |_| None);
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.entries[0].major, 1);
        assert_eq!(a.entries[0].minor, None);
    }

    #[test]
    fn gpu_minor_adds_per_minor_and_unlocks_ctl_nodes() {
        let caps = vec![snap(DeviceKind::GpuMinor(0))];
        let a = build_allowlist(&caps, |_| None);
        // base + 254 + 255 + 0
        assert_eq!(a.entries.len(), 4);
        assert!(a
            .entries
            .iter()
            .any(|r| r.major == 195 && r.minor == Some(0)));
        for ctl in NVIDIA_CTL_MINORS {
            assert!(
                a.entries
                    .iter()
                    .any(|r| r.major == 195 && r.minor == Some(*ctl)),
                "ctl minor {} missing from allowlist",
                ctl
            );
        }
    }

    #[test]
    fn ctl_nodes_only_unlocked_when_some_gpu_held() {
        let caps = vec![snap(DeviceKind::PciSlot(BdfAddr {
            domain: 0,
            bus: 0xc0,
            device: 0,
            function: 0,
        }))];
        let a = build_allowlist(&caps, |_| Some((10, 0)));
        // Should contain base + (10, 0). No 195 entries.
        assert!(!a.entries.iter().any(|r| r.major == 195));
    }

    #[test]
    fn pci_resolver_supplies_major_minor() {
        let bdf = BdfAddr { domain: 0, bus: 0xc0, device: 0, function: 0 };
        let caps = vec![snap(DeviceKind::PciSlot(bdf))];
        let a = build_allowlist(&caps, |b| {
            assert_eq!(b, bdf);
            Some((10, 7))
        });
        assert!(a
            .entries
            .iter()
            .any(|r| r.major == 10 && r.minor == Some(7)));
    }

    #[test]
    fn pci_resolver_none_skips_entry() {
        let bdf = BdfAddr { domain: 0, bus: 0xc0, device: 0, function: 0 };
        let caps = vec![snap(DeviceKind::PciSlot(bdf))];
        let a = build_allowlist(&caps, |_| None);
        // Just the base set.
        assert_eq!(a.entries.len(), 1);
    }

    #[test]
    fn noop_controller_records_last_sync() {
        let c = NoOpDeviceController::new();
        let a = build_allowlist(&[snap(DeviceKind::GpuMinor(0))], |_| None);
        c.sync(&a);
        assert_eq!(c.last().unwrap(), a);
    }
}
