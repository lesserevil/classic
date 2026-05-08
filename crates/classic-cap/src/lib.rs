//! Device capability tokens + broker for classic. Plan-04
//! (`plans/04-spawn-pipeline.md`) §"DeviceCap and CapBroker" is the
//! design source.
//!
//! This crate is the in-memory accounting layer — Tasks 2 and 3 of the
//! plan-04 series add the cgroup-v2 hierarchy and BPF device-controller
//! attach + sync paths against the same broker.

pub mod broker;
pub mod nvidia;

pub use broker::{
    AcquireError, BdfAddr, CapBroker, CapSnapshot, DeviceCap, DeviceKind,
};
pub use nvidia::list_nvidia_minors;
