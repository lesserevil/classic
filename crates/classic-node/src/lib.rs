use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use classic_ad::AdConfig;
use classic_proto::FrameMux;

pub mod config;
pub mod control;
pub mod link;
pub mod mesh;
pub mod node_id;
pub mod peers;
pub mod proto_handler;
pub mod shutdown;

pub use config::{Config, ConfigError, LogConfig, NodeConfig};
pub use link::{
    handshake, send_bye, CloseReason, ExistingPeerLookup, LinkHalves, LinkRuntimeConfig,
    PeerLink, PeerRole, PeerState,
};
pub use mesh::{LinkListener, PeerMesh, PeerStatus};
pub use node_id::{ensure_node_id, NodeIdError};
pub use proto_handler::{ProtoHandler, ProtoSignal};
pub use shutdown::{wait_for_signal, DEFAULT_BYE_GRACE};

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    #[error("state-dir: {0}")]
    StateDir(#[from] NodeIdError),
    #[error("bind: {0}")]
    Bind(std::io::Error),
    #[error("ad subsystem: {0}")]
    Ad(String),
}

impl NodeError {
    /// Map to the exit code documented in plans/01-skeleton-transport.md
    /// § "CLI surface (classicd) — exit codes".
    pub fn exit_code(&self) -> i32 {
        match self {
            NodeError::Config(_) => 1,
            NodeError::StateDir(_) => 2,
            NodeError::Bind(_) => 3,
            NodeError::Ad(_) => 1,
        }
    }
}

/// In-process handle for a running node. Returned from `spawn_node` so
/// integration tests can call `mesh.snapshot()` / `ad_store.all_ads()`
/// and trigger a clean shutdown without going through SIGTERM.
pub struct NodeHandle {
    pub mesh: Arc<PeerMesh>,
    pub listen_addr: std::net::SocketAddr,
    pub self_id: classic_proto::NodeId,
    pub ad_store: classic_ad::AdStore,
    /// Discovery refresh + gossip broadcast + admin UDS tasks. Aborted on shutdown.
    ad_tasks: Vec<tokio::task::JoinHandle<()>>,
    ctrl_shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
}

impl NodeHandle {
    pub async fn shutdown(&self, grace: Duration) {
        if let Some(tx) = &self.ctrl_shutdown_tx {
            let _ = tx.send(true);
        }
        for t in &self.ad_tasks {
            t.abort();
        }
        self.mesh.shutdown(grace).await;
    }
}

/// Build the FrameMux + ProtoHandler + PeerMesh wiring, bring up the ad
/// subsystem, and start dialers / listener. `cfg` is taken by value so
/// callers can construct it programmatically for tests.
pub async fn spawn_node(cfg: Config, runtime_cfg: LinkRuntimeConfig) -> Result<NodeHandle, NodeError> {
    spawn_node_with_ad_config(cfg, runtime_cfg, AdConfig::default()).await
}

/// Like `spawn_node` but lets tests / integration callers override the
/// `AdConfig` (e.g. shorten gossip_period).
pub async fn spawn_node_with_ad_config(
    cfg: Config,
    runtime_cfg: LinkRuntimeConfig,
    ad_cfg: AdConfig,
) -> Result<NodeHandle, NodeError> {
    let self_id = ensure_node_id(&cfg.node.state_dir)?;

    let mux = Arc::new(FrameMux::new());
    let proto = ProtoHandler::new();
    mux.register(0x00, proto.clone()).expect("0x00 is a valid range");

    let mesh = PeerMesh::new(
        self_id,
        cfg.node.listen_addr.clone(),
        mux.clone(),
        proto,
        runtime_cfg,
    );

    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "node".to_string());
    let cores_online = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let peers_adapter: Arc<dyn classic_ad::Peers> = Arc::new(peers::PeerMeshSink::new(mesh.clone()));
    let ad_handles = classic_ad::start(
        self_id,
        hostname,
        Box::new(classic_ad::RealSysroot::new()),
        cores_online,
        mux.clone(),
        peers_adapter,
        ad_cfg,
    )
    .map_err(|e| NodeError::Ad(format!("{e}")))?;
    mesh.add_link_listener(Arc::new(peers::GossipLinkListener::new(ad_handles.gossip.clone())));

    let listen_addr = mesh
        .spawn_listener(cfg.node.listen_addr.clone())
        .await
        .map_err(NodeError::Bind)?;

    for peer in cfg.node.peers {
        mesh.spawn_dialer(peer);
    }

    // Bring up the local admin UDS for `classic ad list` etc. Failure here
    // is fatal — we cannot deliver on the CLI contract without it.
    let (ctrl_shutdown_tx, ctrl_shutdown_rx) = tokio::sync::watch::channel(false);
    let ctrl_task = control::spawn(
        cfg.node.state_dir.clone(),
        ad_handles.store.clone(),
        ctrl_shutdown_rx,
    )
    .await
    .map_err(|e| NodeError::Ad(format!("admin socket: {e}")))?;

    Ok(NodeHandle {
        mesh,
        listen_addr,
        self_id,
        ad_store: ad_handles.store,
        ad_tasks: vec![ad_handles.discovery_task, ad_handles.gossip_task, ctrl_task],
        ctrl_shutdown_tx: Some(ctrl_shutdown_tx),
    })
}

/// Binary entry point: load config, spawn the node, then block on a
/// SIGTERM / SIGINT, then run a clean shutdown.
pub async fn run(cfg_path: Option<PathBuf>) -> Result<(), NodeError> {
    let cfg = config::load_config(cfg_path)?;
    let handle = spawn_node(cfg, LinkRuntimeConfig::default()).await?;

    let _ = wait_for_signal().await;
    handle.shutdown(DEFAULT_BYE_GRACE).await;
    Ok(())
}
