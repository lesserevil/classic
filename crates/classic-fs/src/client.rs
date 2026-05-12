//! `RemoteClient` — outbound 9P2000.L over Classic frames. Speaks to a
//! remote daemon's `LocalServer` via the per-peer TCP connection from
//! plan 01. Tag-multiplexed: each request gets a unique `Tag` and the
//! response routes back to the awaiting future.
//!
//! This commit lands the pure-Rust client + tag pool; classic-node's
//! per-peer wiring (route inbound `0x0400` to `LocalServer::handle`,
//! route inbound `0x0401` back to the awaiting client by tag) lives in
//! the integration follow-up.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use classic_proto::Frame;
use tokio::sync::oneshot;

use crate::errno;
use crate::proto::codec::{decode_r, encode_t, RMessage, TMessage};
use crate::proto::types::{DirEntry, Fid, Qid, Stat, Tag};
use crate::proto::MAX_MSIZE;

/// `NOFID` per 9P spec — passed as `afid` when no auth fid is in use.
pub const NOFID: u32 = u32::MAX;

/// Async error mapped from a `RemoteClient` call. Pinned to numeric
/// errno values so the FUSE bridge can re-export them to the kernel
/// without a layer translation.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("remote returned Rlerror with errno {0}")]
    Errno(u32),
    #[error("transport: {0}")]
    Transport(String),
    #[error("local protocol error: {0}")]
    Protocol(String),
}

impl RemoteError {
    pub fn errno(&self) -> u32 {
        match self {
            RemoteError::Errno(e) => *e,
            RemoteError::Transport(_) => errno::EIO,
            RemoteError::Protocol(_) => errno::EIO,
        }
    }
}

/// Transport this client writes T-message frames onto. The daemon-side
/// wiring routes the response 0x0401 frame back via `Transport`'s
/// inbound channel so the client's pending-request map can resolve.
pub trait Transport: Send + Sync {
    /// Send a `Frame { kind: NineReq, payload }` onto the wire. The
    /// daemon-side adapter wraps a peer's outbound mpsc.
    fn send(&self, frame: Frame) -> Result<(), RemoteError>;
}

struct Pending {
    waiters: Mutex<HashMap<Tag, oneshot::Sender<RMessage>>>,
}

impl Pending {
    fn new() -> Self {
        Self { waiters: Mutex::new(HashMap::new()) }
    }
    fn register(&self, tag: Tag) -> oneshot::Receiver<RMessage> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().expect("pending poisoned").insert(tag, tx);
        rx
    }
    fn complete(&self, msg: RMessage) {
        let tag = msg.tag();
        if let Some(tx) = self.waiters.lock().expect("pending poisoned").remove(&tag) {
            let _ = tx.send(msg);
        }
    }
    fn cancel_all_with_eio(&self) {
        let mut waiters = self.waiters.lock().expect("pending poisoned");
        for (tag, tx) in waiters.drain() {
            let _ = tx.send(RMessage::Lerror { tag, ecode: errno::EIO });
        }
    }
}

/// Async 9P2000.L client targeting one remote daemon. Tag-multiplexed:
/// every method allocates a fresh `Tag` for the duration of the call.
pub struct RemoteClient {
    transport: Arc<dyn Transport>,
    pending: Arc<Pending>,
    next_tag: AtomicU16,
    next_fid: AtomicU32,
}

