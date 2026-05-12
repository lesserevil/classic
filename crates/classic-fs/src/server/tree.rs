//! Tree abstraction the server walks. Real implementations (the
//! synthetic /dev /proc /svc /node tree from classic-b65; remote
//! bind-mounts from classic-4w3) plug in here. Tests use the small
//! in-memory `StubTree` below.
//!
//! Nodes are opaque `u64` ids the tree's owner assigns. Path 0 is the
//! root; subsequent ids are allocated however the tree wants.

use crate::errno;
use crate::proto::types::{DirEntry, Qid, Stat};

pub type NodeId = u64;
pub const ROOT_NODE: NodeId = 0;

pub trait Tree: Send + Sync {
    /// Look up `name` directly under `parent`. `None` if not found —
    /// the server maps that to `ENOENT`.
    fn walk_one(&self, parent: NodeId, name: &str) -> Option<NodeId>;

    /// `Qid` for `node`. Every node has a stable qid for the lifetime
    /// of the server.
    fn qid(&self, node: NodeId) -> Option<Qid>;

    /// Full stat. Server emits this in Rgetattr.
    fn stat(&self, node: NodeId) -> Option<Stat>;

    /// Read `count` bytes starting at `offset`. Empty Vec at EOF; never
    /// errors on "past EOF" — it just returns 0 bytes.
    fn read(&self, node: NodeId, offset: u64, count: u32) -> Result<Vec<u8>, u32>;

    /// Return directory entries starting at the offset cookie. Each
    /// `DirEntry.offset` is the cookie the client passes back to
    /// resume.
    fn readdir(&self, node: NodeId, offset: u64) -> Result<Vec<DirEntry>, u32>;

    /// `readlink` target string. `None` if the node isn't a symlink.
    fn readlink(&self, node: NodeId) -> Option<String>;

    /// True if the node is a directory. The server consults this to
    /// emit ENOTDIR / EISDIR.
    fn is_dir(&self, node: NodeId) -> bool;
}

/// Trivial empty tree — only the root, with no children. Useful for
/// server-loop tests that don't care about the underlying filesystem.
pub struct EmptyTree;

impl Tree for EmptyTree {
    fn walk_one(&self, _parent: NodeId, _name: &str) -> Option<NodeId> {
        None
    }
    fn qid(&self, node: NodeId) -> Option<Qid> {
        if node == ROOT_NODE {
            Some(Qid { type_: Qid::TYPE_DIR, version: 0, path: ROOT_NODE })
        } else {
            None
        }
    }
    fn stat(&self, node: NodeId) -> Option<Stat> {
        if node == ROOT_NODE {
            Some(Stat {
                qid: Qid { type_: Qid::TYPE_DIR, version: 0, path: ROOT_NODE },
                mode: 0o040555, // dr-xr-xr-x
                ..Stat::default()
            })
        } else {
            None
        }
    }
    fn read(&self, _node: NodeId, _offset: u64, _count: u32) -> Result<Vec<u8>, u32> {
        Err(errno::EISDIR)
    }
    fn readdir(&self, node: NodeId, _offset: u64) -> Result<Vec<DirEntry>, u32> {
        if node == ROOT_NODE { Ok(Vec::new()) } else { Err(errno::ENOENT) }
    }
    fn readlink(&self, _node: NodeId) -> Option<String> {
        None
    }
    fn is_dir(&self, node: NodeId) -> bool {
        node == ROOT_NODE
    }
}

/// Small in-memory tree for tests: a root directory and one
/// regular-file child whose content is the byte slice passed in.
pub struct StubTree {
    pub child_name: String,
    pub child_content: Vec<u8>,
    pub child_is_symlink: bool,
}

impl StubTree {
    pub const ROOT: NodeId = ROOT_NODE;
    pub const CHILD: NodeId = 1;

    pub fn file(name: &str, content: &[u8]) -> Self {
        Self {
            child_name: name.to_string(),
            child_content: content.to_vec(),
            child_is_symlink: false,
        }
    }
    pub fn symlink(name: &str, target: &str) -> Self {
        Self {
            child_name: name.to_string(),
            child_content: target.as_bytes().to_vec(),
            child_is_symlink: true,
        }
    }
}

impl Tree for StubTree {
    fn walk_one(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        if parent == Self::ROOT && name == self.child_name {
            Some(Self::CHILD)
        } else {
            None
        }
    }
    fn qid(&self, node: NodeId) -> Option<Qid> {
        match node {
            Self::ROOT => Some(Qid { type_: Qid::TYPE_DIR, version: 0, path: 0 }),
            Self::CHILD => Some(Qid {
                type_: if self.child_is_symlink { Qid::TYPE_SYMLINK } else { Qid::TYPE_FILE },
                version: 0,
                path: 1,
            }),
            _ => None,
        }
    }
    fn stat(&self, node: NodeId) -> Option<Stat> {
        match node {
            Self::ROOT => Some(Stat {
                qid: self.qid(Self::ROOT).unwrap(),
                mode: 0o040555,
                ..Stat::default()
            }),
            Self::CHILD => Some(Stat {
                qid: self.qid(Self::CHILD).unwrap(),
                mode: if self.child_is_symlink { 0o120777 } else { 0o100444 },
                size: self.child_content.len() as u64,
                ..Stat::default()
            }),
            _ => None,
        }
    }
    fn read(&self, node: NodeId, offset: u64, count: u32) -> Result<Vec<u8>, u32> {
        if node == Self::ROOT {
            return Err(errno::EISDIR);
        }
        if node != Self::CHILD {
            return Err(errno::ENOENT);
        }
        let off = offset as usize;
        if off >= self.child_content.len() {
            return Ok(Vec::new()); // EOF: zero-byte read, no error
        }
        let end = (off + count as usize).min(self.child_content.len());
        Ok(self.child_content[off..end].to_vec())
    }
    fn readdir(&self, node: NodeId, offset: u64) -> Result<Vec<DirEntry>, u32> {
        if node != Self::ROOT {
            return Err(errno::ENOTDIR);
        }
        if offset > 0 {
            return Ok(Vec::new()); // already streamed
        }
        Ok(vec![DirEntry {
            qid: self.qid(Self::CHILD).unwrap(),
            offset: 1,
            ty: if self.child_is_symlink { 10 } else { 8 }, // DT_LNK / DT_REG
            name: self.child_name.clone(),
        }])
    }
    fn readlink(&self, node: NodeId) -> Option<String> {
        if node == Self::CHILD && self.child_is_symlink {
            String::from_utf8(self.child_content.clone()).ok()
        } else {
            None
        }
    }
    fn is_dir(&self, node: NodeId) -> bool {
        node == Self::ROOT
    }
}
