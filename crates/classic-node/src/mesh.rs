//! `PeerMesh` — supervisor for outbound dialers, the inbound listener, and
//! the table of established `PeerLink`s. Owns the shared `FrameMux` and
//! `ProtoHandler` and exposes a `snapshot()` for tests / observability.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rand::Rng;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use classic_proto::{Frame, FrameMux, NodeId};

use crate::link::{
    handshake, run_peer_link, CloseReason, ExistingPeerLookup, LinkRuntimeConfig, PeerRole,
    PeerState,
};
use crate::proto_handler::ProtoHandler;

const RECONNECT_INITIAL: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const RECONNECT_JITTER_PCT: f64 = 0.20;

#[derive(Clone, Debug)]
pub struct PeerStatus {
    pub node_id: NodeId,
    pub addr: String,
    pub state: PeerState,
    pub last_rx: Option<Instant>,
    pub last_tx: Option<Instant>,
    pub missed_heartbeats: u8,
}

struct LinkHandle {
    addr: String,
    state: Arc<std::sync::RwLock<PeerState>>,
    sender: mpsc::Sender<Frame>,
}

/// Subscriber to peer-up / peer-down events. Used by upper layers (gossip,
/// service directory) to react to mesh state changes without polling.
pub trait LinkListener: Send + Sync {
    fn on_peer_up(&self, peer: NodeId);
    fn on_peer_down(&self, peer: NodeId);
}

pub struct PeerMesh {
    self_id: NodeId,
    self_listen_addr: String,
    links: DashMap<NodeId, LinkHandle>,
    mux: Arc<FrameMux>,
    proto: Arc<ProtoHandler>,
    cfg: LinkRuntimeConfig,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    tasks: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
    /// Optional subscribers to peer-up / peer-down events. Pushed onto by
    /// `add_link_listener`; iterated when run_link enters / exits Healthy.
    listeners: std::sync::RwLock<Vec<Arc<dyn LinkListener>>>,
}

