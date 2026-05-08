use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::time::timeout;
use tracing::{info, warn};

use classic_proto::{
    decode_frame, decode_payload, encode_frame, encode_payload, ByePayload, CodecError,
    ErrorCode, ErrorPayload, Frame, FrameKind, HelloPayload, NodeId, PROTO_VERSION,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const PEER_CHANNEL_DEPTH: usize = 1024;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PeerRole {
    Dialer,
    Listener,
}

impl PeerRole {
    fn as_str(self) -> &'static str {
        match self {
            PeerRole::Dialer => "dialer",
            PeerRole::Listener => "listener",
        }
    }
}

/// Lifecycle of a single PeerLink. Entries map to plan 01 §"Connection
/// lifecycle state machine".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerState {
    /// TCP is up but Hello has not been exchanged yet.
    HelloPending,
    /// Hello succeeded; heartbeats keep the link healthy.
    Healthy,
    /// Heartbeat misses crossed the threshold; recoverable on any inbound
    /// frame. Populated by the sibling heartbeat task; left exposed here so
    /// downstream code can match on the same enum.
    Unhealthy,
    Closed(CloseReason),
}

impl PeerState {
    fn label(&self) -> &'static str {
        match self {
            PeerState::HelloPending => "hello_pending",
            PeerState::Healthy => "healthy",
            PeerState::Unhealthy => "unhealthy",
            PeerState::Closed(_) => "closed",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// Peer's Hello was structurally invalid (wrong frame kind, malformed
    /// payload, or peer reported a proto version we cannot speak).
    HandshakeRejected,
    /// Peer's Hello carried our own NodeId — we accidentally connected to
    /// ourselves.
    SelfLoop,
    /// Two peers raced and we are the higher NodeId in the ordered pair, so
    /// our connection is the one to drop. The lower-NodeId initiated link
    /// keeps running.
    LosingTiebreak,
    /// Peer never finished the Hello handshake within `HELLO_TIMEOUT`.
    HelloTimeout,
    /// Peer sent Bye.
    PeerBye,
    /// Peer sent an Error frame.
    PeerError,
    /// IO error / EOF on the underlying transport.
    TransportLost,
}

impl CloseReason {
    fn label(&self) -> &'static str {
        match self {
            CloseReason::HandshakeRejected => "handshake_rejected",
            CloseReason::SelfLoop => "self_loop",
            CloseReason::LosingTiebreak => "losing_tiebreak",
            CloseReason::HelloTimeout => "hello_timeout",
            CloseReason::PeerBye => "peer_bye",
            CloseReason::PeerError => "peer_error",
            CloseReason::TransportLost => "transport_lost",
        }
    }
}

/// Returns true if the local node already has a live PeerLink for `peer`.
/// `PeerMesh` (sibling task classic-8kh) implements this; the trait keeps
/// this module independent of the supervisor.
pub trait ExistingPeerLookup: Send + Sync {
    fn has_link_for(&self, peer: NodeId) -> bool;
}