impl RemoteClient {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            pending: Arc::new(Pending::new()),
            next_tag: AtomicU16::new(1),
            next_fid: AtomicU32::new(1),
        }
    }

    /// The daemon-side adapter feeds inbound `NineRsp` frames here so
    /// the client can route responses back to their awaiting tag.
    pub fn deliver_response(&self, frame: &Frame) {
        match decode_r(&frame.payload) {
            Ok(msg) => self.pending.complete(msg),
            Err(e) => tracing::warn!(error = %e, "dropping malformed NineRsp"),
        }
    }

    /// Force every in-flight call to resolve as EIO. Called when the
    /// underlying peer transport drops.
    pub fn fail_all_in_flight(&self) {
        self.pending.cancel_all_with_eio();
    }

    fn alloc_tag(&self) -> Tag {
        // Tag 0xFFFF is NOTAG in 9P. Skip it on wrap.
        loop {
            let t = self.next_tag.fetch_add(1, Ordering::Relaxed);
            if t != 0xFFFF {
                return Tag(t);
            }
        }
    }

    fn alloc_fid(&self) -> Fid {
        Fid(self.next_fid.fetch_add(1, Ordering::Relaxed))
    }

    async fn request(&self, msg: TMessage) -> Result<RMessage, RemoteError> {
        let tag = msg.tag();
        let rx = self.pending.register(tag);
        let body = encode_t(&msg).map_err(|e| RemoteError::Protocol(format!("encode: {e}")))?;
        let frame = Frame::new(
            classic_proto::FrameKind::NineReq as u16,
            body.into(),
        );
        self.transport.send(frame)?;
        match rx.await {
            Ok(RMessage::Lerror { ecode, .. }) => Err(RemoteError::Errno(ecode)),
            Ok(r) => Ok(r),
            Err(_) => Err(RemoteError::Transport("response channel dropped".into())),
        }
    }

    pub async fn version(&self) -> Result<u32, RemoteError> {
        let tag = self.alloc_tag();
        let resp = self
            .request(TMessage::Version {
                tag,
                msize: MAX_MSIZE,
                version: "9P2000.L".into(),
            })
            .await?;
        match resp {
            RMessage::Version { msize, .. } => Ok(msize),
            other => Err(RemoteError::Protocol(format!("expected Rversion, got {other:?}"))),
        }
    }

    pub async fn attach(&self, aname: &str) -> Result<Fid, RemoteError> {
        let tag = self.alloc_tag();
        let fid = self.alloc_fid();
        let resp = self
            .request(TMessage::Attach {
                tag,
                fid,
                afid: Fid(NOFID),
                uname: "".into(),
                aname: aname.into(),
                n_uname: 0,
            })
            .await?;
        match resp {
            RMessage::Attach { .. } => Ok(fid),
            other => Err(RemoteError::Protocol(format!("expected Rattach, got {other:?}"))),
        }
    }

    pub async fn walk(&self, fid: Fid, names: &[&str]) -> Result<Fid, RemoteError> {
        let tag = self.alloc_tag();
        let newfid = self.alloc_fid();
        let resp = self
            .request(TMessage::Walk {
                tag,
                fid,
                newfid,
                wnames: names.iter().map(|s| s.to_string()).collect(),
            })
            .await?;
        match resp {
            RMessage::Walk { .. } => Ok(newfid),
            other => Err(RemoteError::Protocol(format!("expected Rwalk, got {other:?}"))),
        }
    }

    pub async fn open(&self, fid: Fid, flags: u32) -> Result<Qid, RemoteError> {
        let tag = self.alloc_tag();
        let resp = self.request(TMessage::Lopen { tag, fid, flags }).await?;
        match resp {
            RMessage::Lopen { qid, .. } => Ok(qid),
            other => Err(RemoteError::Protocol(format!("expected Rlopen, got {other:?}"))),
        }
    }

    pub async fn read(&self, fid: Fid, offset: u64, count: u32) -> Result<Vec<u8>, RemoteError> {
        let tag = self.alloc_tag();
        let resp = self
            .request(TMessage::Read { tag, fid, offset, count })
            .await?;
        match resp {
            RMessage::Read { data, .. } => Ok(data),
            other => Err(RemoteError::Protocol(format!("expected Rread, got {other:?}"))),
        }
    }

    pub async fn readdir(
        &self,
        fid: Fid,
        offset: u64,
        count: u32,
    ) -> Result<Vec<DirEntry>, RemoteError> {
        let tag = self.alloc_tag();
        let resp = self
            .request(TMessage::Readdir { tag, fid, offset, count })
            .await?;
        match resp {
            RMessage::Readdir { data, .. } => parse_readdir_payload(&data),
            other => Err(RemoteError::Protocol(format!("expected Rreaddir, got {other:?}"))),
        }
    }

    pub async fn getattr(&self, fid: Fid) -> Result<Stat, RemoteError> {
        let tag = self.alloc_tag();
        let resp = self
            .request(TMessage::Getattr {
                tag,
                fid,
                request_mask: 0x7FF,
            })
            .await?;
        match resp {
            RMessage::Getattr { stat, .. } => Ok(stat),
            other => Err(RemoteError::Protocol(format!("expected Rgetattr, got {other:?}"))),
        }
    }

    pub async fn readlink(&self, fid: Fid) -> Result<String, RemoteError> {
        let tag = self.alloc_tag();
        let resp = self.request(TMessage::Readlink { tag, fid }).await?;
        match resp {
            RMessage::Readlink { target, .. } => Ok(target),
            other => Err(RemoteError::Protocol(format!("expected Rreadlink, got {other:?}"))),
        }
    }

    pub async fn clunk(&self, fid: Fid) -> Result<(), RemoteError> {
        let tag = self.alloc_tag();
        let resp = self.request(TMessage::Clunk { tag, fid }).await?;
        match resp {
            RMessage::Clunk { .. } => Ok(()),
            other => Err(RemoteError::Protocol(format!("expected Rclunk, got {other:?}"))),
        }
    }
}