impl PeerMesh {
    pub fn new(
        self_id: NodeId,
        self_listen_addr: String,
        mux: Arc<FrameMux>,
        proto: Arc<ProtoHandler>,
        cfg: LinkRuntimeConfig,
    ) -> Arc<Self> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Arc::new(Self {
            self_id,
            self_listen_addr,
            links: DashMap::new(),
            mux,
            proto,
            cfg,
            shutdown_tx,
            shutdown_rx,
            tasks: tokio::sync::Mutex::new(Vec::new()),
            listeners: std::sync::RwLock::new(Vec::new()),
        })
    }

    /// Register a peer-up / peer-down listener. Listeners fire after the
    /// link is recorded in / removed from the live table.
    pub fn add_link_listener(&self, listener: Arc<dyn LinkListener>) {
        self.listeners
            .write()
            .expect("listeners poisoned")
            .push(listener);
    }

    /// Live peer NodeIds. Used by gossip / service directory broadcasts.
    pub fn live_peers(&self) -> Vec<NodeId> {
        self.links.iter().map(|e| *e.key()).collect()
    }

    /// Send a frame to one peer. Fire-and-forget — if the per-link mpsc is
    /// full or closed, the frame is dropped with a debug log; the caller
    /// is not blocked. Senders that need delivery confirmation should use
    /// the higher-level RPC primitives once they exist.
    pub fn send_to(&self, peer: NodeId, frame: Frame) {
        if let Some(handle) = self.links.get(&peer) {
            if let Err(_) = handle.sender.try_send(frame) {
                tracing::debug!(?peer, "dropped outbound frame: link queue full or closed");
            }
        }
    }

    pub fn self_id(&self) -> NodeId {
        self_id_log(self.self_id)
    }

    pub fn snapshot(&self) -> Vec<PeerStatus> {
        self.links
            .iter()
            .map(|entry| {
                let h = entry.value();
                let state = h
                    .state
                    .read()
                    .map(|s| s.clone())
                    .unwrap_or(PeerState::Closed(CloseReason::TransportLost));
                PeerStatus {
                    node_id: *entry.key(),
                    addr: h.addr.clone(),
                    state,
                    last_rx: None,
                    last_tx: None,
                    missed_heartbeats: 0,
                }
            })
            .collect()
    }

    pub fn spawn_dialer(self: &Arc<Self>, peer_addr: String) {
        let mesh = self.clone();
        let handle = tokio::spawn(async move { dialer_loop(mesh, peer_addr).await });
        self.tasks
            .try_lock()
            .expect("mesh tasks lock should not be contended")
            .push(handle);
    }

    /// Bind the listener and start accepting. Returns the bound address so
    /// tests can pick `127.0.0.1:0` and learn the actual port.
    pub async fn spawn_listener(
        self: &Arc<Self>,
        listen_addr: String,
    ) -> Result<std::net::SocketAddr, std::io::Error> {
        let listener = TcpListener::bind(&listen_addr).await?;
        let local = listener.local_addr()?;
        let mesh = self.clone();
        let handle = tokio::spawn(async move { listener_loop(mesh, listener).await });
        self.tasks
            .try_lock()
            .expect("mesh tasks lock should not be contended")
            .push(handle);
        Ok(local)
    }

    /// Send Bye to every healthy peer, wait `grace` for the writes to flush,
    /// then signal shutdown so dialer / listener loops exit. Idempotent.
    pub async fn shutdown(self: &Arc<Self>, grace: Duration) {
        for entry in self.links.iter() {
            let h = entry.value();
            // Best-effort: enqueue Bye through the per-link mpsc. If the
            // channel is closed (link already torn down) we just skip it.
            let bye = Frame::new(
                classic_proto::FrameKind::Bye as u16,
                bytes::Bytes::from(
                    classic_proto::encode_payload(&classic_proto::ByePayload).unwrap(),
                ),
            );
            let _ = h.sender.try_send(bye);
        }
        tokio::time::sleep(grace).await;
        let _ = self.shutdown_tx.send(true);
        let mut tasks = self.tasks.lock().await;
        for t in tasks.drain(..) {
            // The tasks should observe the shutdown and exit promptly. Give
            // each up to 200 ms; abort otherwise.
            match tokio::time::timeout(Duration::from_millis(200), t).await {
                Ok(_) => {}
                Err(_) => {} // task already detached or running; let runtime drop
            }
        }
    }
}

impl ExistingPeerLookup for PeerMesh {
    fn has_link_for(&self, peer: NodeId) -> bool {
        self.links.contains_key(&peer)
    }
}

fn self_id_log(id: NodeId) -> NodeId {
    id
}

async fn dialer_loop(mesh: Arc<PeerMesh>, peer_addr: String) {
    let mut backoff = RECONNECT_INITIAL;
    let mut shutdown_rx = mesh.shutdown_rx.clone();

    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        let connect = TcpStream::connect(&peer_addr);
        let stream = tokio::select! {
            _ = shutdown_rx.changed() => return,
            res = connect => match res {
                Ok(s) => s,
                Err(e) => {
                    warn!(peer_addr = %peer_addr, error = %e, "dial failed; backing off");
                    sleep_with_jitter(backoff, &mut shutdown_rx).await;
                    backoff = next_backoff(backoff);
                    continue;
                }
            }
        };
        let _ = stream.set_nodelay(true);

        match handshake(
            stream,
            PeerRole::Dialer,
            mesh.self_id,
            mesh.self_listen_addr.clone(),
            mesh.as_ref(),
        )
        .await
        {
            Ok(link) => {
                info!(peer_addr = %peer_addr, peer_id = %link.peer_id(), "dialer handshake ok");
                run_link(&mesh, link, peer_addr.clone()).await;
                backoff = RECONNECT_INITIAL;
            }
            Err(reason) => {
                warn!(peer_addr = %peer_addr, ?reason, "dialer handshake failed");
            }
        }

        sleep_with_jitter(backoff, &mut shutdown_rx).await;
        backoff = next_backoff(backoff);
    }
}

