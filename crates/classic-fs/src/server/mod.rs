//! Local 9P2000.L server loop. Consumes inbound T-messages and emits
//! R-messages (or `Rlerror` for the read-mostly v1 rejections).
//!
//! The actual filesystem content comes from a pluggable `Tree`
//! implementation (classic-b65 ships the production synthetic tree;
//! tests use `StubTree`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use classic_proto::FrameKind;

use crate::errno;
use crate::proto::codec::{decode_t, encode_r, RMessage, TMessage};
use crate::proto::types::{Fid, Qid, Tag};
use crate::proto::MAX_MSIZE;

pub mod tree;

pub use tree::{NodeId, Tree, EmptyTree, StubTree, ROOT_NODE};

/// Server-side handle for one fid. Tracks the underlying tree node
/// plus whether the client has opened it via Tlopen.
#[derive(Copy, Clone, Debug)]
struct FidHandle {
    node: NodeId,
    opened: bool,
}

/// Local 9P server. One per attach context; production wires one per
/// FUSE mount, tests instantiate directly.
pub struct LocalServer {
    tree: Arc<dyn Tree>,
    fids: Mutex<HashMap<Fid, FidHandle>>,
}

impl LocalServer {
    pub fn new(tree: Arc<dyn Tree>) -> Self {
        Self {
            tree,
            fids: Mutex::new(HashMap::new()),
        }
    }

    /// Drive one inbound 9P frame. Returns:
    /// - `Some((NineRsp, body))` for every parseable T-message
    ///   (including those that produce `Rlerror`).
    /// - `None` when the frame is unparseable or arrives on the wrong
    ///   FrameKind — the caller closes the connection.
    pub fn handle(&self, kind: FrameKind, body: &[u8]) -> Option<(FrameKind, Vec<u8>)> {
        if kind != FrameKind::NineReq {
            return None;
        }
        let msg = match decode_t(body) {
            Ok(m) => m,
            Err(_) => return None,
        };
        let tag = msg.tag();
        let response = self.dispatch(msg).unwrap_or_else(|ecode| RMessage::Lerror { tag, ecode });
        encode_r(&response).ok().map(|b| (FrameKind::NineRsp, b))
    }