/// An established PeerLink, returned from `handshake`. Holds the split read
/// and write halves of the underlying stream so the heartbeat / dispatch
/// tasks (classic-62g, classic-8kh) can drive the link without re-acquiring
/// any state.
pub struct PeerLink<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> {
    peer_id: NodeId,
    peer_listen_addr: String,
    role: PeerRole,
    state: Arc<RwLock<PeerState>>,
    reader: ReadHalf<S>,
    writer: WriteHalf<S>,
    /// Bound mpsc the writer task can drain to send frames. Created here so
    /// every link in this codebase shares the same backpressure profile.
    send_tx: tokio::sync::mpsc::Sender<Frame>,
    send_rx: Option<tokio::sync::mpsc::Receiver<Frame>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> std::fmt::Debug for PeerLink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerLink")
            .field("peer_id", &self.peer_id)
            .field("peer_listen_addr", &self.peer_listen_addr)
            .field("role", &self.role)
            .field("state", &*self.state.read().expect("PeerState poisoned"))
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> PeerLink<S> {
    pub fn peer_id(&self) -> NodeId {
        self.peer_id
    }
    pub fn peer_listen_addr(&self) -> &str {
        &self.peer_listen_addr
    }
    pub fn role(&self) -> PeerRole {
        self.role
    }
    pub fn state_handle(&self) -> Arc<RwLock<PeerState>> {
        self.state.clone()
    }
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<Frame> {
        self.send_tx.clone()
    }
    /// Returns the receiver half of the bound mpsc. Should be called once,
    /// by the writer task. Returns `None` if already taken.
    pub fn take_send_rx(&mut self) -> Option<tokio::sync::mpsc::Receiver<Frame>> {
        self.send_rx.take()
    }
    /// Splits ownership: the reader half (for the dispatch loop) and the
    /// writer half (for the writer loop). After this call the PeerLink
    /// shell remains for state inspection; the caller drives both halves.
    pub fn into_halves(self) -> LinkHalves<S> {
        LinkHalves {
            peer_id: self.peer_id,
            peer_listen_addr: self.peer_listen_addr,
            role: self.role,
            state: self.state,
            reader: self.reader,
            writer: self.writer,
            send_tx: self.send_tx,
            send_rx: self.send_rx,
        }
    }
}

pub struct LinkHalves<S> {
    pub peer_id: NodeId,
    pub peer_listen_addr: String,
    pub role: PeerRole,
    pub state: Arc<RwLock<PeerState>>,
    pub reader: ReadHalf<S>,
    pub writer: WriteHalf<S>,
    pub send_tx: tokio::sync::mpsc::Sender<Frame>,
    pub send_rx: Option<tokio::sync::mpsc::Receiver<Frame>>,
}

fn transition(state: &Arc<RwLock<PeerState>>, peer: Option<NodeId>, role: PeerRole, to: PeerState) {
    let from_label;
    {
        let cur = state.read().expect("PeerState poisoned");
        from_label = cur.label();
    }
    let to_label = to.label();
    let close_reason = if let PeerState::Closed(r) = &to {
        Some(r.label())
    } else {
        None
    };
    *state.write().expect("PeerState poisoned") = to;
    info!(
        peer = ?peer,
        role = role.as_str(),
        from = from_label,
        to = to_label,
        reason = close_reason.unwrap_or(""),
        "peer-link state transition"
    );
}

/// Run the Hello handshake on `stream` and return a healthy `PeerLink` on
/// success. On rejection (any non-`PeerBye` close reason for this stage), the
/// stream is dropped (which closes the underlying connection); the caller
/// gets the close reason and decides whether to retry.
pub async fn handshake<S, L>(
    stream: S,
    role: PeerRole,
    self_id: NodeId,
    self_listen_addr: String,
    existing: &L,
) -> Result<PeerLink<S>, CloseReason>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    L: ExistingPeerLookup + ?Sized,
{
    let state = Arc::new(RwLock::new(PeerState::HelloPending));
    info!(role = role.as_str(), state = "hello_pending", "peer-link opened");

    let (mut reader, mut writer) = tokio::io::split(stream);

    let our_hello = HelloPayload {
        proto_version: PROTO_VERSION,
        node_id: self_id,
        listen_addr: self_listen_addr,
        capabilities: 0,
    };
    let our_hello_bytes = encode_payload(&our_hello).map_err(|e| {
        warn!(error = %e, "failed to encode our Hello");
        CloseReason::HandshakeRejected
    })?;
    let hello_frame = Frame::new(FrameKind::Hello as u16, Bytes::from(our_hello_bytes));
    if let Err(e) = encode_frame(&mut writer, &hello_frame).await {
        transition(&state, None, role, PeerState::Closed(transport_or(e)));
        return Err(CloseReason::TransportLost);
    }

    // Await peer Hello with the 5 s timeout per plan §FR-? / lifecycle FSM.
    let peer_frame = match timeout(HELLO_TIMEOUT, decode_frame(&mut reader)).await {
        Err(_elapsed) => {
            transition(&state, None, role, PeerState::Closed(CloseReason::HelloTimeout));
            return Err(CloseReason::HelloTimeout);
        }
        Ok(Err(_codec)) => {
            transition(&state, None, role, PeerState::Closed(CloseReason::TransportLost));
            return Err(CloseReason::TransportLost);
        }
        Ok(Ok(frame)) => frame,
    };

    if peer_frame.kind != FrameKind::Hello as u16 {
        let _ = send_error(&mut writer, ErrorCode::ProtocolViolation, "expected Hello").await;
        transition(&state, None, role, PeerState::Closed(CloseReason::HandshakeRejected));
        return Err(CloseReason::HandshakeRejected);
    }

    let peer_hello: HelloPayload = match decode_payload(&peer_frame.payload) {
        Ok(h) => h,
        Err(_e) => {
            let _ = send_error(&mut writer, ErrorCode::ProtocolViolation, "malformed Hello").await;
            transition(&state, None, role, PeerState::Closed(CloseReason::HandshakeRejected));
            return Err(CloseReason::HandshakeRejected);
        }
    };

    if peer_hello.proto_version != PROTO_VERSION {
        let _ = send_error(
            &mut writer,
            ErrorCode::ProtoVersionMismatch,
            &format!("we speak v{PROTO_VERSION}; peer speaks v{}", peer_hello.proto_version),
        )
        .await;
        transition(
            &state,
            Some(peer_hello.node_id),
            role,
            PeerState::Closed(CloseReason::HandshakeRejected),
        );
        return Err(CloseReason::HandshakeRejected);
    }

    if peer_hello.node_id == self_id {
        transition(&state, Some(self_id), role, PeerState::Closed(CloseReason::SelfLoop));
        return Err(CloseReason::SelfLoop);
    }

    // Tiebreak: if a link already exists and we are the higher NodeId in the
    // ordered pair, drop our connection. The lower-NodeId-initiated link
    // wins per plan §FR-7.
    if existing.has_link_for(peer_hello.node_id) && self_id > peer_hello.node_id {
        transition(
            &state,
            Some(peer_hello.node_id),
            role,
            PeerState::Closed(CloseReason::LosingTiebreak),
        );
        return Err(CloseReason::LosingTiebreak);
    }

    transition(&state, Some(peer_hello.node_id), role, PeerState::Healthy);

    let (send_tx, send_rx) = tokio::sync::mpsc::channel(PEER_CHANNEL_DEPTH);
    Ok(PeerLink {
        peer_id: peer_hello.node_id,
        peer_listen_addr: peer_hello.listen_addr,
        role,
        state,
        reader,
        writer,
        send_tx,
        send_rx: Some(send_rx),
    })
}

fn transport_or(_: CodecError) -> CloseReason {
    CloseReason::TransportLost
}

async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: ErrorCode,
    message: &str,
) -> Result<(), CodecError> {
    let payload = encode_payload(&ErrorPayload {
        code,
        message: message.to_string(),
    })?;
    let frame = Frame::new(FrameKind::Error as u16, Bytes::from(payload));
    encode_frame(writer, &frame).await
}

