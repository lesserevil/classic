pub mod config;
pub mod discovery;
pub mod discovery_loop;
pub mod frames;
pub mod gossip;
pub mod schema;
pub mod start;
pub mod store;

pub use config::{AdConfig, AdConfigError};
pub use discovery::{RealSysroot, Sysroot};
pub use discovery_loop::Discovery;
pub use frames::{
    decode_ad_frame, encode_ad_gossip, encode_ad_request, encode_node_ad, AdFrameError,
    AdInbound, AD_RANGE, FRAME_AD_GOSSIP, FRAME_AD_REQUEST, FRAME_NODE_AD,
};
pub use gossip::{ad_request_frame, Gossip, Peers};
pub use schema::{
    AdGossip, AdRequest, AdUpdate, CpuInfo, GpuInfo, LoadSample, MemInfo, NodeAd, NumaNode,
    PciDevice,
};
pub use start::{start, AdHandles, StartError};
pub use store::{AdStore, UpsertOutcome};
