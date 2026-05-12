//! Synthetic file tree backing the 9P server. Materialized lazily from
//! a `NodeView` snapshot of `classic-ad`'s `NodeAd` plus
//! `classic-cap`'s `CapBroker`. Static fields (vendor / model / VRAM
//! total) are stable across reads; dynamic fields
//! (`/dev/gpu/<i>/in_use`, `vram_free_mb`) re-evaluate per read.
//!
//! Coverage in this commit (per the plan-06 §"File hierarchy" sketch):
//! - `/node` — local NodeId hex
//! - `/dev/gpu/<i>/{vendor, device, vram_total_mb, vram_free_mb, in_use}`
//! - `/svc/<name>` — symlink whose target encodes service NetIds
//!
//! Deferred to follow-ups: `/dev/pci/...` and `/proc/<MboxId>/...`
//! (the latter needs a ProcTable type classic-mbox doesn't expose yet).

use std::sync::Arc;

use classic_ad::AdStore;
use classic_cap::CapBroker;
use classic_proto::NodeId;

use crate::errno;
use crate::proto::types::{DirEntry, Qid, Stat};
use crate::server::tree::{NodeId as TreeNodeId, Tree, ROOT_NODE};

/// Stable `path` ids for the qid space. Real implementations would
/// hash the abstract path; we use coarse buckets so tests can be
/// explicit about which node is which.
mod path {
    pub const ROOT: u64 = 0;
    pub const NODE_FILE: u64 = 1;
    pub const DEV_DIR: u64 = 2;
    pub const DEV_GPU_DIR: u64 = 3;
    pub const SVC_DIR: u64 = 4;
    /// GPU entries start here: (`GPU_BASE + gpu_index * 16 + field_idx`).
    pub const GPU_BASE: u64 = 0x1000;
    /// Service symlinks start here.
    pub const SVC_BASE: u64 = 0x2000;
}

/// Snapshot of everything the tree exposes. The 9P server takes one
/// per attach; rebuild between attaches for fresh-cache semantics
/// (dynamic fields still re-read per access).
pub struct NodeView {
    pub node_id: NodeId,
    pub ad: Arc<AdStore>,
    pub caps: Arc<CapBroker>,
    /// Snapshot of `(name, vec_of_NetIds)` for live services. Captured
    /// at attach time so the symlink target list is stable for the
    /// duration of a fid's lifetime.
    pub services: Vec<(String, Vec<classic_proto::NetId>)>,
}

impl NodeView {
    pub fn new(node_id: NodeId, ad: Arc<AdStore>, caps: Arc<CapBroker>) -> Self {
        Self {
            node_id,
            ad,
            caps,
            services: Vec::new(),
        }
    }

    /// Convenience for tests / production: snapshot the live services
    /// (those known to `classic_mbox::snapshot()`) into the view.
    pub fn with_services(mut self) -> Self {
        let mut by_name: std::collections::BTreeMap<String, Vec<classic_proto::NetId>> =
            Default::default();
        for entry in classic_mbox::snapshot() {
            if entry.tombstone {
                continue;
            }
            by_name.entry(entry.name).or_default().push(entry.net_id);
        }
        self.services = by_name.into_iter().collect();
        self
    }
}

/// Production Tree backed by a `NodeView`. Cheap to clone — all heavy
/// state is in `Arc`s.
pub struct SyntheticTree {
    view: NodeView,
}

impl SyntheticTree {
    pub fn new(view: NodeView) -> Self {
        Self { view }
    }

    fn gpu_count(&self) -> usize {
        self.view.ad.self_ad().gpus.len()
    }

    /// Decode a GPU-field qid path into (gpu_index, field_idx).
    fn decode_gpu(path: u64) -> Option<(usize, usize)> {
        if path < path::GPU_BASE || path >= path::SVC_BASE {
            return None;
        }
        let offset = (path - path::GPU_BASE) as usize;
        Some((offset / 16, offset % 16))
    }