/// Decode a 9P2000.L Rreaddir data block into a `Vec<DirEntry>`.
/// Layout: each entry is `qid(13) + offset(8) + type(1) + name[s]`.
fn parse_readdir_payload(buf: &[u8]) -> Result<Vec<DirEntry>, RemoteError> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if i + 13 + 8 + 1 + 2 > buf.len() {
            return Err(RemoteError::Protocol("truncated Rreaddir entry".into()));
        }
        let type_ = buf[i];
        i += 1;
        let version = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        i += 4;
        let path = u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
        i += 8;
        let offset = u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
        i += 8;
        let ty = buf[i];
        i += 1;
        let name_len = u16::from_le_bytes(buf[i..i + 2].try_into().unwrap()) as usize;
        i += 2;
        if i + name_len > buf.len() {
            return Err(RemoteError::Protocol("truncated Rreaddir name".into()));
        }
        let name = std::str::from_utf8(&buf[i..i + name_len])
            .map_err(|_| RemoteError::Protocol("non-utf8 name".into()))?
            .to_string();
        i += name_len;
        out.push(DirEntry {
            qid: Qid { type_, version, path },
            offset,
            ty,
            name,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::proto::codec::{decode_t, encode_r};
    use crate::server::tree::EmptyTree;
    use crate::server::LocalServer;

    /// Loopback transport: forwards each NineReq through a server
    /// instance and delivers the response back to a registered client.
    struct LoopbackTransport {
        server: LocalServer,
        client: Mutex<Option<Arc<RemoteClient>>>,
    }
    impl LoopbackTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                server: LocalServer::new(Arc::new(EmptyTree)),
                client: Mutex::new(None),
            })
        }
        fn attach_client(&self, client: Arc<RemoteClient>) {
            *self.client.lock().unwrap() = Some(client);
        }
    }
    impl Transport for LoopbackTransport {
        fn send(&self, frame: Frame) -> Result<(), RemoteError> {
            // Server expects a NineReq with the T-message body.
            let (kind, body) = self
                .server
                .handle(classic_proto::FrameKind::NineReq, &frame.payload)
                .ok_or_else(|| RemoteError::Protocol("server closed connection".into()))?;
            let resp_frame = Frame::new(kind as u16, body.into());
            if let Some(client) = self.client.lock().unwrap().clone() {
                client.deliver_response(&resp_frame);
            }
            Ok(())
        }
    }

    /// Variant that uses a StubTree so reads can return content.
    struct StubLoopback {
        server: LocalServer,
        client: Mutex<Option<Arc<RemoteClient>>>,
    }
    impl StubLoopback {
        fn new(name: &str, content: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                server: LocalServer::new(Arc::new(crate::server::StubTree::file(name, content))),
                client: Mutex::new(None),
            })
        }
        fn attach_client(&self, client: Arc<RemoteClient>) {
            *self.client.lock().unwrap() = Some(client);
        }
    }
    impl Transport for StubLoopback {
        fn send(&self, frame: Frame) -> Result<(), RemoteError> {
            let (kind, body) = self
                .server
                .handle(classic_proto::FrameKind::NineReq, &frame.payload)
                .ok_or_else(|| RemoteError::Protocol("server closed connection".into()))?;
            let resp_frame = Frame::new(kind as u16, body.into());
            if let Some(client) = self.client.lock().unwrap().clone() {
                client.deliver_response(&resp_frame);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn version_negotiates_msize_clamp() {
        let transport = LoopbackTransport::new();
        let client = Arc::new(RemoteClient::new(transport.clone()));
        transport.attach_client(client.clone());
        let msize = client.version().await.unwrap();
        assert_eq!(msize, MAX_MSIZE);
    }

    #[tokio::test]
    async fn attach_clunk_roundtrip() {
        let transport = LoopbackTransport::new();
        let client = Arc::new(RemoteClient::new(transport.clone()));
        transport.attach_client(client.clone());
        let fid = client.attach("").await.unwrap();
        assert_eq!(fid.0, 1);
        client.clunk(fid).await.unwrap();
        // Double clunk -> EBADF from the server.
        let err = client.clunk(fid).await.unwrap_err();
        assert_eq!(err.errno(), errno::EBADF);
    }

    #[tokio::test]
    async fn walk_unknown_returns_enoent() {
        let transport = LoopbackTransport::new();
        let client = Arc::new(RemoteClient::new(transport.clone()));
        transport.attach_client(client.clone());
        let root = client.attach("").await.unwrap();
        let err = client.walk(root, &["nope"]).await.unwrap_err();
        assert_eq!(err.errno(), errno::ENOENT);
    }

    #[tokio::test]
    async fn read_after_walk_open_returns_content() {
        let transport = StubLoopback::new("greeting", b"hi from remote");
        let client = Arc::new(RemoteClient::new(transport.clone()));
        transport.attach_client(client.clone());
        let root = client.attach("").await.unwrap();
        let file = client.walk(root, &["greeting"]).await.unwrap();
        client.open(file, 0).await.unwrap();
        let data = client.read(file, 0, 4096).await.unwrap();
        assert_eq!(&data, b"hi from remote");
    }

    #[tokio::test]
    async fn fail_all_in_flight_yields_eio() {
        let transport = LoopbackTransport::new();
        let client = Arc::new(RemoteClient::new(transport.clone()));
        transport.attach_client(client.clone());

        // Build a pending request manually — register a tag but don't
        // run the loopback for it. Then trip fail_all_in_flight().
        let tag = client.alloc_tag();
        let rx = client.pending.register(tag);
        let waiter = tokio::spawn(async move {
            match rx.await {
                Ok(RMessage::Lerror { ecode, .. }) => ecode,
                _ => 0,
            }
        });
        client.fail_all_in_flight();
        let ecode = waiter.await.unwrap();
        assert_eq!(ecode, errno::EIO);
    }

    #[test]
    fn readdir_payload_parses_one_entry() {
        // Build a Rreaddir body the server would emit: qid(13) +
        // offset(8) + ty(1) + name-string.
        let mut buf = Vec::new();
        buf.push(Qid::TYPE_FILE);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.push(8); // DT_REG
        let name = b"foo";
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name);

        let entries = parse_readdir_payload(&buf).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "foo");
        assert_eq!(entries[0].qid.type_, Qid::TYPE_FILE);
        assert_eq!(entries[0].ty, 8);
    }

    #[test]
    fn readdir_payload_rejects_truncated_input() {
        let buf = vec![0u8; 5]; // way too short
        let err = parse_readdir_payload(&buf).unwrap_err();
        assert!(matches!(err, RemoteError::Protocol(_)));
    }

    // Suppress dead-code warnings on the codec round-trip helper used
    // only by future tests.
    fn _codec_anchor(t: TMessage) -> Result<TMessage, RemoteError> {
        let body = encode_t(&t).map_err(|e| RemoteError::Protocol(format!("{e}")))?;
        let back = decode_t(&body).map_err(|e| RemoteError::Protocol(format!("{e}")))?;
        let _ = encode_r(&RMessage::Flush { tag: t.tag() }).ok();
        Ok(back)
    }
}
