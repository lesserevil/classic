//! Device capability tokens + broker for classic. Plan-04
//! (`plans/04-spawn-pipeline.md`) §"DeviceCap and CapBroker" is the
//! design source.
//!
//! This crate is the in-memory accounting layer — Tasks 2 and 3 of the
//! plan-04 series add the cgroup-v2 hierarchy and BPF device-controller
//! attach + sync paths against the same broker.

pub mod broker;
pub mod cgroup;
pub mod devctrl;
pub mod nvidia;
pub mod pci;

pub use broker::{
    AcquireError, BdfAddr, CapBroker, CapSnapshot, DeviceCap, DeviceKind,
};
pub use cgroup::{
    create_scope, ensure_slice, RealSysroot, ScopeHandle, Sysroot, CGROUP_REL, PIDS_MAX,
    SUBTREE_CONTROL,
};
pub use devctrl::{
    build_allowlist, Allowlist, DeviceClass, DeviceController, DeviceRule, NoOpDeviceController,
};
pub use nvidia::list_nvidia_minors;
pub use pci::resolve_bdf;
