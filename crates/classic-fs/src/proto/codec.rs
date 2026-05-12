//! Low-level 9P2000.L wire codec. Provides typed reader / writer
//! primitives (LE integers, length-prefixed strings, Qid) and the
//! per-message encoders/decoders for every op the v1 server
//! implements or explicitly rejects.

use crate::proto::types::{DirEntry, Fid, Qid, Stat, Tag};

// ---------- Reader / Writer helpers ----------

#[derive(Debug, thiserror::Error)]
pub enum NineError {
    #[error("unexpected end of message")]
    Eom,
    #[error("string too long: {0} bytes")]
    StringTooLong(usize),
    #[error("invalid utf-8 in 9P string")]
    Utf8,
    #[error("unknown message type {0}")]
    UnknownType(u8),
    #[error("Twalk wname count {0} exceeds 16")]
    TwalkTooManyNames(u16),
    #[error("Rwalk wqid count {0} exceeds 16")]
    RwalkTooManyQids(u16),
}

struct R<'a> {
    buf: &'a [u8],
    i: usize,
}
impl<'a> R<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, i: 0 }
    }
    fn need(&self, n: usize) -> Result<(), NineError> {
        if self.i + n > self.buf.len() { Err(NineError::Eom) } else { Ok(()) }
    }
    fn u8(&mut self) -> Result<u8, NineError> {
        self.need(1)?;
        let v = self.buf[self.i];
        self.i += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, NineError> {
        self.need(2)?;
        let v = u16::from_le_bytes(self.buf[self.i..self.i + 2].try_into().unwrap());
        self.i += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, NineError> {
        self.need(4)?;
        let v = u32::from_le_bytes(self.buf[self.i..self.i + 4].try_into().unwrap());
        self.i += 4;
        Ok(v)
    }
    fn u64(&mut self) -> Result<u64, NineError> {
        self.need(8)?;
        let v = u64::from_le_bytes(self.buf[self.i..self.i + 8].try_into().unwrap());
        self.i += 8;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, NineError> {
        let len = self.u16()? as usize;
        self.need(len)?;
        let s = std::str::from_utf8(&self.buf[self.i..self.i + len])
            .map_err(|_| NineError::Utf8)?
            .to_string();
        self.i += len;
        Ok(s)
    }
    fn qid(&mut self) -> Result<Qid, NineError> {
        let type_ = self.u8()?;
        let version = self.u32()?;
        let path = self.u64()?;
        Ok(Qid { type_, version, path })
    }
    fn data(&mut self, n: usize) -> Result<Vec<u8>, NineError> {
        self.need(n)?;
        let v = self.buf[self.i..self.i + n].to_vec();
        self.i += n;
        Ok(v)
    }
}

struct W {
    buf: Vec<u8>,
}
impl W {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn string(&mut self, s: &str) -> Result<(), NineError> {
        if s.len() > u16::MAX as usize {
            return Err(NineError::StringTooLong(s.len()));
        }
        self.u16(s.len() as u16);
        self.buf.extend_from_slice(s.as_bytes());
        Ok(())
    }
    fn qid(&mut self, q: Qid) {
        self.u8(q.type_);
        self.u32(q.version);
        self.u64(q.path);
    }
    fn data(&mut self, d: &[u8]) {
        self.buf.extend_from_slice(d);
    }
    fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

// ---------- T-message op codes ----------

/// Op-code constants for every 9P2000.L T-message the server inspects.
/// `R<op>` codes are `T<op> + 1` per spec. Numbers from
/// https://github.com/chaos/diod/blob/master/protocol.md.
pub mod tcode {
    pub const TLERROR: u8 = 6; // never sent by clients; reserved
    pub const RLERROR: u8 = 7;
    pub const TSTATFS: u8 = 8;
    pub const TLOPEN: u8 = 12;
    pub const TLCREATE: u8 = 14;
    pub const TSYMLINK: u8 = 16;
    pub const TMKNOD: u8 = 18;
    pub const TRENAME: u8 = 20;
    pub const TREADLINK: u8 = 22;
    pub const TGETATTR: u8 = 24;
    pub const TSETATTR: u8 = 26;
    pub const TXATTRWALK: u8 = 30;
    pub const TXATTRCREATE: u8 = 32;
    pub const TREADDIR: u8 = 40;
    pub const TFSYNC: u8 = 50;
    pub const TLOCK: u8 = 52;
    pub const TGETLOCK: u8 = 54;
    pub const TLINK: u8 = 70;
    pub const TMKDIR: u8 = 72;
    pub const TRENAMEAT: u8 = 74;
    pub const TUNLINKAT: u8 = 76;
    pub const TVERSION: u8 = 100;
    pub const TAUTH: u8 = 102;
    pub const TATTACH: u8 = 104;
    pub const TFLUSH: u8 = 108;
    pub const TWALK: u8 = 110;
    pub const TREAD: u8 = 116;
    pub const TWRITE: u8 = 118;
    pub const TCLUNK: u8 = 120;
    pub const TREMOVE: u8 = 122;
    pub const TSTAT: u8 = 124;
    pub const TWSTAT: u8 = 126;
}

// ---------- Implemented T-messages ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TMessage {
    Version { tag: Tag, msize: u32, version: String },
    Attach {
        tag: Tag,
        fid: Fid,
        afid: Fid,
        uname: String,
        aname: String,
        n_uname: u32,
    },
    Flush { tag: Tag, oldtag: Tag },
    Walk { tag: Tag, fid: Fid, newfid: Fid, wnames: Vec<String> },
    Lopen { tag: Tag, fid: Fid, flags: u32 },
    Readlink { tag: Tag, fid: Fid },
    Getattr { tag: Tag, fid: Fid, request_mask: u64 },
    Readdir { tag: Tag, fid: Fid, offset: u64, count: u32 },
    Fsync { tag: Tag, fid: Fid },
    Read { tag: Tag, fid: Fid, offset: u64, count: u32 },
    Clunk { tag: Tag, fid: Fid },
    /// Any T-message whose op is in the rejected set. The dispatcher
    /// emits `Rlerror{ecode}` with the appropriate errno without
    /// parsing the body.
    Rejected { tag: Tag, code: u8 },
}

impl TMessage {
    pub fn tag(&self) -> Tag {
        match self {
            TMessage::Version { tag, .. }
            | TMessage::Attach { tag, .. }
            | TMessage::Flush { tag, .. }
            | TMessage::Walk { tag, .. }
            | TMessage::Lopen { tag, .. }
            | TMessage::Readlink { tag, .. }
            | TMessage::Getattr { tag, .. }
            | TMessage::Readdir { tag, .. }
            | TMessage::Fsync { tag, .. }
            | TMessage::Read { tag, .. }
            | TMessage::Clunk { tag, .. }
            | TMessage::Rejected { tag, .. } => *tag,
        }
    }
}

const MAX_WALK_NAMES: u16 = 16;

pub fn decode_t(bytes: &[u8]) -> Result<TMessage, NineError> {
    let mut r = R::new(bytes);
    let code = r.u8()?;
    let tag_v = r.u16()?;
    let tag = Tag(tag_v);
    use tcode::*;
    match code {
        TVERSION => {
            let msize = r.u32()?;
            let version = r.string()?;
            Ok(TMessage::Version { tag, msize, version })
        }
        TATTACH => {
            let fid = Fid(r.u32()?);
            let afid = Fid(r.u32()?);
            let uname = r.string()?;
            let aname = r.string()?;
            let n_uname = r.u32()?;
            Ok(TMessage::Attach { tag, fid, afid, uname, aname, n_uname })
        }
        TFLUSH => Ok(TMessage::Flush { tag, oldtag: Tag(r.u16()?) }),
        TWALK => {
            let fid = Fid(r.u32()?);
            let newfid = Fid(r.u32()?);
            let nwname = r.u16()?;
            if nwname > MAX_WALK_NAMES {
                return Err(NineError::TwalkTooManyNames(nwname));
            }
            let mut wnames = Vec::with_capacity(nwname as usize);
            for _ in 0..nwname {
                wnames.push(r.string()?);
            }
            Ok(TMessage::Walk { tag, fid, newfid, wnames })
        }
        TLOPEN => Ok(TMessage::Lopen { tag, fid: Fid(r.u32()?), flags: r.u32()? }),
        TREADLINK => Ok(TMessage::Readlink { tag, fid: Fid(r.u32()?) }),
        TGETATTR => Ok(TMessage::Getattr {
            tag,
            fid: Fid(r.u32()?),
            request_mask: r.u64()?,
        }),
        TREADDIR => Ok(TMessage::Readdir {
            tag,
            fid: Fid(r.u32()?),
            offset: r.u64()?,
            count: r.u32()?,
        }),
        TFSYNC => Ok(TMessage::Fsync { tag, fid: Fid(r.u32()?) }),
        TREAD => Ok(TMessage::Read {
            tag,
            fid: Fid(r.u32()?),
            offset: r.u64()?,
            count: r.u32()?,
        }),
        TCLUNK => Ok(TMessage::Clunk { tag, fid: Fid(r.u32()?) }),
        // Rejected ops — recognize and surface with the type code so
        // the dispatcher can map to Rlerror{ecode}.
        TLCREATE | TSYMLINK | TMKNOD | TRENAME | TSETATTR | TLINK | TMKDIR | TRENAMEAT
        | TUNLINKAT | TWRITE | TREMOVE | TAUTH | TXATTRWALK | TXATTRCREATE | TLOCK
        | TGETLOCK | TSTAT | TWSTAT => Ok(TMessage::Rejected { tag, code }),
        other => Err(NineError::UnknownType(other)),
    }
}

pub fn encode_t(msg: &TMessage) -> Result<Vec<u8>, NineError> {
    use tcode::*;
    let mut w = W::new();
    match msg {
        TMessage::Version { tag, msize, version } => {
            w.u8(TVERSION);
            w.u16(tag.0);
            w.u32(*msize);
            w.string(version)?;
        }
        TMessage::Attach { tag, fid, afid, uname, aname, n_uname } => {
            w.u8(TATTACH);
            w.u16(tag.0);
            w.u32(fid.0);
            w.u32(afid.0);
            w.string(uname)?;
            w.string(aname)?;
            w.u32(*n_uname);
        }
        TMessage::Flush { tag, oldtag } => {
            w.u8(TFLUSH);
            w.u16(tag.0);
            w.u16(oldtag.0);
        }
        TMessage::Walk { tag, fid, newfid, wnames } => {
            if wnames.len() > MAX_WALK_NAMES as usize {
                return Err(NineError::TwalkTooManyNames(wnames.len() as u16));
            }
            w.u8(TWALK);
            w.u16(tag.0);
            w.u32(fid.0);
            w.u32(newfid.0);
            w.u16(wnames.len() as u16);
            for n in wnames {
                w.string(n)?;
            }
        }
        TMessage::Lopen { tag, fid, flags } => {
            w.u8(TLOPEN);
            w.u16(tag.0);
            w.u32(fid.0);
            w.u32(*flags);
        }
        TMessage::Readlink { tag, fid } => {
            w.u8(TREADLINK);
            w.u16(tag.0);
            w.u32(fid.0);
        }
        TMessage::Getattr { tag, fid, request_mask } => {
            w.u8(TGETATTR);
            w.u16(tag.0);
            w.u32(fid.0);
            w.u64(*request_mask);
        }
        TMessage::Readdir { tag, fid, offset, count } => {
            w.u8(TREADDIR);
            w.u16(tag.0);
            w.u32(fid.0);
            w.u64(*offset);
            w.u32(*count);
        }
        TMessage::Fsync { tag, fid } => {
            w.u8(TFSYNC);
            w.u16(tag.0);
            w.u32(fid.0);
        }
        TMessage::Read { tag, fid, offset, count } => {
            w.u8(TREAD);
            w.u16(tag.0);
            w.u32(fid.0);
            w.u64(*offset);
            w.u32(*count);
        }
        TMessage::Clunk { tag, fid } => {
            w.u8(TCLUNK);
            w.u16(tag.0);
            w.u32(fid.0);
        }
        TMessage::Rejected { tag, code } => {
            // Rejected re-encode just writes the bare header; the body
            // can't be reproduced from the typed form. This is mostly
            // useful for round-tripping in tests.
            w.u8(*code);
            w.u16(tag.0);
        }
    }
    Ok(w.into_vec())
}

// ---------- R-messages ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RMessage {
    Lerror { tag: Tag, ecode: u32 },
    Version { tag: Tag, msize: u32, version: String },
    Attach { tag: Tag, qid: Qid },
    Flush { tag: Tag },
    Walk { tag: Tag, wqids: Vec<Qid> },
    Lopen { tag: Tag, qid: Qid, iounit: u32 },
    Readlink { tag: Tag, target: String },
    Getattr { tag: Tag, valid: u64, stat: Stat },
    Readdir { tag: Tag, data: Vec<u8> },
    Fsync { tag: Tag },
    Read { tag: Tag, data: Vec<u8> },
    Clunk { tag: Tag },
}

impl RMessage {
    pub fn tag(&self) -> Tag {
        match self {
            RMessage::Lerror { tag, .. }
            | RMessage::Version { tag, .. }
            | RMessage::Attach { tag, .. }
            | RMessage::Flush { tag }
            | RMessage::Walk { tag, .. }
            | RMessage::Lopen { tag, .. }
            | RMessage::Readlink { tag, .. }
            | RMessage::Getattr { tag, .. }
            | RMessage::Readdir { tag, .. }
            | RMessage::Fsync { tag }
            | RMessage::Read { tag, .. }
            | RMessage::Clunk { tag } => *tag,
        }
    }
}

pub fn decode_r(bytes: &[u8]) -> Result<RMessage, NineError> {
    let mut r = R::new(bytes);
    let code = r.u8()?;
    let tag_v = r.u16()?;
    let tag = Tag(tag_v);
    use tcode::*;
    match code {
        RLERROR => Ok(RMessage::Lerror { tag, ecode: r.u32()? }),
        c if c == TVERSION + 1 => {
            let msize = r.u32()?;
            let version = r.string()?;
            Ok(RMessage::Version { tag, msize, version })
        }
        c if c == TATTACH + 1 => Ok(RMessage::Attach { tag, qid: r.qid()? }),
        c if c == TFLUSH + 1 => Ok(RMessage::Flush { tag }),
        c if c == TWALK + 1 => {
            let nwqid = r.u16()?;
            if nwqid > MAX_WALK_NAMES {
                return Err(NineError::RwalkTooManyQids(nwqid));
            }
            let mut wqids = Vec::with_capacity(nwqid as usize);
            for _ in 0..nwqid {
                wqids.push(r.qid()?);
            }
            Ok(RMessage::Walk { tag, wqids })
        }
        c if c == TLOPEN + 1 => Ok(RMessage::Lopen {
            tag,
            qid: r.qid()?,
            iounit: r.u32()?,
        }),
        c if c == TREADLINK + 1 => Ok(RMessage::Readlink {
            tag,
            target: r.string()?,
        }),
        c if c == TGETATTR + 1 => {
            let valid = r.u64()?;
            let mut s = Stat::default();
            s.qid = r.qid()?;
            s.mode = r.u32()?;
            s.uid = r.u32()?;
            s.gid = r.u32()?;
            s.nlink = r.u64()?;
            s.rdev = r.u64()?;
            s.size = r.u64()?;
            s.blksize = r.u64()?;
            s.blocks = r.u64()?;
            s.atime_sec = r.u64()?;
            s.atime_nsec = r.u64()?;
            s.mtime_sec = r.u64()?;
            s.mtime_nsec = r.u64()?;
            s.ctime_sec = r.u64()?;
            s.ctime_nsec = r.u64()?;
            s.btime_sec = r.u64()?;
            s.btime_nsec = r.u64()?;
            s.gen = r.u64()?;
            s.data_version = r.u64()?;
            Ok(RMessage::Getattr { tag, valid, stat: s })
        }
        c if c == TREADDIR + 1 => {
            let count = r.u32()? as usize;
            let data = r.data(count)?;
            Ok(RMessage::Readdir { tag, data })
        }
        c if c == TFSYNC + 1 => Ok(RMessage::Fsync { tag }),
        c if c == TREAD + 1 => {
            let count = r.u32()? as usize;
            let data = r.data(count)?;
            Ok(RMessage::Read { tag, data })
        }
        c if c == TCLUNK + 1 => Ok(RMessage::Clunk { tag }),
        other => Err(NineError::UnknownType(other)),
    }
}

pub fn encode_r(msg: &RMessage) -> Result<Vec<u8>, NineError> {
    use tcode::*;
    let mut w = W::new();
    match msg {
        RMessage::Lerror { tag, ecode } => {
            w.u8(RLERROR);
            w.u16(tag.0);
            w.u32(*ecode);
        }
        RMessage::Version { tag, msize, version } => {
            w.u8(TVERSION + 1);
            w.u16(tag.0);
            w.u32(*msize);
            w.string(version)?;
        }
        RMessage::Attach { tag, qid } => {
            w.u8(TATTACH + 1);
            w.u16(tag.0);
            w.qid(*qid);
        }
        RMessage::Flush { tag } => {
            w.u8(TFLUSH + 1);
            w.u16(tag.0);
        }
        RMessage::Walk { tag, wqids } => {
            if wqids.len() > MAX_WALK_NAMES as usize {
                return Err(NineError::RwalkTooManyQids(wqids.len() as u16));
            }
            w.u8(TWALK + 1);
            w.u16(tag.0);
            w.u16(wqids.len() as u16);
            for q in wqids {
                w.qid(*q);
            }
        }
        RMessage::Lopen { tag, qid, iounit } => {
            w.u8(TLOPEN + 1);
            w.u16(tag.0);
            w.qid(*qid);
            w.u32(*iounit);
        }
        RMessage::Readlink { tag, target } => {
            w.u8(TREADLINK + 1);
            w.u16(tag.0);
            w.string(target)?;
        }
        RMessage::Getattr { tag, valid, stat } => {
            w.u8(TGETATTR + 1);
            w.u16(tag.0);
            w.u64(*valid);
            w.qid(stat.qid);
            w.u32(stat.mode);
            w.u32(stat.uid);
            w.u32(stat.gid);
            w.u64(stat.nlink);
            w.u64(stat.rdev);
            w.u64(stat.size);
            w.u64(stat.blksize);
            w.u64(stat.blocks);
            w.u64(stat.atime_sec);
            w.u64(stat.atime_nsec);
            w.u64(stat.mtime_sec);
            w.u64(stat.mtime_nsec);
            w.u64(stat.ctime_sec);
            w.u64(stat.ctime_nsec);
            w.u64(stat.btime_sec);
            w.u64(stat.btime_nsec);
            w.u64(stat.gen);
            w.u64(stat.data_version);
        }
        RMessage::Readdir { tag, data } => {
            w.u8(TREADDIR + 1);
            w.u16(tag.0);
            w.u32(data.len() as u32);
            w.data(data);
        }
        RMessage::Fsync { tag } => {
            w.u8(TFSYNC + 1);
            w.u16(tag.0);
        }
        RMessage::Read { tag, data } => {
            w.u8(TREAD + 1);
            w.u16(tag.0);
            w.u32(data.len() as u32);
            w.data(data);
        }
        RMessage::Clunk { tag } => {
            w.u8(TCLUNK + 1);
            w.u16(tag.0);
        }
    }
    Ok(w.into_vec())
}

/// 9P version negotiation cap. Clients above this clamp to it.
pub const MAX_MSIZE: u32 = 64 * 1024;

/// Convenience: build a per-op `Rlerror` with the given errno.
pub fn rlerror(tag: Tag, ecode: u32) -> RMessage {
    RMessage::Lerror { tag, ecode }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_t(msg: TMessage) {
        let bytes = encode_t(&msg).unwrap();
        let back = decode_t(&bytes).unwrap();
        assert_eq!(back, msg);
    }
    fn rt_r(msg: RMessage) {
        let bytes = encode_r(&msg).unwrap();
        let back = decode_r(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn version_roundtrip() {
        rt_t(TMessage::Version { tag: Tag(1), msize: MAX_MSIZE, version: "9P2000.L".into() });
        rt_r(RMessage::Version { tag: Tag(1), msize: MAX_MSIZE, version: "9P2000.L".into() });
    }

    #[test]
    fn attach_roundtrip() {
        rt_t(TMessage::Attach {
            tag: Tag(2),
            fid: Fid(0),
            afid: Fid(u32::MAX),
            uname: "user".into(),
            aname: "tree".into(),
            n_uname: 1000,
        });
    }

    #[test]
    fn walk_roundtrip_all_sizes() {
        for n in 0..=16usize {
            let names: Vec<String> = (0..n).map(|i| format!("seg{i}")).collect();
            rt_t(TMessage::Walk {
                tag: Tag(3),
                fid: Fid(1),
                newfid: Fid(2),
                wnames: names.clone(),
            });
            let qids: Vec<Qid> = (0..n)
                .map(|i| Qid { type_: Qid::TYPE_DIR, version: i as u32, path: i as u64 })
                .collect();
            rt_r(RMessage::Walk { tag: Tag(3), wqids: qids });
        }
    }

    #[test]
    fn walk_too_many_names_errors_on_decode() {
        let mut w = W::new();
        w.u8(tcode::TWALK);
        w.u16(0);
        w.u32(0); // fid
        w.u32(1); // newfid
        w.u16(17); // 17 names — too many
        for i in 0..17 {
            w.string(&format!("seg{i}")).unwrap();
        }
        let err = decode_t(&w.into_vec()).unwrap_err();
        assert!(matches!(err, NineError::TwalkTooManyNames(17)));
    }

    #[test]
    fn lopen_readlink_getattr_readdir_roundtrip() {
        rt_t(TMessage::Lopen { tag: Tag(1), fid: Fid(5), flags: 0 });
        rt_t(TMessage::Readlink { tag: Tag(1), fid: Fid(7) });
        rt_t(TMessage::Getattr { tag: Tag(1), fid: Fid(7), request_mask: 0x7FF });
        rt_t(TMessage::Readdir { tag: Tag(1), fid: Fid(7), offset: 0, count: 8192 });
        rt_r(RMessage::Lopen {
            tag: Tag(1),
            qid: Qid { type_: Qid::TYPE_FILE, version: 1, path: 42 },
            iounit: 4096,
        });
        rt_r(RMessage::Readlink { tag: Tag(1), target: "/run/classic/ns/0/dev/gpu/0".into() });
        rt_r(RMessage::Getattr {
            tag: Tag(1),
            valid: 0x7FF,
            stat: Stat {
                qid: Qid { type_: Qid::TYPE_FILE, version: 1, path: 99 },
                mode: 0o444,
                uid: 1000,
                gid: 1000,
                nlink: 1,
                size: 16,
                ..Stat::default()
            },
        });
        rt_r(RMessage::Readdir { tag: Tag(1), data: vec![0xAA; 256] });
    }

    #[test]
    fn read_fsync_clunk_flush_roundtrip() {
        rt_t(TMessage::Read { tag: Tag(1), fid: Fid(3), offset: 100, count: 4096 });
        rt_t(TMessage::Fsync { tag: Tag(1), fid: Fid(3) });
        rt_t(TMessage::Clunk { tag: Tag(1), fid: Fid(3) });
        rt_t(TMessage::Flush { tag: Tag(1), oldtag: Tag(99) });
        rt_r(RMessage::Read { tag: Tag(1), data: b"hello".to_vec() });
        rt_r(RMessage::Fsync { tag: Tag(1) });
        rt_r(RMessage::Clunk { tag: Tag(1) });
        rt_r(RMessage::Flush { tag: Tag(1) });
    }

    #[test]
    fn rlerror_roundtrip() {
        rt_r(RMessage::Lerror { tag: Tag(1), ecode: crate::errno::EROFS });
        rt_r(RMessage::Lerror { tag: Tag(2), ecode: crate::errno::EOPNOTSUPP });
        rt_r(RMessage::Lerror { tag: Tag(3), ecode: crate::errno::ENOENT });
    }

    #[test]
    fn rejected_ops_decode_with_code() {
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
            tcode::TAUTH,
            tcode::TXATTRWALK,
            tcode::TXATTRCREATE,
            tcode::TLOCK,
            tcode::TGETLOCK,
            tcode::TSTAT,
            tcode::TWSTAT,
        ] {
            let mut w = W::new();
            w.u8(code);
            w.u16(7);
            // Trailing junk: a rejected message body. The decoder
            // shouldn't care — we just want the type code + tag.
            w.u32(0xDEADBEEF);
            let msg = decode_t(&w.into_vec()).unwrap();
            match msg {
                TMessage::Rejected { tag, code: c } => {
                    assert_eq!(tag, Tag(7));
                    assert_eq!(c, code);
                }
                other => panic!("expected Rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_op_errors() {
        let mut w = W::new();
        w.u8(200); // not a real op
        w.u16(0);
        let err = decode_t(&w.into_vec()).unwrap_err();
        assert!(matches!(err, NineError::UnknownType(200)));
    }

    #[test]
    fn truncated_message_errors() {
        let err = decode_t(&[tcode::TVERSION, 0x01]).unwrap_err();
        assert!(matches!(err, NineError::Eom));
    }
}
