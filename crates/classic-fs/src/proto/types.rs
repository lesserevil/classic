//! 9P2000.L primitive types reused across message encodings.

/// File identifier — opaque handle exchanged between client and server.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fid(pub u32);

/// Message tag — pairs T and R messages.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tag(pub u16);

/// Qid: a 13-byte server-side identity for a file. `path` is the
/// stable id used by clients to detect aliases.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Qid {
    /// Qid type bitfield (file/dir/append/excl/auth/tmp/etc.).
    pub type_: u8,
    pub version: u32,
    pub path: u64,
}

impl Qid {
    pub const TYPE_FILE: u8 = 0x00;
    pub const TYPE_DIR: u8 = 0x80;
    pub const TYPE_SYMLINK: u8 = 0x02;
}

/// Single directory-entry record emitted by `Rreaddir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub qid: Qid,
    /// Cookie the client passes back on the next `Treaddir` to resume.
    pub offset: u64,
    /// Linux DT_* type code (DT_DIR=4, DT_REG=8, DT_LNK=10, ...).
    pub ty: u8,
    pub name: String,
}

/// Subset of the 9P2000.L getattr response we emit. v1's read-mostly
/// server fills in mode/uid/gid/size/atime/mtime/ctime; the rest are
/// zeroed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Stat {
    pub qid: Qid,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub atime_sec: u64,
    pub atime_nsec: u64,
    pub mtime_sec: u64,
    pub mtime_nsec: u64,
    pub ctime_sec: u64,
    pub ctime_nsec: u64,
    pub btime_sec: u64,
    pub btime_nsec: u64,
    pub gen: u64,
    pub data_version: u64,
}

impl Default for Qid {
    fn default() -> Self {
        Qid { type_: 0, version: 0, path: 0 }
    }
}
