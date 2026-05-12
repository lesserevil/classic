//! Spawn-frame control socket. Binds `<state_dir>/spawn.sock` and serves
//! one SpawnRequest per connection by running the originator/executor
//! pipeline in-process. v1 ships a local-only Placer + PeerSpawn so
//! every spawn lands on the daemon itself; cross-node spawn is a
//! follow-up that wires PeerMesh::send_to + a peer-side router.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use classic_proto::{
    decode_frame, encode_frame, encode_payload, ChildExit, ChildStdio, Frame, FrameKind, NodeId,
    SpawnAck, SpawnDeny, SpawnRequest, StdioStream,
};
use classic_spawn::{
    deny::CandidateDenial, run_executor, run_originator, AttemptOutcome, LocalAdMatcher,
    MboxAllocator, NoOpScopeProvider, PeerSpawn, Placer, SpawnError,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, info, warn};

pub const SOCKET_FILENAME: &str = "spawn.sock";

pub fn socket_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join(SOCKET_FILENAME)
}

/// Bring up the UDS and serve. Returns a `JoinHandle` the daemon
/// stores so it can abort on shutdown. The `mbox_alloc` is shared with
/// the rest of the daemon so MboxIds stay unique across all entry
/// points.
pub async fn spawn(
    state_dir: PathBuf,
    self_id: NodeId,
    mbox_alloc: Arc<MboxAllocator>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let path = socket_path(&state_dir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    info!(socket = %path.display(), "spawn socket bound");
    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = std::fs::remove_file(&path);
                    return;
                }
                accept = listener.accept() => match accept {
                    Ok((stream, _)) => {
                        let alloc = mbox_alloc.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, self_id, alloc).await {
                                debug!(error = %e, "spawn socket client exited");
                            }
                        });
                    }
                    Err(e) => warn!(error = %e, "spawn socket accept failed"),
                }
            }
        }
    }))
}

