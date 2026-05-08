use std::sync::{Arc, RwLock};

use tracing::warn;

use crate::frame::Frame;
use crate::ids::NodeId;

/// Number of dispatch slots. Each slot owns one full `0x0N00..=0x0NFF` range
/// of `FrameKind` values, indexed by the high byte of `frame.kind`. Eight
/// slots cover `0x0000..=0x07FF` — every range allocated in
/// ARCHITECTURE.md plus one reserved.
pub const MUX_SLOTS: usize = 8;

/// Dispatch target for a single frame-kind range.
///
/// `on_frame` runs synchronously on the dispatch path, so implementors must
/// not block — offload onto the implementor's own task / channel. The trait
/// is `'static` so handlers can be stored as `Arc<dyn FrameHandler>` without
/// borrowing.
pub trait FrameHandler: Send + Sync + 'static {
    fn on_frame(&self, peer: NodeId, frame: Frame);
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("range high byte {0:#04x} out of supported range 0x00..=0x07")]
    RangeOutOfBounds(u8),
}

/// Range-keyed frame dispatcher. Each slot is an `RwLock<Option<Arc<...>>>`,
/// so the hot path (`dispatch`) takes a read lock long enough to clone the
/// `Arc` (uncontended) and releases it before invoking the handler — handlers
/// can reentrantly call `register` without deadlocking.
pub struct FrameMux {
    handlers: [RwLock<Option<Arc<dyn FrameHandler>>>; MUX_SLOTS],
}

impl FrameMux {
    pub fn new() -> Self {
        Self {
            handlers: std::array::from_fn(|_| RwLock::new(None)),
        }
    }

    /// Install (or replace) the handler for the `0x0N00..=0x0NFF` range
    /// where `N == range_high_byte`. Returns an error for high bytes
    /// `>= MUX_SLOTS` rather than panicking — out-of-range registrations are
    /// a programmer error but are also reachable from configuration paths,
    /// so callers get a chance to surface the problem.
    pub fn register(
        &self,
        range_high_byte: u8,
        handler: Arc<dyn FrameHandler>,
    ) -> Result<(), MuxError> {
        let idx = range_high_byte as usize;
        if idx >= MUX_SLOTS {
            return Err(MuxError::RangeOutOfBounds(range_high_byte));
        }
        *self.handlers[idx].write().expect("FrameMux slot poisoned") = Some(handler);
        Ok(())
    }

    /// Remove the handler for `range_high_byte`, returning the previous one
    /// if any. Used in tests; the runtime never deregisters in v1.
    pub fn deregister(&self, range_high_byte: u8) -> Option<Arc<dyn FrameHandler>> {
        let idx = range_high_byte as usize;
        if idx >= MUX_SLOTS {
            return None;
        }
        self.handlers[idx]
            .write()
            .expect("FrameMux slot poisoned")
            .take()
    }

    /// Dispatch `frame` to whatever handler owns its range. A frame whose
    /// high byte is outside the supported window, or whose slot is empty,
    /// is logged at warn level and dropped — the connection stays open.
    /// Plan 01 §FR-9: unknown kinds are NOT a protocol violation.
    pub fn dispatch(&self, peer: NodeId, frame: Frame) {
        let high = (frame.kind >> 8) as usize;
        if high >= MUX_SLOTS {
            warn!(
                peer = %peer,
                kind = format!("{:#06x}", frame.kind),
                "frame kind outside dispatchable range; dropped"
            );
            return;
        }
        // Clone the Arc out under the read lock, then drop the lock before
        // invoking the handler so handlers may freely re-enter the mux.
        let handler = self.handlers[high]
            .read()
            .expect("FrameMux slot poisoned")
            .clone();
        match handler {
            Some(h) => h.on_frame(peer, frame),
            None => warn!(
                peer = %peer,
                kind = format!("{:#06x}", frame.kind),
                range = format!("{:#04x}", high as u8),
                "no handler registered for frame range; dropped"
            ),
        }
    }
}