    fn dispatch(&self, msg: TMessage) -> Result<RMessage, u32> {
        let tag = msg.tag();
        match msg {
            TMessage::Version { tag, msize, version } => {
                let clamped = msize.min(MAX_MSIZE);
                Ok(RMessage::Version {
                    tag,
                    msize: clamped,
                    version: if version == "9P2000.L" {
                        version
                    } else {
                        "unknown".into()
                    },
                })
            }
            TMessage::Attach { tag, fid, .. } => {
                // afid==NOFID and aname=='' always returns root in our
                // read-mostly v1 — we accept any auth-fid since
                // Tauth is rejected separately.
                self.insert_fid(fid, ROOT_NODE)?;
                let qid = self.tree.qid(ROOT_NODE).ok_or(errno::EIO)?;
                Ok(RMessage::Attach { tag, qid })
            }
            TMessage::Flush { tag, .. } => Ok(RMessage::Flush { tag }),
            TMessage::Walk { tag, fid, newfid, wnames } => {
                let start = self.lookup_node(fid)?;
                let mut wqids = Vec::with_capacity(wnames.len());
                let mut current = start;
                for name in &wnames {
                    let next = self.tree.walk_one(current, name).ok_or(errno::ENOENT)?;
                    wqids.push(self.tree.qid(next).ok_or(errno::ENOENT)?);
                    current = next;
                }
                // If walk succeeded fully OR walked zero names, install
                // newfid pointing at the final node. (Plan-9 semantics
                // also allow partial walks to fail-without-installing
                // newfid; we keep it simple here and require full success.)
                if wnames.is_empty() || wqids.len() == wnames.len() {
                    // newfid may equal fid (clone). Replace it.
                    self.insert_fid(newfid, current)?;
                }
                Ok(RMessage::Walk { tag, wqids })
            }
            TMessage::Lopen { tag, fid, flags } => {
                // Reject any write-mode open per FR EROFS.
                const O_ACCMODE: u32 = 3;
                const O_RDONLY: u32 = 0;
                let access = flags & O_ACCMODE;
                if access != O_RDONLY {
                    return Err(errno::EROFS);
                }
                let node = self.lookup_node(fid)?;
                let qid = self.tree.qid(node).ok_or(errno::EBADF)?;
                self.mark_opened(fid)?;
                Ok(RMessage::Lopen { tag, qid, iounit: 0 })
            }
            TMessage::Readlink { tag, fid } => {
                let node = self.lookup_node(fid)?;
                let target = self.tree.readlink(node).ok_or(errno::EINVAL)?;
                Ok(RMessage::Readlink { tag, target })
            }
            TMessage::Getattr { tag, fid, request_mask } => {
                let node = self.lookup_node(fid)?;
                let stat = self.tree.stat(node).ok_or(errno::EBADF)?;
                Ok(RMessage::Getattr { tag, valid: request_mask, stat })
            }
            TMessage::Readdir { tag, fid, offset, count: _ } => {
                let node = self.lookup_node(fid)?;
                if !self.tree.is_dir(node) {
                    return Err(errno::ENOTDIR);
                }
                let entries = self.tree.readdir(node, offset)?;
                // Encode entries inline — same layout the 9P2000.L wire
                // expects: qid(13) + offset(8) + type(1) + name[s].
                let mut buf = Vec::new();
                for e in entries {
                    buf.extend_from_slice(&[e.qid.type_]);
                    buf.extend_from_slice(&e.qid.version.to_le_bytes());
                    buf.extend_from_slice(&e.qid.path.to_le_bytes());
                    buf.extend_from_slice(&e.offset.to_le_bytes());
                    buf.push(e.ty);
                    buf.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
                    buf.extend_from_slice(e.name.as_bytes());
                }
                Ok(RMessage::Readdir { tag, data: buf })
            }
            TMessage::Fsync { tag, .. } => Ok(RMessage::Fsync { tag }),
            TMessage::Read { tag, fid, offset, count } => {
                let handle = self.fids.lock().unwrap();
                let h = *handle.get(&fid).ok_or(errno::EBADF)?;
                drop(handle);
                if !h.opened {
                    return Err(errno::EBADF);
                }
                let data = self.tree.read(h.node, offset, count)?;
                Ok(RMessage::Read { tag, data })
            }
            TMessage::Clunk { tag, fid } => {
                let mut handle = self.fids.lock().unwrap();
                handle.remove(&fid).ok_or(errno::EBADF)?;
                Ok(RMessage::Clunk { tag })
            }
            TMessage::Rejected { tag: _, code } => {
                use crate::proto::codec::tcode;
                let ecode = match code {
                    // Writes / mutating ops → EROFS.
                    tcode::TLCREATE
                    | tcode::TSYMLINK
                    | tcode::TMKNOD
                    | tcode::TRENAME
                    | tcode::TSETATTR
                    | tcode::TLINK
                    | tcode::TMKDIR
                    | tcode::TRENAMEAT
                    | tcode::TUNLINKAT
                    | tcode::TWRITE
                    | tcode::TREMOVE => errno::EROFS,
                    // Auth / xattr / lock / legacy → EOPNOTSUPP.
                    tcode::TAUTH
                    | tcode::TXATTRWALK
                    | tcode::TXATTRCREATE
                    | tcode::TLOCK
                    | tcode::TGETLOCK
                    | tcode::TSTAT
                    | tcode::TWSTAT => errno::EOPNOTSUPP,
                    _ => errno::EOPNOTSUPP,
                };
                Err(ecode)
            }
        }
        .map(|r| match &r {
            RMessage::Lerror { .. } => r.clone(), // already an Rlerror
            _ if tag.0 == 0 => r,                 // NOTAG passes through
            _ => r,
        })
    }

    fn insert_fid(&self, fid: Fid, node: NodeId) -> Result<(), u32> {
        self.fids
            .lock()
            .expect("fid table poisoned")
            .insert(fid, FidHandle { node, opened: false });
        Ok(())
    }

    fn lookup_node(&self, fid: Fid) -> Result<NodeId, u32> {
        self.fids
            .lock()
            .expect("fid table poisoned")
            .get(&fid)
            .map(|h| h.node)
            .ok_or(errno::EBADF)
    }