    fn gpu_field_name(field_idx: usize) -> Option<&'static str> {
        match field_idx {
            0 => Some("vendor"),
            1 => Some("device"),
            2 => Some("vram_total_mb"),
            3 => Some("vram_free_mb"),
            4 => Some("in_use"),
            _ => None,
        }
    }

    fn gpu_field_idx(name: &str) -> Option<usize> {
        match name {
            "vendor" => Some(0),
            "device" => Some(1),
            "vram_total_mb" => Some(2),
            "vram_free_mb" => Some(3),
            "in_use" => Some(4),
            _ => None,
        }
    }

    fn gpu_field_content(&self, gpu_index: usize, field_idx: usize) -> Option<Vec<u8>> {
        let ad = self.view.ad.self_ad();
        let gpu = ad.gpus.get(gpu_index)?;
        let content = match field_idx {
            0 => format!("0x{:04x}\n", gpu.pci_vendor),
            1 => format!("0x{:04x}\n", gpu.pci_device),
            2 => format!("{}\n", gpu.vram_total_mb),
            3 => format!("{}\n", gpu.vram_free_mb),
            4 => self.render_in_use(gpu_index as u32),
            _ => return None,
        };
        Some(content.into_bytes())
    }

    /// Per plan: `cat /dev/gpu/<i>/in_use` triggers a synchronous
    /// `CapBroker` snapshot and returns `0\n` (free) or `1 mbox=<id>\n`
    /// (held).
    fn render_in_use(&self, gpu_index: u32) -> String {
        for snap in self.view.caps.snapshot() {
            if matches!(snap.kind, classic_cap::DeviceKind::GpuMinor(idx) if idx == gpu_index) {
                return format!("1 mbox={}\n", snap.holder.0);
            }
        }
        "0\n".into()
    }

    fn service_at(&self, idx: usize) -> Option<&(String, Vec<classic_proto::NetId>)> {
        self.view.services.get(idx)
    }
}

impl Tree for SyntheticTree {
    fn walk_one(&self, parent: TreeNodeId, name: &str) -> Option<TreeNodeId> {
        match parent {
            // root → top-level children
            ROOT_NODE => match name {
                "node" => Some(path::NODE_FILE),
                "dev" => Some(path::DEV_DIR),
                "svc" => Some(path::SVC_DIR),
                _ => None,
            },
            // /dev → gpu (and eventually pci)
            id if id == path::DEV_DIR => match name {
                "gpu" => Some(path::DEV_GPU_DIR),
                _ => None,
            },
            // /dev/gpu → numeric gpu indices
            id if id == path::DEV_GPU_DIR => {
                let gpu_idx: usize = name.parse().ok()?;
                if gpu_idx < self.gpu_count() {
                    // Map to a dir-shaped qid: GPU_BASE + idx*16 + 15
                    // is reserved as the gpu-dir marker (different from
                    // the field offsets 0..=4).
                    Some(path::GPU_BASE + (gpu_idx as u64) * 16 + 15)
                } else {
                    None
                }
            }
            // /dev/gpu/<i> → field files
            id if id >= path::GPU_BASE && id < path::SVC_BASE => {
                // We arrive here only when parent is a gpu-dir
                // (path ending in +15). Strip that and route on name.
                let parent_idx = ((id - path::GPU_BASE) / 16) as usize;
                let field_idx = Self::gpu_field_idx(name)?;
                Some(path::GPU_BASE + (parent_idx as u64) * 16 + field_idx as u64)
            }
            // /svc → service-name symlinks
            id if id == path::SVC_DIR => {
                let idx = self
                    .view
                    .services
                    .iter()
                    .position(|(n, _)| n == name)?;
                Some(path::SVC_BASE + idx as u64)
            }
            _ => None,
        }
    }

    fn qid(&self, node: TreeNodeId) -> Option<Qid> {
        match node {
            ROOT_NODE => Some(Qid {
                type_: Qid::TYPE_DIR,
                version: 0,
                path: path::ROOT,
            }),
            n if n == path::NODE_FILE => Some(Qid {
                type_: Qid::TYPE_FILE,
                version: 0,
                path: n,
            }),
            n if n == path::DEV_DIR || n == path::DEV_GPU_DIR || n == path::SVC_DIR => {
                Some(Qid { type_: Qid::TYPE_DIR, version: 0, path: n })
            }
            n if n >= path::GPU_BASE && n < path::SVC_BASE => {
                // Field offset 15 means "the gpu's own dir"; 0..=4 are
                // the field files; anything else is unreachable.
                let off = (n - path::GPU_BASE) % 16;
                let type_ = if off == 15 {
                    Qid::TYPE_DIR
                } else {
                    Qid::TYPE_FILE
                };
                Some(Qid { type_, version: 0, path: n })
            }
            n if n >= path::SVC_BASE => Some(Qid {
                type_: Qid::TYPE_SYMLINK,
                version: 0,
                path: n,
            }),
            _ => None,
        }
    }

