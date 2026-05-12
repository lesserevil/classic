//! `classic submit <group.toml>` subcommand.
//!
//! v1 scope:
//! - Parse the TOML into a `PlacementGroup` (eager predicate parse,
//!   label uniqueness check).
//! - Fetch ads from the local daemon's admin socket.
//! - Run `classic_place::place_group` to compute the assignment.
//! - Print `{label}  -> {short-node-id}` per member; exit 0.
//!
//! End-to-end 2PC submission (Phase-1 reserve, Phase-2 commit through
//! the node-side reservation table) requires a real cross-node
//! transport that the daemon doesn't yet expose. Once that wire path
//! lands, this subcommand will swap from `place_group` to
//! `classic_spawn::submit_group` without changing the user-facing
//! contract — the assignment lines are already the public output.

use std::path::Path;
use std::path::PathBuf;

use classic_place::{place_group, GpuAd, GroupPlaceError, NodeAd as PlaceNodeAd};
use classic_proto::NodeId;

use crate::group_toml::{parse_group_toml, GroupTomlError};

#[derive(clap::Args, Debug)]
pub struct SubmitArgs {
    /// Path to the group.toml file.
    pub path: PathBuf,
}

/// Top-level submit error. Each variant prints with a stable
/// "classic submit: …" prefix at the CLI's main error handler.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("submit: {0}")]
    Toml(#[from] GroupTomlError),
    #[error("submit: ad fetch: {0}")]
    AdFetch(String),
    #[error("submit: placement: {0}")]
    Place(#[from] GroupPlaceError),
}

/// Adapt classic-ad's NodeAd (cluster wire shape) into classic-place's
/// NodeAd (predicate-eval shape). The two structs name fields
/// differently — classic-ad uses `gpus`/`vram_total_mb` etc., classic-
/// place uses `gpu`/`vram_mb` — so the adapter is unavoidable.
fn adapt_ad(ad: &classic_ad::NodeAd) -> PlaceNodeAd {
    let mut out = PlaceNodeAd::default();
    out.node_id = ad.node_id;
    out.hostname = ad.hostname.clone();
    out.gen = ad.generation;
    out.cpu.cores = ad.cpu.cores_online;
    out.cpu.threads = ad.cpu.cores_online;
    out.cpu.arch = ad.cpu.arch.clone();
    out.cpu.model = ad.cpu.model.clone();
    out.mem.total_mb = ad.mem.total_mb;
    out.mem.free_mb = ad.mem.available_mb;
    out.load.cpu_pct = ad.load.cpu_pct as f64;
    out.load.mem_pct = ad.load.mem_pct as f64;
    out.load.load_1m = ad.load.loadavg_1m as f64 / 1000.0;
    out.gpu = ad
        .gpus
        .iter()
        .map(|g| GpuAd {
            index: g.index,
            vendor: g.pci_vendor as u32,
            device: g.pci_device as u32,
            model: g.name.clone(),
            vram_mb: g.vram_total_mb,
            vram_free_mb: g.vram_free_mb,
            sm_count: 0,
            in_use: g.utilization_pct >= 50, // crude proxy; refined when caps land
            mig: false,
        })
        .collect();
    out
}

/// Hex-render a NodeId. Matches the format the rest of the CLI uses
/// for ad listings.
fn format_node_id(n: &NodeId) -> String {
    let mut s = String::with_capacity(33);
    for b in n.0 {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{:02x}", b);
    }
    // Print just the short prefix — easier to eyeball when many
    // members land on the same node.
    s.truncate(12);
    s
}

/// Run the `submit` subcommand. Returns Ok(()) on success (prints to
/// stdout); errors surface to main.rs's error handler.
pub async fn run_submit(state_dir: &Path, args: SubmitArgs) -> Result<(), SubmitError> {
    let group = parse_group_toml(&args.path)?;
    let ads = fetch_ads(state_dir).await?;
    let placement = place_group(&group, &ads)?;
    for (label, node_id) in placement {
        println!("{label}  -> {}", format_node_id(&node_id));
    }
    Ok(())
}

async fn fetch_ads(state_dir: &Path) -> Result<Vec<PlaceNodeAd>, SubmitError> {
    use classic_node::control::{self, Request};
    let v = control::send_request(state_dir, &Request::AdList)
        .await
        .map_err(|e| SubmitError::AdFetch(e.to_string()))?;
    // Server emits {"AdList": {"ads": [...]}} via serde's untagged
    // pattern. Project the `ads` array out by hand to keep the CLI
    // independent of any future re-shaping.
    let raw = v
        .get("ads")
        .and_then(|a| a.as_array())
        .ok_or_else(|| SubmitError::AdFetch("daemon returned no ads array".into()))?;
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let ad: classic_ad::NodeAd = serde_json::from_value(item.clone())
            .map_err(|e| SubmitError::AdFetch(format!("decode ad: {e}")))?;
        out.push(adapt_ad(&ad));
    }
    Ok(out)
}
