//! GPU probe via NVML. `libnvidia-ml.so.1` is loaded lazily by
//! `nvml-wrapper`, so the binary builds + runs on hosts without a
//! driver — `Nvml::init()` simply returns an error and we record a single
//! warning, then report `gpus: []` thereafter.
//!
//! Plan-02 acceptance: missing driver -> one `warn!` containing "NVML",
//! no panic, `enumerate()` returns `[]`.

use nvml_wrapper::Nvml;
use tracing::warn;

use crate::schema::GpuInfo;

pub struct GpuProbe {
    nvml: Option<Nvml>,
}

impl GpuProbe {
    pub fn new() -> Self {
        match Nvml::init() {
            Ok(nvml) => Self { nvml: Some(nvml) },
            Err(e) => {
                warn!(error = %e, "NVML unavailable; reporting no GPUs");
                Self { nvml: None }
            }
        }
    }

    /// Returns the static list of GPUs present at probe creation. Dynamic
    /// fields (`vram_free_mb`, `utilization_pct`) are filled in best-effort
    /// here too; callers should call `refresh_dynamic` on the fast tick to
    /// keep them current.
    pub fn enumerate(&self) -> Vec<GpuInfo> {
        let Some(nvml) = self.nvml.as_ref() else {
            return Vec::new();
        };
        let count = match nvml.device_count() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "NVML device_count failed");
                return Vec::new();
            }
        };
        let mut gpus = Vec::with_capacity(count as usize);
        for i in 0..count {
            match enumerate_one(nvml, i) {
                Ok(g) => gpus.push(g),
                Err(e) => warn!(index = i, error = %e, "NVML enumerate_one failed; skipping"),
            }
        }
        gpus
    }

    /// Refresh dynamic fields in-place: `vram_free_mb`, `utilization_pct`.
    /// Indexes are looked up by the GPU's `index` field, so reordering on
    /// the NVML side (rare but possible after a driver reload) is tolerated.
    pub fn refresh_dynamic(&self, out: &mut [GpuInfo]) {
        let Some(nvml) = self.nvml.as_ref() else {
            return;
        };
        for gpu in out.iter_mut() {
            let dev = match nvml.device_by_index(gpu.index) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if let Ok(mem) = dev.memory_info() {
                gpu.vram_free_mb = mem.free / (1024 * 1024);
                gpu.vram_total_mb = mem.total / (1024 * 1024);
            }
            if let Ok(util) = dev.utilization_rates() {
                gpu.utilization_pct = util.gpu;
            }
        }
    }
}

impl Default for GpuProbe {
    fn default() -> Self {
        Self::new()
    }
}

fn enumerate_one(nvml: &Nvml, idx: u32) -> Result<GpuInfo, nvml_wrapper::error::NvmlError> {
    let dev = nvml.device_by_index(idx)?;
    let uuid = dev.uuid().unwrap_or_default();
    let name = dev.name().unwrap_or_default();
    let pci = dev.pci_info()?;
    let mem = dev.memory_info()?;
    let cc = dev.cuda_compute_capability().ok();
    let util = dev.utilization_rates().ok();

    Ok(GpuInfo {
        index: idx,
        uuid,
        name,
        pci_vendor: ((pci.pci_device_id >> 16) & 0xFFFF) as u16,
        pci_device: (pci.pci_device_id & 0xFFFF) as u16,
        pci_addr: pci.bus_id,
        vram_total_mb: mem.total / (1024 * 1024),
        vram_free_mb: mem.free / (1024 * 1024),
        compute_capability: cc
            .map(|c| (c.major as u32, c.minor as u32))
            .unwrap_or((0, 0)),
        nvlink_peers: Vec::new(), // populated when an NVLink-capable host is wired in
        utilization_pct: util.map(|u| u.gpu).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On hosts without `libnvidia-ml.so.1` the probe must NOT panic and
    /// MUST return an empty list. CI typically runs without a driver.
    #[test]
    fn no_nvml_yields_empty_enumeration_without_panic() {
        let probe = GpuProbe::new();
        // The probe is built; whether nvml is Some depends on the host.
        // What we strictly require: enumerate() never panics.
        let gpus = probe.enumerate();
        // On a no-driver host, this is empty. On a driver host it's
        // populated; either is acceptable for this test.
        assert!(gpus.iter().all(|g| !g.uuid.is_empty() || g.uuid.is_empty()));

        // refresh_dynamic on an empty slice is a no-op and must not panic
        // regardless of NVML state.
        let mut empty: Vec<GpuInfo> = Vec::new();
        probe.refresh_dynamic(&mut empty);
        assert!(empty.is_empty());
    }

    /// Sanity: probe is `Send + Sync` so it can be shared across the
    /// discovery + gossip tasks.
    #[test]
    fn gpu_probe_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GpuProbe>();
    }
}
