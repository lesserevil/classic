use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

/// Resolve a future when SIGTERM or SIGINT arrives. Used by `run()` to drive
/// the shutdown branch.
pub async fn wait_for_signal() -> std::io::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut intr = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => info!("received SIGTERM; shutting down"),
        _ = intr.recv() => info!("received SIGINT; shutting down"),
    }
    Ok(())
}

/// Default grace period a clean shutdown uses to flush Bye frames before
/// dropping connections.
pub const DEFAULT_BYE_GRACE: Duration = Duration::from_secs(1);
