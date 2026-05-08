pub mod conn;
pub mod frame;
pub mod ids;
pub mod mux;
pub mod proto;
pub mod spawn;
pub mod version;

pub use conn::Connection;
pub use frame::{decode_frame, encode_frame, CodecError, Frame, FrameKind, MAX_FRAME_SIZE};
pub use ids::{MboxId, NetId, NodeId};
pub use mux::{FrameHandler, FrameMux, MuxError, MUX_SLOTS};
pub use proto::{
    decode_payload, encode_payload, ByePayload, ErrorCode, ErrorPayload, HeartbeatPayload,
    HelloPayload,
};
pub use spawn::{
    ChildExit, ChildStdio, DenyReason, SpawnAck, SpawnDeny, SpawnRequest, StdinKind,
    StdioStream, MAX_HOPS,
};
pub use version::PROTO_VERSION;
