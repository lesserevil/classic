//! Pure-read helpers for NVIDIA-related device discovery. No NVML, no
//! probing — just enumerate `/dev/nvidia<N>` minors that exist right now.

use std::path::Path;

const DEV: &str = "/dev";

/// Returns sorted minors `N` for which `/dev/nvidia<N>` exists. Returns
/// an empty `Vec` if `/dev` is missing or no nvidia nodes are present.
/// Never panics.
///
/// `/dev/nvidia-uvm`, `/dev/nvidia-modeset`, and the control nodes
/// (`/dev/nvidiactl`) are deliberately not returned — only numbered
/// device nodes that map to GPU minor numbers the BPF allowlist gates
/// per `DeviceKind::GpuMinor(N)`.
pub fn list_nvidia_minors() -> Vec<u32> {
    let read_dir = match std::fs::read_dir(Path::new(DEV)) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if let Some(rest) = name.strip_prefix("nvidia") {
                // Numbered nodes look like "nvidia0", "nvidia7" — pure digits
                // after the prefix. Skip "nvidia-uvm", "nvidiactl", etc.
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(n) = rest.parse::<u32>() {
                        out.push(n);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_nvidia_minors_never_panics() {
        // Whatever `/dev` has, this should produce a Vec without panicking.
        let _ = list_nvidia_minors();
    }
}