async fn serve_connection(
    stream: UnixStream,
    self_id: NodeId,
    _mbox_alloc: Arc<MboxAllocator>,
) -> std::io::Result<()> {
    let (read, write) = stream.into_split();
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let mut read = read;
    // Each connection serves exactly one SpawnRequest. The CLI sends
    // its request, then the daemon streams ChildStdio + ChildExit
    // frames back until the child exits.
    let frame = decode_frame(&mut read)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if frame.kind != FrameKind::SpawnRequest as u16 {
        warn!(kind = format!("{:#06x}", frame.kind), "spawn socket: expected SpawnRequest");
        return Ok(());
    }
    let req: SpawnRequest = classic_proto::decode_payload(&frame.payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let req_id = req.req_id;

    // v1 local-only: Placer returns self; PeerSpawn ack's locally.
    let placer = LocalOnlyPlacer { self_id };
    let attempted = std::sync::Mutex::new(Vec::<CandidateDenial>::new());
    let peer_spawn = LocalAckPeerSpawn { self_id, attempted: &attempted };

    let placement = run_originator(&req, &placer, &peer_spawn);
    match placement {
        Ok(node) => {
            // Send SpawnAck.
            let ack = SpawnAck {
                req_id,
                net_id: classic_proto::NetId {
                    node,
                    mbox: classic_proto::MboxId(0),
                },
            };
            let body = encode_payload(&ack).expect("encode SpawnAck");
            let ack_frame = Frame::new(FrameKind::SpawnAck as u16, Bytes::from(body));
            send_frame(&write, &ack_frame).await?;

            // Run the executor in-process. Predicate-matching against
            // the local ad is skipped here (always-true matcher) since
            // the proper adapter between classic-ad and classic-place
            // NodeAd is a separate task.
            let matcher = AlwaysMatch;
            let scope = NoOpScopeProvider;
            match run_executor(&req, node, &matcher, &scope).await {
                Ok((_node, child, _scope_guard)) => {
                    relay_child(write.clone(), req_id, child).await?;
                }
                Err(e) => {
                    let deny = SpawnDeny {
                        req_id,
                        reason: e.deny_reason(),
                        detail: format!("{e}"),
                    };
                    let body = encode_payload(&deny).expect("encode SpawnDeny");
                    let f = Frame::new(FrameKind::SpawnDeny as u16, Bytes::from(body));
                    send_frame(&write, &f).await?;
                }
            }
        }
        Err(e) => {
            let deny = SpawnDeny {
                req_id,
                reason: e.deny_reason(),
                detail: format!("{e}"),
            };
            let body = encode_payload(&deny).expect("encode SpawnDeny");
            let f = Frame::new(FrameKind::SpawnDeny as u16, Bytes::from(body));
            send_frame(&write, &f).await?;
        }
    }

    Ok(())
}

async fn send_frame(
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    frame: &Frame,
) -> std::io::Result<()> {
    let mut guard = writer.lock().await;
    encode_frame(&mut *guard, frame)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
}

async fn relay_child(
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    req_id: u64,
    child: classic_spawn::ChildHandle,
) -> std::io::Result<()> {
    let parts = child.into_parts();
    let classic_spawn::ChildParts { stdout, stderr, stdin: _, wait, .. } = parts;
    let mut stdout_rx = stdout;
    let mut stderr_rx = stderr;

    let writer_stdout = writer.clone();
    let stdout_task = tokio::spawn(async move {
        while let Some(chunk) = stdout_rx.recv().await {
            let frame = ChildStdio { req_id, stream: StdioStream::Stdout, data: chunk };
            if let Ok(body) = encode_payload(&frame) {
                let f = Frame::new(FrameKind::ChildStdio as u16, Bytes::from(body));
                if send_frame(&writer_stdout, &f).await.is_err() {
                    break;
                }
            }
        }
    });
    let writer_stderr = writer.clone();
    let stderr_task = tokio::spawn(async move {
        while let Some(chunk) = stderr_rx.recv().await {
            let frame = ChildStdio { req_id, stream: StdioStream::Stderr, data: chunk };
            if let Ok(body) = encode_payload(&frame) {
                let f = Frame::new(FrameKind::ChildStdio as u16, Bytes::from(body));
                if send_frame(&writer_stderr, &f).await.is_err() {
                    break;
                }
            }
        }
    });

    let exit_info = match wait.await {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            warn!(error = %e, "child wait failed");
            classic_spawn::ChildExitInfo { code: None, signal: None }
        }
        Err(e) => {
            warn!(error = %e, "child wait task panicked");
            classic_spawn::ChildExitInfo { code: None, signal: None }
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let exit = ChildExit { req_id, code: exit_info.code, signal: exit_info.signal };
    let body = encode_payload(&exit).expect("encode ChildExit");
    let f = Frame::new(FrameKind::ChildExit as u16, Bytes::from(body));
    send_frame(&writer, &f).await?;
    let mut g = writer.lock().await;
    let _ = g.shutdown().await;
    Ok(())
}

struct LocalOnlyPlacer {
    self_id: NodeId,
}
impl Placer for LocalOnlyPlacer {
    fn place(&self, _requires: &str, _rank: &str) -> Result<Vec<NodeId>, SpawnError> {
        Ok(vec![self.self_id])
    }
}

struct LocalAckPeerSpawn<'a> {
    self_id: NodeId,
    attempted: &'a std::sync::Mutex<Vec<CandidateDenial>>,
}
impl<'a> PeerSpawn for LocalAckPeerSpawn<'a> {
    fn try_spawn(
        &self,
        peer: NodeId,
        _req: &SpawnRequest,
    ) -> Result<AttemptOutcome, SpawnError> {
        if peer == self.self_id {
            Ok(AttemptOutcome::Ack)
        } else {
            self.attempted
                .lock()
                .unwrap()
                .push(CandidateDenial {
                    node: peer,
                    reason: classic_proto::DenyReason::Internal,
                    detail: "cross-node spawn not yet wired".into(),
                });
            Err(SpawnError::Internal("remote spawn not yet wired".into()))
        }
    }
}

struct AlwaysMatch;
impl LocalAdMatcher for AlwaysMatch {
    fn matches(&self, _requires: &str) -> Result<bool, SpawnError> {
        Ok(true)
    }
}
