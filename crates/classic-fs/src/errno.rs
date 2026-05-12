//! Linux errno constants the 9P2000.L server returns inside `Rlerror`.
//! Subset used by the read-mostly v1 server.

pub const EROFS: u32 = 30;
pub const EOPNOTSUPP: u32 = 95;
pub const EIO: u32 = 5;
pub const EINVAL: u32 = 22;
pub const ENOENT: u32 = 2;
pub const EBADF: u32 = 9;
pub const ENOTDIR: u32 = 20;
pub const EISDIR: u32 = 21;
pub const EINTR: u32 = 4;
