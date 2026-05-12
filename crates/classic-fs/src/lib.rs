//! Plan-9-style hardware-as-files via a 9P2000.L namespace server.
//! Plan-06 (`plans/06-9p-namespace-server.md`) is the design source.
//!
//! This crate owns frame range `0x0400..=0x04FF` — `NineReq=0x0400` and
//! `NineRsp=0x0401` carry raw 9P bytes (minus the 9P size[4] prefix —
//! the outer Classic frame length is authoritative).
//!
//! Task 1 (classic-imf, this commit) ships the wire codec; later tasks
//! add the synthetic file-tree, server loop, namespace assembly + FUSE
//! mount lifecycle, and remote bind-mount support.

pub mod errno;
pub mod proto;
pub mod server;

pub use proto::{
    decode_r, decode_t, encode_r, encode_t, rlerror, tcode, DirEntry, Fid, NineError, Qid,
    RMessage, Stat, Tag, TMessage, MAX_MSIZE,
};
pub use server::{EmptyTree, LocalServer, NodeId, StubTree, Tree, ROOT_NODE};