/// Convenience: a clean-shutdown helper that emits a Bye frame. Used by the
/// shutdown path; lives here because it shares the writer-half abstraction.
pub async fn send_bye<W: AsyncWrite + Unpin>(writer: &mut W) -> Result<(), CodecError> {
    let payload = encode_payload(&ByePayload).expect("ByePayload encodes to zero bytes");
    let frame = Frame::new(FrameKind::Bye as u16, Bytes::from(payload));
    encode_frame(writer, &frame).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    struct NoExisting;
    impl ExistingPeerLookup for NoExisting {
        fn has_link_for(&self, _peer: NodeId) -> bool {
            false
        }
    }

    struct AlwaysExists;
    impl ExistingPeerLookup for AlwaysExists {
        fn has_link_for(&self, _peer: NodeId) -> bool {
            true
        }
    }

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    /// Build a Hello frame with arbitrary fields, useful when one side of
    /// the test pretends to be a peer at a different proto version.
    async fn write_custom_hello<W: AsyncWrite + Unpin>(
        w: &mut W,
        proto_version: u32,
        node_id: NodeId,
    ) {
        let p = HelloPayload {
            proto_version,
            node_id,
            listen_addr: "127.0.0.1:0".into(),
            capabilities: 0,
        };
        let bytes = encode_payload(&p).unwrap();
        let f = Frame::new(FrameKind::Hello as u16, Bytes::from(bytes));
        encode_frame(w, &f).await.unwrap();
    }

    #[tokio::test]
    async fn handshake_ok() {
        let (a, b) = duplex(64 * 1024);
        let id_a = id(1);
        let id_b = id(2);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, id_a, "a:1".into(), &NoExisting).await
        });
        let b_task = tokio::spawn(async move {
            handshake(b, PeerRole::Listener, id_b, "b:1".into(), &NoExisting).await
        });
        let la = a_task.await.unwrap().unwrap();
        let lb = b_task.await.unwrap().unwrap();
        assert_eq!(la.peer_id(), id_b);
        assert_eq!(lb.peer_id(), id_a);
        assert_eq!(la.peer_listen_addr(), "b:1");
        assert!(matches!(*la.state.read().unwrap(), PeerState::Healthy));
        assert!(matches!(*lb.state.read().unwrap(), PeerState::Healthy));
    }

    #[tokio::test]
    async fn handshake_version_mismatch() {
        let (a, b) = duplex(64 * 1024);
        let id_a = id(1);
        let id_b = id(2);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, id_a, "a:1".into(), &NoExisting).await
        });
        // Peer pretends to speak v999. Read our Hello first to drain the
        // duplex so our writer doesn't block, then send the bogus one.
        let (mut br, mut bw) = tokio::io::split(b);
        let _ = decode_frame(&mut br).await.unwrap();
        write_custom_hello(&mut bw, 999, id_b).await;
        // Read whatever response we send (Error frame), to verify it was sent.
        let resp = decode_frame(&mut br).await.unwrap();
        assert_eq!(resp.kind, FrameKind::Error as u16);
        let err: ErrorPayload = decode_payload(&resp.payload).unwrap();
        assert_eq!(err.code, ErrorCode::ProtoVersionMismatch);

        let a_result = a_task.await.unwrap();
        assert_eq!(a_result.unwrap_err(), CloseReason::HandshakeRejected);
    }

    #[tokio::test]
    async fn self_loop() {
        let (a, b) = duplex(64 * 1024);
        let me = id(7);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, me, "self:1".into(), &NoExisting).await
        });
        // Peer responds with our own NodeId.
        let (mut br, mut bw) = tokio::io::split(b);
        let _ = decode_frame(&mut br).await.unwrap();
        write_custom_hello(&mut bw, PROTO_VERSION, me).await;
        let res = a_task.await.unwrap();
        assert_eq!(res.unwrap_err(), CloseReason::SelfLoop);
    }

    #[tokio::test]
    async fn tiebreak_higher_loses() {
        // Higher-NodeId side has an existing link for the lower NodeId, so
        // it should drop. Lower-NodeId side has no existing link and wins.
        let (a, b) = duplex(64 * 1024);
        let high = id(9);
        let low = id(2);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, high, "high:1".into(), &AlwaysExists).await
        });
        let b_task = tokio::spawn(async move {
            handshake(b, PeerRole::Listener, low, "low:1".into(), &NoExisting).await
        });
        let a_res = a_task.await.unwrap();
        assert_eq!(a_res.unwrap_err(), CloseReason::LosingTiebreak);
        // The lower-NodeId side may either complete (if it ran fast enough)
        // or fail with TransportLost when the higher side drops the duplex.
        // We accept either; what matters is that the higher side closed.
        let _ = b_task.await.unwrap();
    }

    #[tokio::test]
    async fn tiebreak_lower_wins() {
        // The lower-NodeId side should NOT drop even if it sees an existing
        // link, since tiebreak only fires when self_id > peer_id.
        let (a, b) = duplex(64 * 1024);
        let low = id(2);
        let high = id(9);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, low, "low:1".into(), &AlwaysExists).await
        });
        let b_task = tokio::spawn(async move {
            handshake(b, PeerRole::Listener, high, "high:1".into(), &NoExisting).await
        });
        let res_low = a_task.await.unwrap();
        assert!(res_low.is_ok(), "lower NodeId must keep its link");
        let res_high = b_task.await.unwrap();
        assert!(res_high.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn hello_timeout() {
        let (a, _b) = duplex(64 * 1024);
        let me = id(1);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, me, "a:1".into(), &NoExisting).await
        });
        tokio::time::advance(HELLO_TIMEOUT + Duration::from_millis(1)).await;
        let res = a_task.await.unwrap();
        assert_eq!(res.unwrap_err(), CloseReason::HelloTimeout);
    }

    #[tokio::test]
    async fn non_hello_first_frame() {
        let (a, b) = duplex(64 * 1024);
        let me = id(1);
        let a_task = tokio::spawn(async move {
            handshake(a, PeerRole::Dialer, me, "a:1".into(), &NoExisting).await
        });
        let (mut br, mut bw) = tokio::io::split(b);
        let _ = decode_frame(&mut br).await.unwrap();
        // Send a Heartbeat where Hello is required.
        let hb_payload = encode_payload(&classic_proto::HeartbeatPayload { seq: 1, send_time_ns: 0 })
            .unwrap();
        let hb = Frame::new(FrameKind::Heartbeat as u16, Bytes::from(hb_payload));
        encode_frame(&mut bw, &hb).await.unwrap();

        // Verify we sent an Error frame back.
        let resp = decode_frame(&mut br).await.unwrap();
        assert_eq!(resp.kind, FrameKind::Error as u16);
        let err: ErrorPayload = decode_payload(&resp.payload).unwrap();
        assert_eq!(err.code, ErrorCode::ProtocolViolation);

        let res = a_task.await.unwrap();
        assert_eq!(res.unwrap_err(), CloseReason::HandshakeRejected);
    }
}