async fn listener_loop(mesh: Arc<PeerMesh>, listener: TcpListener) {
    let mut shutdown_rx = mesh.shutdown_rx.clone();
    info!(addr = %mesh.self_listen_addr, "listener accepting");
    loop {
        let accept = listener.accept();
        let (stream, _peer_addr) = tokio::select! {
            _ = shutdown_rx.changed() => return,
            res = accept => match res {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    continue;
                }
            }
        };
        let _ = stream.set_nodelay(true);
        let mesh = mesh.clone();
        tokio::spawn(async move {
            match handshake(
                stream,
                PeerRole::Listener,
                mesh.self_id,
                mesh.self_listen_addr.clone(),
                mesh.as_ref(),
            )
            .await
            {
                Ok(link) => {
                    info!(peer_id = %link.peer_id(), "listener handshake ok");
                    run_link(&mesh, link, link_addr_unknown()).await;
                }
                Err(reason) => warn!(?reason, "listener handshake failed"),
            }
        });
    }
}

fn link_addr_unknown() -> String {
    String::new()
}

async fn run_link<S>(mesh: &Arc<PeerMesh>, link: crate::link::PeerLink<S>, addr: String)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let peer_id = link.peer_id();
    let state = link.state_handle();
    let halves = link.into_halves();
    let sender = halves.send_tx.clone();

    let display_addr = if addr.is_empty() {
        halves.peer_listen_addr.clone()
    } else {
        addr
    };

    mesh.links.insert(
        peer_id,
        LinkHandle { addr: display_addr, state: state.clone(), sender },
    );
    // Notify subscribers (gossip, etc.) that a peer is healthy.
    for l in mesh.listeners.read().expect("listeners poisoned").iter() {
        l.on_peer_up(peer_id);
    }
    let close = run_peer_link(halves, mesh.mux.clone(), mesh.proto.clone(), mesh.cfg.clone()).await;
    info!(peer_id = %peer_id, ?close, "peer link closed");
    // Only remove our entry if it still points at this link's state; otherwise
    // a tiebreak winner has already taken over and we mustn't drop their handle.
    let removed = mesh
        .links
        .remove_if(&peer_id, |_, h| Arc::ptr_eq(&h.state, &state))
        .is_some();
    if removed {
        for l in mesh.listeners.read().expect("listeners poisoned").iter() {
            l.on_peer_down(peer_id);
        }
    }
}

fn sleep_with_jitter(base: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> JitterSleep<'_> {
    JitterSleep::new(base, shutdown_rx)
}

struct JitterSleep<'a> {
    sleep: Duration,
    shutdown_rx: &'a mut watch::Receiver<bool>,
}

impl<'a> JitterSleep<'a> {
    fn new(base: Duration, shutdown_rx: &'a mut watch::Receiver<bool>) -> Self {
        let jitter_factor = {
            let mut rng = rand::thread_rng();
            rng.gen_range(-RECONNECT_JITTER_PCT..=RECONNECT_JITTER_PCT)
        };
        let nanos = base.as_nanos() as f64;
        let final_nanos = (nanos * (1.0 + jitter_factor)).max(0.0) as u64;
        Self {
            sleep: Duration::from_nanos(final_nanos),
            shutdown_rx,
        }
    }
}

impl<'a> std::future::IntoFuture for JitterSleep<'a> {
    type Output = ();
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let JitterSleep { sleep, shutdown_rx } = self;
            tokio::select! {
                _ = shutdown_rx.changed() => {}
                _ = tokio::time::sleep(sleep) => {}
            }
        })
    }
}

fn next_backoff(prev: Duration) -> Duration {
    std::cmp::min(prev.saturating_mul(2), RECONNECT_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_doubles_then_caps() {
        let mut b = RECONNECT_INITIAL;
        for _ in 0..20 {
            b = next_backoff(b);
        }
        assert_eq!(b, RECONNECT_MAX);
    }

    #[test]
    fn jitter_stays_within_pct() {
        let (_tx, mut rx) = watch::channel(false);
        let base = Duration::from_secs(1);
        for _ in 0..1000 {
            let s = JitterSleep::new(base, &mut rx).sleep;
            let lower = base.mul_f64(1.0 - RECONNECT_JITTER_PCT - 0.001);
            let upper = base.mul_f64(1.0 + RECONNECT_JITTER_PCT + 0.001);
            assert!(s >= lower && s <= upper, "jitter {:?} out of band", s);
        }
    }
}
