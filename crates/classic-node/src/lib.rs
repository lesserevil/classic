use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use classic_proto::FrameMux;

pub mod config;
pub mod link;
pub mod mesh;
pub mod node_id;
pub mod proto_handler;
pub mod shutdown;

pub use config::{Config, ConfigError, LogConfig, NodeConfig};
pub use link::{
    handshake, send_bye, CloseReason, ExistingPeerLookup, LinkHalves, LinkRuntimeConfig,
    PeerLink, PeerRole, PeerState,
};
pub use mesh::{PeerMesh, PeerStatus};
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
}

impl NodeError {
    /// Map to the exit code documented in plans/01-skeleton-transport.md
    /// § "CLI surface (classicd) — exit codes".
    pub fn exit_code(&self) -> i32 {
        match self {
            NodeError::Config(_) => 1,
            NodeError::StateDir(_) => 2,
            NodeError::Bind(_) => 3,
        }
    }
}

/// In-process handle for a running node. Returned from `spawn_node` so
/// integration tests can call `mesh.snapshot()` and trigger a clean
/// shutdown without going through SIGTERM.
pub struct NodeHandle {
    pub mesh: Arc<PeerMesh>,
    pub listen_addr: std::net::SocketAddr,
    pub self_id: classic_proto::NodeId,
}

impl NodeHandle {
    pub async fn shutdown(&self, grace: Duration) {
        self.mesh.shutdown(grace).await;
    }
}

/// Build the FrameMux + ProtoHandler + PeerMesh wiring and start dialers /
/// listener. `cfg` is taken by value so callers can construct it
/// programmatically for tests.
pub async fn spawn_node(cfg: Config, runtime_cfg: LinkRuntimeConfig) -> Result<NodeHandle, NodeError> {
    let self_id = ensure_node_id(&cfg.node.state_dir)?;

    let mux = Arc::new(FrameMux::new());
    let proto = ProtoHandler::new();
    mux.register(0x00, proto.clone()).expect("0x00 is a valid range");

    // listen_addr in cfg may be `host:0` for tests; PeerMesh learns the real
    // address back from spawn_listener.
    let mesh = PeerMesh::new(
        self_id,
        cfg.node.listen_addr.clone(),
        mux,
        proto,
        runtime_cfg,
    );

    let listen_addr = mesh
        .spawn_listener(cfg.node.listen_addr.clone())
        .await
        .map_err(NodeError::Bind)?;

    for peer in cfg.node.peers {
        mesh.spawn_dialer(peer);
    }

    Ok(NodeHandle { mesh, listen_addr, self_id })
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