    fn stat(&self, node: TreeNodeId) -> Option<Stat> {
        let qid = self.qid(node)?;
        let mode = match qid.type_ {
            Qid::TYPE_DIR => 0o040555,
            Qid::TYPE_FILE => 0o100444,
            Qid::TYPE_SYMLINK => 0o120777,
            _ => 0o100444,
        };
        let size = match node {
            n if n == path::NODE_FILE => 33, // 32-hex + \n
            n if n >= path::GPU_BASE && n < path::SVC_BASE => {
                let off = (n - path::GPU_BASE) % 16;
                let gpu_idx = ((n - path::GPU_BASE) / 16) as usize;
                if off < 5 {
                    self.gpu_field_content(gpu_idx, off as usize)
                        .map(|v| v.len() as u64)
                        .unwrap_or(0)
                } else {
                    0
                }
            }
            n if n >= path::SVC_BASE => {
                let idx = (n - path::SVC_BASE) as usize;
                self.readlink(node)
                    .map(|s| s.len() as u64)
                    .unwrap_or_else(|| {
                        let _ = idx;
                        0
                    })
            }
            _ => 0,
        };
        Some(Stat {
            qid,
            mode,
            uid: 0,
            gid: 0,
            nlink: 1,
            size,
            ..Stat::default()
        })
    }

    fn read(&self, node: TreeNodeId, offset: u64, count: u32) -> Result<Vec<u8>, u32> {
        let payload: Vec<u8> = match node {
            n if n == path::NODE_FILE => {
                let mut s = String::with_capacity(33);
                for b in self.view.node_id.0.iter() {
                    s.push_str(&format!("{:02x}", b));
                }
                s.push('\n');
                s.into_bytes()
            }
            n if n >= path::GPU_BASE && n < path::SVC_BASE => {
                let (gpu_idx, field_idx) = Self::decode_gpu(n).ok_or(errno::ENOENT)?;
                if field_idx == 15 {
                    return Err(errno::EISDIR);
                }
                self.gpu_field_content(gpu_idx, field_idx)
                    .ok_or(errno::ENOENT)?
            }
            _ => return Err(errno::EISDIR),
        };
        let off = offset as usize;
        if off >= payload.len() {
            return Ok(Vec::new());
        }
        let end = (off + count as usize).min(payload.len());
        Ok(payload[off..end].to_vec())
    }

    fn readdir(&self, node: TreeNodeId, offset: u64) -> Result<Vec<DirEntry>, u32> {
        if offset > 0 {
            return Ok(Vec::new());
        }
        let entries: Vec<(Qid, u8, String)> = match node {
            ROOT_NODE => vec![
                (
                    Qid { type_: Qid::TYPE_DIR, version: 0, path: path::DEV_DIR },
                    4, // DT_DIR
                    "dev".into(),
                ),
                (
                    Qid { type_: Qid::TYPE_FILE, version: 0, path: path::NODE_FILE },
                    8, // DT_REG
                    "node".into(),
                ),
                (
                    Qid { type_: Qid::TYPE_DIR, version: 0, path: path::SVC_DIR },
                    4,
                    "svc".into(),
                ),
            ],
            n if n == path::DEV_DIR => vec![(
                Qid { type_: Qid::TYPE_DIR, version: 0, path: path::DEV_GPU_DIR },
                4,
                "gpu".into(),
            )],
            n if n == path::DEV_GPU_DIR => (0..self.gpu_count())
                .map(|i| {
                    (
                        Qid {
                            type_: Qid::TYPE_DIR,
                            version: 0,
                            path: path::GPU_BASE + (i as u64) * 16 + 15,
                        },
                        4,
                        format!("{i}"),
                    )
                })
                .collect(),
            n if n >= path::GPU_BASE
                && n < path::SVC_BASE
                && (n - path::GPU_BASE) % 16 == 15 =>
            {
                let gpu_idx = ((n - path::GPU_BASE) / 16) as usize;
                (0..5usize)
                    .filter_map(|f| {
                        let name = Self::gpu_field_name(f)?;
                        Some((
                            Qid {
                                type_: Qid::TYPE_FILE,
                                version: 0,
                                path: path::GPU_BASE + (gpu_idx as u64) * 16 + f as u64,
                            },
                            8,
                            name.into(),
                        ))
                    })
                    .collect()
            }
            n if n == path::SVC_DIR => self
                .view
                .services
                .iter()
                .enumerate()
                .map(|(i, (name, _))| {
                    (
                        Qid {
                            type_: Qid::TYPE_SYMLINK,
                            version: 0,
                            path: path::SVC_BASE + i as u64,
                        },
                        10, // DT_LNK
                        name.clone(),
                    )
                })
                .collect(),
            _ => return Err(errno::ENOTDIR),
        };

        // Sort lexicographic per plan §"Treaddir order".
        let mut sorted: Vec<(Qid, u8, String)> = entries;
        sorted.sort_by(|a, b| a.2.cmp(&b.2));

        Ok(sorted
            .into_iter()
            .enumerate()
            .map(|(idx, (qid, ty, name))| DirEntry {
                qid,
                offset: (idx as u64) + 1,
                ty,
                name,
            })
            .collect())
    }

