pub mod conn;
pub mod frame;
pub mod ids;
pub mod mux;
pub mod proto;
pub mod version;

pub use conn::Connection;
pub use frame::{decode_frame, encode_frame, CodecError, Frame, FrameKind, MAX_FRAME_SIZE};
pub use ids::{MboxId, NetId, NodeId};
pub use proto::{
    decode_payload, encode_payload, ByePayload, ErrorCode, ErrorPayload, HeartbeatPayload,
    HelloPayload,
};
pub use version::PROTO_VERSION;