impl Default for FrameMux {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Recorder {
        seen: Mutex<Vec<(NodeId, u16)>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self { seen: Mutex::new(Vec::new()) })
        }
        fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl FrameHandler for Recorder {
        fn on_frame(&self, peer: NodeId, frame: Frame) {
            self.seen.lock().unwrap().push((peer, frame.kind));
        }
    }

    fn frame(kind: u16) -> Frame {
        Frame::new(kind, Bytes::new())
    }

    fn peer() -> NodeId {
        NodeId([9u8; 16])
    }

    #[test]
    fn register_and_dispatch() {
        let mux = FrameMux::new();
        let rec = Recorder::new();
        mux.register(0x01, rec.clone()).unwrap();
        mux.dispatch(peer(), frame(0x0123));
        assert_eq!(rec.count(), 1);
        assert_eq!(rec.seen.lock().unwrap()[0], (peer(), 0x0123));
    }

    #[test]
    fn unknown_range_dropped() {
        let mux = FrameMux::new();
        // Slot 0x07 is in-range but empty.
        mux.dispatch(peer(), frame(0x0723));
        // Slot 0x08 is out-of-range entirely.
        mux.dispatch(peer(), frame(0x0823));
        // Did not panic, did not error. (Log-only is the spec.)
    }

    #[test]
    fn multiple_ranges() {
        let mux = FrameMux::new();
        let r1 = Recorder::new();
        let r2 = Recorder::new();
        let r5 = Recorder::new();
        mux.register(0x01, r1.clone()).unwrap();
        mux.register(0x02, r2.clone()).unwrap();
        mux.register(0x05, r5.clone()).unwrap();

        mux.dispatch(peer(), frame(0x0100));
        mux.dispatch(peer(), frame(0x01FF));
        mux.dispatch(peer(), frame(0x0200));
        mux.dispatch(peer(), frame(0x0500));
        mux.dispatch(peer(), frame(0x0600)); // unhandled, dropped

        assert_eq!(r1.count(), 2);
        assert_eq!(r2.count(), 1);
        assert_eq!(r5.count(), 1);
    }

    #[test]
    fn replace_handler() {
        let mux = FrameMux::new();
        let first = Recorder::new();
        let second = Recorder::new();
        mux.register(0x01, first.clone()).unwrap();
        mux.dispatch(peer(), frame(0x0100));
        mux.register(0x01, second.clone()).unwrap();
        mux.dispatch(peer(), frame(0x0101));
        assert_eq!(first.count(), 1);
        assert_eq!(second.count(), 1);
        assert_eq!(second.seen.lock().unwrap()[0].1, 0x0101);
    }

    #[test]
    fn out_of_range_register_errors() {
        let mux = FrameMux::new();
        let rec = Recorder::new();
        let err = mux.register(0x08, rec).unwrap_err();
        assert!(matches!(err, MuxError::RangeOutOfBounds(0x08)));
    }

    #[test]
    fn deregister_returns_previous() {
        let mux = FrameMux::new();
        let rec = Recorder::new();
        mux.register(0x01, rec.clone()).unwrap();
        let prev = mux.deregister(0x01);
        assert!(prev.is_some());
        // After deregister, dispatching that range no longer reaches the handler.
        mux.dispatch(peer(), frame(0x0100));
        assert_eq!(rec.count(), 0);
    }

    /// Concurrent dispatch from many threads while another thread keeps
    /// swapping handlers: must not panic, must not race. We don't assert
    /// counts on the in-place handlers (they can be swapped out mid-flight);
    /// a single AtomicUsize counted across all generations is what we check.
    #[test]
    fn concurrent_dispatch_and_register() {
        struct AtomicHandler(Arc<AtomicUsize>);
        impl FrameHandler for AtomicHandler {
            fn on_frame(&self, _peer: NodeId, _frame: Frame) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mux = Arc::new(FrameMux::new());
        let total = Arc::new(AtomicUsize::new(0));

        // Pre-install a handler so the very first dispatch lands somewhere.
        mux.register(0x01, Arc::new(AtomicHandler(total.clone())))
            .unwrap();

        let dispatchers: Vec<_> = (0..4)
            .map(|_| {
                let mux = mux.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        mux.dispatch(peer(), frame(0x0142));
                    }
                })
            })
            .collect();

        let registrar = {
            let mux = mux.clone();
            let total = total.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    mux.register(0x01, Arc::new(AtomicHandler(total.clone())))
                        .unwrap();
                }
            })
        };

        for d in dispatchers {
            d.join().unwrap();
        }
        registrar.join().unwrap();

        // Every dispatch landed on *some* handler instance — none were lost
        // to a torn swap, since ArcSwapOption guarantees an atomic load.
        assert_eq!(total.load(Ordering::Relaxed), 4 * 1000);
    }
}