    fn readlink(&self, node: TreeNodeId) -> Option<String> {
        if node < path::SVC_BASE {
            return None;
        }
        let idx = (node - path::SVC_BASE) as usize;
        let (_, net_ids) = self.service_at(idx)?;
        let mut s = String::new();
        for nid in net_ids {
            let mut hex = String::with_capacity(32);
            for b in nid.node.0.iter() {
                hex.push_str(&format!("{:02x}", b));
            }
            s.push_str(&format!("node={} mbox={}\n", hex, nid.mbox.0));
        }
        Some(s)
    }

    fn is_dir(&self, node: TreeNodeId) -> bool {
        if node == ROOT_NODE
            || node == path::DEV_DIR
            || node == path::DEV_GPU_DIR
            || node == path::SVC_DIR
        {
            return true;
        }
        node >= path::GPU_BASE
            && node < path::SVC_BASE
            && (node - path::GPU_BASE) % 16 == 15
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_cap::{CapBroker, DeviceKind};
    use classic_proto::MboxId;

    fn nid(byte: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = byte;
        NodeId(b)
    }

    fn ad_with_gpu(count: usize) -> Arc<AdStore> {
        // Build a NodeAd ourselves and feed it into a fresh AdStore.
        // Use plan-03's local NodeAd model — that's what classic-place
        // exposes — except this synthetic tree consumes classic-ad's
        // NodeAd, not the place-side model. classic-ad re-exports
        // NodeAd / CpuAd / etc. via its lib.
        let mut gpu = Vec::new();
        for i in 0..count {
            gpu.push(classic_ad::GpuInfo {
                index: i as u32,
                uuid: format!("GPU-{i}"),
                name: "NVIDIA H100".into(),
                pci_vendor: 0x10de,
                pci_device: 0x2330,
                pci_addr: format!("0000:{:02x}:00.0", i + 1),
                vram_total_mb: 81920,
                vram_free_mb: 80000,
                compute_capability: (9, 0),
                nvlink_peers: vec![],
                utilization_pct: 0,
            });
        }
        let ad = classic_ad::NodeAd {
            node_id: nid(1),
            hostname: "host-1".into(),
            proto_version: 1,
            generation: 1,
            boot_time: 0,
            cpu: classic_ad::CpuInfo {
                cores_online: 32,
                cores_physical: 16,
                sockets: 1,
                model: "Test".into(),
                vendor: "TestVendor".into(),
                arch: "x86_64".into(),
                mhz: 3000,
            },
            mem: classic_ad::MemInfo { total_mb: 65536, available_mb: 60000 },
            gpus: gpu,
            pci: vec![],
            numa: vec![],
            load: classic_ad::LoadSample {
                loadavg_1m: 0,
                loadavg_5m: 0,
                loadavg_15m: 0,
                cpu_pct: 0,
                mem_pct: 0,
                task_count: 0,
            },
        };
        Arc::new(AdStore::new(ad))
    }

    fn view_with_gpus(count: usize) -> NodeView {
        NodeView::new(nid(1), ad_with_gpu(count), Arc::new(CapBroker::new()))
    }

    #[test]
    fn root_has_three_entries() {
        let t = SyntheticTree::new(view_with_gpus(0));
        let entries = t.readdir(ROOT_NODE, 0).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["dev", "node", "svc"]); // lex-sorted
    }

