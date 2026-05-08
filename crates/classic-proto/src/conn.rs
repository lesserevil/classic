use async_trait::async_trait;

use crate::frame::{CodecError, Frame};
use crate::ids::NodeId;

/// An established peer connection. Implementors are owned by `classic-node`
/// (the `PeerLink` actor) and handed to subsystems through `Arc<dyn Connection>`,
/// so the trait must remain object-safe and `Send + Sync`.
///
/// `recv` takes `&mut self` because exactly one task drives the read side of a
/// link; multiple writers may share a connection through `Arc`, hence `send`
/// takes `&self`. Implementors typically guard the underlying writer with a
/// mutex.
#[async_trait]
pub trait Connection: Send + Sync {
    async fn send(&self, frame: Frame) -> Result<(), CodecError>;
    async fn recv(&mut self) -> Result<Frame, CodecError>;
    fn peer(&self) -> NodeId;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Mutex;

    struct DummyConn {
        peer: NodeId,
        outbox: Mutex<Vec<Frame>>,
        inbox: Mutex<Vec<Frame>>,
    }

    #[async_trait]
    impl Connection for DummyConn {
        async fn send(&self, frame: Frame) -> Result<(), CodecError> {
            self.outbox.lock().unwrap().push(frame);
            Ok(())
        }
        async fn recv(&mut self) -> Result<Frame, CodecError> {
            self.inbox
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| CodecError::Decode("inbox empty".into()))
        }
        fn peer(&self) -> NodeId {
            self.peer
        }
    }

    #[test]
    fn connection_is_object_safe() {
        let _b: Box<dyn Connection> = Box::new(DummyConn {
            peer: NodeId([1u8; 16]),
            outbox: Mutex::new(Vec::new()),
            inbox: Mutex::new(vec![Frame::new(0x0002, Bytes::new())]),
        });
    }
}
