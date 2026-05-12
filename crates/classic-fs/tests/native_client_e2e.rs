//! Plan-06 integration test: end-to-end LocalServer ↔ RemoteClient
//! round-trip via the in-process loopback transport. The 9pfuse-
//! interop + FUSE smoke tests live behind a future hw-fuse feature
//! (tracked in classic-s37); this file covers everything we can run
//! without root.

use std::sync::{Arc, Mutex};

use classic_fs::{
    LocalServer, RemoteClient, RemoteError, StubTree, Transport,
};
use classic_proto::Frame;

/// Loopback: every NineReq the client sends gets forwarded straight
/// through a LocalServer; the server's reply is fed back to the
/// client's deliver_response hook.
struct Loopback {
    server: LocalServer,
    client: Mutex<Option<Arc<RemoteClient>>>,
}
impl Loopback {
    fn new(tree: Arc<dyn classic_fs::Tree>) -> Arc<Self> {
        Arc::new(Self {
            server: LocalServer::new(tree),
            client: Mutex::new(None),
        })
    }
    fn attach_client(&self, c: Arc<RemoteClient>) {
        *self.client.lock().unwrap() = Some(c);
    }
}
impl Transport for Loopback {
    fn send(&self, frame: Frame) -> Result<(), RemoteError> {
        let (kind, body) = self
            .server
            .handle(classic_proto::FrameKind::NineReq, &frame.payload)
            .ok_or_else(|| RemoteError::Protocol("server closed connection".into()))?;
        let resp = Frame::new(kind as u16, body.into());
        if let Some(c) = self.client.lock().unwrap().clone() {
            c.deliver_response(&resp);
        }
        Ok(())
    }
}

#[tokio::test]
async fn full_attach_walk_open_read_clunk_round_trip() {
    let transport = Loopback::new(Arc::new(StubTree::file(
        "answer",
        b"forty-two\n",
    )));
    let client = Arc::new(RemoteClient::new(transport.clone()));
    transport.attach_client(client.clone());

    client.version().await.unwrap();
    let root = client.attach("").await.unwrap();
    let file = client.walk(root, &["answer"]).await.unwrap();
    let qid = client.open(file, 0).await.unwrap();
    assert_eq!(qid.type_, classic_fs::Qid::TYPE_FILE);

    let data = client.read(file, 0, 64).await.unwrap();
    assert_eq!(&data, b"forty-two\n");

    // getattr reports the size.
    let stat = client.getattr(file).await.unwrap();
    assert_eq!(stat.size, b"forty-two\n".len() as u64);

    client.clunk(file).await.unwrap();
    client.clunk(root).await.unwrap();
}

#[tokio::test]
async fn readdir_returns_lex_sorted_entries() {
    use classic_fs::Tree;
    use classic_fs::{DirEntry, Qid, Stat};

    // Custom tree with three files for the readdir test.
    struct ThreeFiles;
    impl Tree for ThreeFiles {
        fn walk_one(&self, parent: classic_fs::NodeId, name: &str) -> Option<classic_fs::NodeId> {
            if parent != classic_fs::ROOT_NODE {
                return None;
            }
            match name {
                "alpha" => Some(1),
                "beta" => Some(2),
                "gamma" => Some(3),
                _ => None,
            }
        }
        fn qid(&self, node: classic_fs::NodeId) -> Option<Qid> {
            Some(Qid {
                type_: if node == classic_fs::ROOT_NODE { Qid::TYPE_DIR } else { Qid::TYPE_FILE },
                version: 0,
                path: node,
            })
        }
        fn stat(&self, node: classic_fs::NodeId) -> Option<Stat> {
            Some(Stat {
                qid: self.qid(node)?,
                mode: if node == classic_fs::ROOT_NODE { 0o040555 } else { 0o100444 },
                ..Stat::default()
            })
        }
        fn read(&self, _: classic_fs::NodeId, _: u64, _: u32) -> Result<Vec<u8>, u32> {
            Ok(b"x".to_vec())
        }
        fn readdir(&self, node: classic_fs::NodeId, offset: u64) -> Result<Vec<DirEntry>, u32> {
            if node != classic_fs::ROOT_NODE {
                return Err(classic_fs::ENOTDIR);
            }
            if offset > 0 {
                return Ok(Vec::new());
            }
            // Provide entries already-sorted lex; the server passes
            // them through verbatim per plan §"Treaddir order".
            Ok(vec![
                DirEntry {
                    qid: self.qid(1).unwrap(),
                    offset: 1,
                    ty: 8,
                    name: "alpha".into(),
                },
                DirEntry {
                    qid: self.qid(2).unwrap(),
                    offset: 2,
                    ty: 8,
                    name: "beta".into(),
                },
                DirEntry {
                    qid: self.qid(3).unwrap(),
                    offset: 3,
                    ty: 8,
                    name: "gamma".into(),
                },
            ])
        }
        fn readlink(&self, _: classic_fs::NodeId) -> Option<String> {
            None
        }
        fn is_dir(&self, node: classic_fs::NodeId) -> bool {
            node == classic_fs::ROOT_NODE
        }
    }

    let transport = Loopback::new(Arc::new(ThreeFiles));
    let client = Arc::new(RemoteClient::new(transport.clone()));
    transport.attach_client(client.clone());

    let root = client.attach("").await.unwrap();
    // Server requires a Tlopen before Tread on a fid; readdir is
    // similar — but our server emits dir entries via readdir() without
    // checking opened. Walk to root clone first so the cloned fid is
    // safe to readdir on.
    let dir = client.walk(root, &[]).await.unwrap();
    let entries = client.readdir(dir, 0, 4096).await.unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
}

#[tokio::test]
async fn write_op_rejected_with_erofs() {
    let transport = Loopback::new(Arc::new(StubTree::file("x", b"y")));
    let client = Arc::new(RemoteClient::new(transport.clone()));
    transport.attach_client(client.clone());

    let root = client.attach("").await.unwrap();
    let file = client.walk(root, &["x"]).await.unwrap();
    // O_RDWR=2 — server should reject with EROFS on Tlopen.
    let err = client.open(file, 2).await.unwrap_err();
    assert_eq!(err.errno(), classic_fs::EROFS);
}