    #[test]
    fn node_file_renders_node_id_hex_with_newline() {
        let t = SyntheticTree::new(view_with_gpus(0));
        let bytes = t.read(path::NODE_FILE, 0, 100).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("01"), "got {s:?}");
        assert!(s.ends_with('\n'));
        assert_eq!(s.len(), 33);
    }

    #[test]
    fn dev_gpu_dir_lists_numeric_indices() {
        let t = SyntheticTree::new(view_with_gpus(3));
        let entries = t.readdir(path::DEV_GPU_DIR, 0).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["0", "1", "2"]);
    }

    #[test]
    fn dev_gpu_field_files_render_correctly() {
        let t = SyntheticTree::new(view_with_gpus(1));
        // Walk into /dev/gpu/0
        let dev = t.walk_one(ROOT_NODE, "dev").unwrap();
        let gpu = t.walk_one(dev, "gpu").unwrap();
        let g0 = t.walk_one(gpu, "0").unwrap();
        let vendor = t.walk_one(g0, "vendor").unwrap();
        let vram = t.walk_one(g0, "vram_total_mb").unwrap();
        let in_use = t.walk_one(g0, "in_use").unwrap();

        assert_eq!(t.read(vendor, 0, 100).unwrap(), b"0x10de\n");
        assert_eq!(t.read(vram, 0, 100).unwrap(), b"81920\n");
        // No caps held: in_use is "0\n".
        assert_eq!(t.read(in_use, 0, 100).unwrap(), b"0\n");
    }

    #[test]
    fn in_use_reflects_cap_broker_state() {
        let caps = Arc::new(CapBroker::new());
        let _held = caps
            .acquire(MboxId(42), &[DeviceKind::GpuMinor(0)], true)
            .unwrap();
        let view = NodeView::new(nid(1), ad_with_gpu(1), caps);
        let t = SyntheticTree::new(view);
        // Walk to /dev/gpu/0/in_use directly.
        let g0 = t.walk_one(t.walk_one(t.walk_one(ROOT_NODE, "dev").unwrap(), "gpu").unwrap(), "0").unwrap();
        let in_use = t.walk_one(g0, "in_use").unwrap();
        let bytes = t.read(in_use, 0, 100).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("1 mbox=42"), "got {s:?}");
    }

    #[test]
    fn svc_symlink_target_encodes_net_ids() {
        let mut view = view_with_gpus(0);
        view.services.push((
            "registry".into(),
            vec![classic_proto::NetId { node: nid(5), mbox: MboxId(7) }],
        ));
        let t = SyntheticTree::new(view);
        let svc = t.walk_one(ROOT_NODE, "svc").unwrap();
        let link = t.walk_one(svc, "registry").unwrap();
        let target = t.readlink(link).unwrap();
        assert!(target.contains("node="));
        assert!(target.contains("mbox=7"));
    }

    #[test]
    fn lookup_unknown_path_returns_none() {
        let t = SyntheticTree::new(view_with_gpus(0));
        assert!(t.walk_one(ROOT_NODE, "bogus").is_none());
        let dev = t.walk_one(ROOT_NODE, "dev").unwrap();
        assert!(t.walk_one(dev, "bogus").is_none());
    }

    #[test]
    fn stat_modes_match_plan() {
        let mut view = view_with_gpus(1);
        view.services.push((
            "alpha".into(),
            vec![classic_proto::NetId { node: nid(2), mbox: MboxId(1) }],
        ));
        let t = SyntheticTree::new(view);
        // /node — file
        assert_eq!(t.stat(path::NODE_FILE).unwrap().mode, 0o100444);
        // /dev — dir
        assert_eq!(t.stat(path::DEV_DIR).unwrap().mode, 0o040555);
        // /svc/alpha — symlink
        let svc = t.walk_one(ROOT_NODE, "svc").unwrap();
        let alpha = t.walk_one(svc, "alpha").unwrap();
        assert_eq!(t.stat(alpha).unwrap().mode, 0o120777);
    }
}