    fn mark_opened(&self, fid: Fid) -> Result<(), u32> {
        let mut g = self.fids.lock().expect("fid table poisoned");
        let h = g.get_mut(&fid).ok_or(errno::EBADF)?;
        h.opened = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::codec::{encode_t, tcode};
    use crate::proto::types::Fid as F;

    fn server_with(tree: Arc<dyn Tree>) -> LocalServer {
        LocalServer::new(tree)
    }

    fn round_trip(server: &LocalServer, t: TMessage) -> RMessage {
        let bytes = encode_t(&t).unwrap();
        let (kind, resp) = server
            .handle(FrameKind::NineReq, &bytes)
            .expect("handle returned None — connection should not close on valid frame");
        assert_eq!(kind, FrameKind::NineRsp);
        crate::proto::codec::decode_r(&resp).unwrap()
    }

    #[test]
    fn version_clamps_msize_and_echoes_protocol() {
        let server = server_with(Arc::new(EmptyTree));
        let r = round_trip(
            &server,
            TMessage::Version {
                tag: Tag(0xFFFF),
                msize: 1024 * 1024,
                version: "9P2000.L".into(),
            },
        );
        match r {
            RMessage::Version { tag, msize, version } => {
                assert_eq!(tag, Tag(0xFFFF));
                assert_eq!(msize, MAX_MSIZE);
                assert_eq!(version, "9P2000.L");
            }
            other => panic!("expected Rversion, got {other:?}"),
        }
    }

    #[test]
    fn attach_installs_root_fid() {
        let server = server_with(Arc::new(EmptyTree));
        let r = round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "user".into(),
                aname: "".into(),
                n_uname: 1000,
            },
        );
        match r {
            RMessage::Attach { qid, .. } => assert_eq!(qid.type_, Qid::TYPE_DIR),
            other => panic!("expected Rattach, got {other:?}"),
        }
    }

    #[test]
    fn walk_zero_names_clones_fid() {
        let server = server_with(Arc::new(EmptyTree));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        let r = round_trip(
            &server,
            TMessage::Walk { tag: Tag(2), fid: F(0), newfid: F(1), wnames: vec![] },
        );
        match r {
            RMessage::Walk { wqids, .. } => assert!(wqids.is_empty()),
            other => panic!("expected Rwalk, got {other:?}"),
        }
    }

    #[test]
    fn walk_unknown_child_returns_enoent() {
        let server = server_with(Arc::new(EmptyTree));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        let r = round_trip(
            &server,
            TMessage::Walk {
                tag: Tag(2),
                fid: F(0),
                newfid: F(1),
                wnames: vec!["nope".into()],
            },
        );
        match r {
            RMessage::Lerror { ecode, .. } => assert_eq!(ecode, errno::ENOENT),
            other => panic!("expected Rlerror, got {other:?}"),
        }
    }

    #[test]
    fn read_after_open_returns_file_content() {
        let server = server_with(Arc::new(StubTree::file("greeting", b"hello classic")));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        round_trip(
            &server,
            TMessage::Walk {
                tag: Tag(2),
                fid: F(0),
                newfid: F(1),
                wnames: vec!["greeting".into()],
            },
        );
        round_trip(
            &server,
            TMessage::Lopen { tag: Tag(3), fid: F(1), flags: 0 },
        );
        let r = round_trip(
            &server,
            TMessage::Read { tag: Tag(4), fid: F(1), offset: 0, count: 4096 },
        );
        match r {
            RMessage::Read { data, .. } => assert_eq!(&data, b"hello classic"),
            other => panic!("expected Rread, got {other:?}"),
        }
    }

    #[test]
    fn read_past_eof_returns_zero_bytes() {
        let server = server_with(Arc::new(StubTree::file("g", b"hi")));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        round_trip(
            &server,
            TMessage::Walk { tag: Tag(2), fid: F(0), newfid: F(1), wnames: vec!["g".into()] },
        );
        round_trip(
            &server,
            TMessage::Lopen { tag: Tag(3), fid: F(1), flags: 0 },
        );
        let r = round_trip(
            &server,
            TMessage::Read { tag: Tag(4), fid: F(1), offset: 100, count: 4096 },
        );
        match r {
            RMessage::Read { data, .. } => assert!(data.is_empty()),
            other => panic!("expected Rread, got {other:?}"),
        }
    }

    #[test]
    fn read_without_open_returns_ebadf() {
        let server = server_with(Arc::new(StubTree::file("g", b"x")));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        round_trip(
            &server,
            TMessage::Walk { tag: Tag(2), fid: F(0), newfid: F(1), wnames: vec!["g".into()] },
        );
        // Skip Tlopen.
        let r = round_trip(
            &server,
            TMessage::Read { tag: Tag(3), fid: F(1), offset: 0, count: 1 },
        );
        match r {
            RMessage::Lerror { ecode, .. } => assert_eq!(ecode, errno::EBADF),
            other => panic!("expected Rlerror EBADF, got {other:?}"),
        }
    }

    #[test]
    fn clunk_releases_fid_and_double_clunk_is_ebadf() {
        let server = server_with(Arc::new(EmptyTree));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        let r1 = round_trip(&server, TMessage::Clunk { tag: Tag(2), fid: F(0) });
        assert!(matches!(r1, RMessage::Clunk { .. }));
        let r2 = round_trip(&server, TMessage::Clunk { tag: Tag(3), fid: F(0) });
        match r2 {
            RMessage::Lerror { ecode, .. } => assert_eq!(ecode, errno::EBADF),
            other => panic!("expected Rlerror EBADF, got {other:?}"),
        }
    }

    #[test]
    fn rejected_writes_return_erofs() {
        let server = server_with(Arc::new(EmptyTree));
        for code in [
            tcode::TLCREATE,
            tcode::TSYMLINK,
            tcode::TMKNOD,
            tcode::TRENAME,
            tcode::TSETATTR,
            tcode::TLINK,
            tcode::TMKDIR,
            tcode::TRENAMEAT,
            tcode::TUNLINKAT,
            tcode::TWRITE,
            tcode::TREMOVE,
        ] {
            let t = TMessage::Rejected { tag: Tag(1), code };
            let r = round_trip(&server, t);
            match r {
                RMessage::Lerror { ecode, .. } => {
                    assert_eq!(ecode, errno::EROFS, "code {code} should map to EROFS");
                }
                other => panic!("expected Rlerror, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejected_auth_xattr_lock_legacy_return_eopnotsupp() {
        let server = server_with(Arc::new(EmptyTree));
        for code in [
            tcode::TAUTH,
            tcode::TXATTRWALK,
            tcode::TXATTRCREATE,
            tcode::TLOCK,
            tcode::TGETLOCK,
            tcode::TSTAT,
            tcode::TWSTAT,
        ] {
            let r = round_trip(&server, TMessage::Rejected { tag: Tag(1), code });
            match r {
                RMessage::Lerror { ecode, .. } => {
                    assert_eq!(
                        ecode, errno::EOPNOTSUPP,
                        "code {code} should map to EOPNOTSUPP"
                    );
                }
                other => panic!("expected Rlerror, got {other:?}"),
            }
        }
    }

    #[test]
    fn open_with_write_mode_returns_erofs() {
        let server = server_with(Arc::new(StubTree::file("g", b"x")));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        round_trip(
            &server,
            TMessage::Walk { tag: Tag(2), fid: F(0), newfid: F(1), wnames: vec!["g".into()] },
        );
        let r = round_trip(
            &server,
            TMessage::Lopen { tag: Tag(3), fid: F(1), flags: 2 /* O_RDWR */ },
        );
        match r {
            RMessage::Lerror { ecode, .. } => assert_eq!(ecode, errno::EROFS),
            other => panic!("expected EROFS, got {other:?}"),
        }
    }

    #[test]
    fn readlink_on_symlink_returns_target() {
        let server = server_with(Arc::new(StubTree::symlink(
            "linkme",
            "/target/path/here",
        )));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        round_trip(
            &server,
            TMessage::Walk {
                tag: Tag(2),
                fid: F(0),
                newfid: F(1),
                wnames: vec!["linkme".into()],
            },
        );
        let r = round_trip(&server, TMessage::Readlink { tag: Tag(3), fid: F(1) });
        match r {
            RMessage::Readlink { target, .. } => assert_eq!(target, "/target/path/here"),
            other => panic!("expected Rreadlink, got {other:?}"),
        }
    }

    #[test]
    fn getattr_emits_stat_for_fid() {
        let server = server_with(Arc::new(StubTree::file("g", b"hello")));
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(1),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        round_trip(
            &server,
            TMessage::Walk { tag: Tag(2), fid: F(0), newfid: F(1), wnames: vec!["g".into()] },
        );
        let r = round_trip(
            &server,
            TMessage::Getattr { tag: Tag(3), fid: F(1), request_mask: 0x7FF },
        );
        match r {
            RMessage::Getattr { stat, .. } => {
                assert_eq!(stat.size, 5);
                assert_eq!(stat.qid.type_, Qid::TYPE_FILE);
            }
            other => panic!("expected Rgetattr, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_frame_signals_close() {
        let server = server_with(Arc::new(EmptyTree));
        // Empty body — decode_t reads u8 then fails at u16.
        let result = server.handle(FrameKind::NineReq, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn wrong_frame_kind_returns_none() {
        let server = server_with(Arc::new(EmptyTree));
        // Pass a NineRsp body to the server's handle — it expects NineReq.
        let result = server.handle(FrameKind::NineRsp, &[100, 0, 0, 0, 0, 0, 0]);
        assert!(result.is_none());
    }

    #[test]
    fn flush_and_fsync_are_noops() {
        let server = server_with(Arc::new(EmptyTree));
        match round_trip(&server, TMessage::Flush { tag: Tag(1), oldtag: Tag(2) }) {
            RMessage::Flush { tag } => assert_eq!(tag, Tag(1)),
            other => panic!("{other:?}"),
        }
        // Fsync requires a valid fid in our impl since fsync(no-fid) is weird;
        // but our handler treats it as a no-op regardless. Just confirm
        // it doesn't error.
        round_trip(
            &server,
            TMessage::Attach {
                tag: Tag(2),
                fid: F(0),
                afid: F(u32::MAX),
                uname: "u".into(),
                aname: "".into(),
                n_uname: 0,
            },
        );
        match round_trip(&server, TMessage::Fsync { tag: Tag(3), fid: F(0) }) {
            RMessage::Fsync { tag } => assert_eq!(tag, Tag(3)),
            other => panic!("{other:?}"),
        }
    }
}
