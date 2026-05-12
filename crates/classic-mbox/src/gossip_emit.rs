//! Outbound-broadcast sink for service gossip. Decoupled from the
//! gossip engine itself so tests can capture emitted frames without
//! standing up a real Peers backend, and so the daemon can wire either
//! the cross-node `Peers` adapter or a unit-test capture impl.
//!
//! The gossip engine calls `emit(frame)`; production wires a callback
//! that fans the frame out to every live peer via `Peers::send_to`.
//! Until a callback is installed, `emit` is a no-op (with a counter so
//! tests can observe).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use classic_proto::Frame;

type Sink = Box<dyn Fn(Frame) + Send + Sync>;

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

fn sink() -> &'static Mutex<Option<Sink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// Install (or replace) the broadcast sink. Production callers wire
/// this to fan a frame out across every live peer.
pub fn set_sink<F: Fn(Frame) + Send + Sync + 'static>(f: F) {
    *sink().lock().expect("emit sink poisoned") = Some(Box::new(f));
}

/// Test helper: drop any installed sink so subsequent emits go to /dev/null.
pub fn clear_sink() {
    *sink().lock().expect("emit sink poisoned") = None;
}

pub static UNSUNK_FRAMES: AtomicU64 = AtomicU64::new(0);

pub fn emit(frame: Frame) {
    let guard = sink().lock().expect("emit sink poisoned");
    match &*guard {
        Some(sink) => sink(frame),
        None => {
            UNSUNK_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
    }
}
