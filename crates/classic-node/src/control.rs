//! Local-only admin control endpoint over a unix-domain socket. The CLI
//! (`classic ad list` / `classic ad show`) connects to this socket and
//! exchanges newline-delimited JSON. There is intentionally NO networked
//! control plane in v1 — the socket lives next to `node_id` in the
//! `state_dir` and is only reachable to processes that can read that
//! directory.
//!
//! Wire format:
//!   request  := one JSON object per line, e.g. `{"cmd":"ad_list"}`
//!   response := one JSON object per line, e.g. `{"ads":[...]}`

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use classic_ad::AdStore;

pub const SOCKET_FILENAME: &str = "admin.sock";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    AdList,
    AdShow { hostname: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdListResponse {
    pub ads: Vec<classic_ad::NodeAd>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    AdList(AdListResponse),
    AdShow(Option<classic_ad::NodeAd>),
    Error { error: String },
}

pub fn socket_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join(SOCKET_FILENAME)
}

pub async fn spawn(
    state_dir: PathBuf,
    store: AdStore,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let path = socket_path(&state_dir);
    // Best-effort cleanup: an old socket from a crashed daemon would block bind.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    debug!(socket = %path.display(), "admin socket bound");
    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = std::fs::remove_file(&path);
                    return;
                }
                accept = listener.accept() => match accept {
                    Ok((stream, _)) => {
                        let store = store.clone();
                        tokio::spawn(async move { handle_client(stream, store).await });
                    }
                    Err(e) => warn!(error = %e, "admin accept failed"),
                }
            }
        }
    }))
}

async fn handle_client(stream: UnixStream, store: AdStore) {
    let (rx, mut tx) = stream.into_split();
    let mut lines = BufReader::new(rx).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = handle_line(&line, &store);
        let mut bytes = match serde_json::to_vec(&resp) {
            Ok(b) => b,
            Err(_) => continue,
        };
        bytes.push(b'\n');
        if tx.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

fn handle_line(line: &str, store: &AdStore) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Response::Error { error: format!("malformed request: {e}") },
    };
    match req {
        Request::AdList => Response::AdList(AdListResponse { ads: store.all_ads() }),
        Request::AdShow { hostname } => {
            let ad = store
                .all_ads()
                .into_iter()
                .find(|a| a.hostname == hostname);
            Response::AdShow(ad)
        }
    }
}

/// CLI-side helper: open the socket, send a single request, return the raw
/// response as a `serde_json::Value`. The CLI then projects out the bits it
/// needs — no shared response type to keep coupling minimal.
pub async fn send_request(
    state_dir: &std::path::Path,
    req: &Request,
) -> std::io::Result<serde_json::Value> {
    let path = socket_path(state_dir);
    let stream = UnixStream::connect(&path).await?;
    let (rx, mut tx) = stream.into_split();
    let mut bytes = serde_json::to_vec(req)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    tx.write_all(&bytes).await?;
    tx.shutdown().await?;
    let mut lines = BufReader::new(rx).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response"))?;
    serde_json::from_str(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// CLI-side parser for the control endpoint's response. We don't share a
/// derive because `Response` is `untagged` on the wire, so `serde_json`
/// will try variants in order at decode time.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ResponseDe {
    AdList(AdListResponse),
    Error { error: String },
    AdShow(Option<NodeAdDe>),
}

/// Mirrors `classic_ad::NodeAd` for client-side decoding (so the CLI can
/// read the JSON response without depending on the precise wire types
/// shifting). For now we just re-export the same type.
pub type NodeAdDe = classic_ad::NodeAd;

#[derive(Debug, Deserialize)]
pub struct AdListResponseDe {
    pub ads: Vec<classic_ad::NodeAd>,
}
