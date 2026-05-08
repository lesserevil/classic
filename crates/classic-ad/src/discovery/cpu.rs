//! Static CPU info probe. Reads `/proc/cpuinfo` for vendor / model / freq
//! and de-duplicates `(physical id, core id)` pairs to recover physical
//! core and socket counts.
//!
//! Online core count comes from `nproc` (`sysconf(_SC_NPROCESSORS_ONLN)`).
//! We can't get that through `Sysroot` without a sysconf wrapper, so the
//! probe takes it as an argument — discovery's caller obtains it once at
//! probe time.

use std::collections::HashSet;
use std::path::Path;

use crate::discovery::{read_string, Sysroot};
use crate::schema::CpuInfo;

pub fn probe<S: Sysroot + ?Sized>(sr: &S, cores_online: u32) -> std::io::Result<CpuInfo> {
    let raw = read_string(sr, Path::new("proc/cpuinfo"))?;
    Ok(parse(&raw, cores_online))
}

fn parse(raw: &str, cores_online: u32) -> CpuInfo {
    let mut model = String::new();
    let mut vendor = String::new();
    let mut mhz: u32 = 0;
    let mut cur_phys: Option<u32> = None;
    let mut cur_core: Option<u32> = None;
    let mut sockets: HashSet<u32> = HashSet::new();
    let mut phys_cores: HashSet<(u32, u32)> = HashSet::new();
    let mut first_processor = true;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // End of one logical processor's record.
            if let (Some(p), Some(c)) = (cur_phys, cur_core) {
                phys_cores.insert((p, c));
                sockets.insert(p);
            }
            cur_phys = None;
            cur_core = None;
            first_processor = false;
            continue;
        }
        let (key, value) = match trimmed.split_once(':') {
            Some(kv) => (kv.0.trim(), kv.1.trim()),
            None => continue,
        };
        match key {
            "vendor_id" if first_processor && vendor.is_empty() => vendor = value.to_string(),
            "model name" if first_processor && model.is_empty() => model = value.to_string(),
            "cpu MHz" if first_processor && mhz == 0 => {
                mhz = value
                    .split('.')
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
            }
            "physical id" => cur_phys = value.parse().ok(),
            "core id" => cur_core = value.parse().ok(),
            _ => {}
        }
    }
    // Final block (file may not end with a blank line).
    if let (Some(p), Some(c)) = (cur_phys, cur_core) {
        phys_cores.insert((p, c));
        sockets.insert(p);
    }

    let cores_physical = if phys_cores.is_empty() {
        cores_online
    } else {
        phys_cores.len() as u32
    };
    let socket_count = if sockets.is_empty() { 1 } else { sockets.len() as u32 };

    CpuInfo {
        cores_online,
        cores_physical,
        sockets: socket_count,
        model,
        vendor,
        arch: std::env::consts::ARCH.to_string(),
        mhz,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;

    /// Two-socket Xeon-like fixture: 8 logical cores per package, 2 packages.
    /// Each package has 4 physical cores with 2 SMT threads each.
    fn xeon_fixture() -> String {
        let mut buf = String::new();
        for cpu in 0..16 {
            let phys = (cpu / 8) as u32;
            let core = ((cpu % 8) / 2) as u32;
            buf.push_str(&format!(
                "processor\t: {cpu}\n\
                 vendor_id\t: GenuineIntel\n\
                 model name\t: Intel(R) Xeon(R) Platinum 8480+\n\
                 physical id\t: {phys}\n\
                 core id\t: {core}\n\
                 cpu MHz\t\t: 2700.000\n\n"
            ));
        }
        buf
    }

    /// Single-socket Ryzen 9 7950X (16 cores, 32 threads).
    fn ryzen_fixture() -> String {
        let mut buf = String::new();
        for cpu in 0..32 {
            let core = (cpu / 2) as u32;
            buf.push_str(&format!(
                "processor\t: {cpu}\n\
                 vendor_id\t: AuthenticAMD\n\
                 model name\t: AMD Ryzen 9 7950X 16-Core Processor\n\
                 physical id\t: 0\n\
                 core id\t: {core}\n\
                 cpu MHz\t\t: 4500.123\n\n"
            ));
        }
        buf
    }

    #[test]
    fn parses_dual_socket_xeon() {
        let sr = TempdirSysroot::new();
        sr.write("proc/cpuinfo", xeon_fixture());
        let info = probe(&sr, 16).unwrap();
        assert_eq!(info.vendor, "GenuineIntel");
        assert_eq!(info.cores_online, 16);
        assert_eq!(info.cores_physical, 8); // 4 cores per package × 2 sockets
        assert_eq!(info.sockets, 2);
        assert_eq!(info.mhz, 2700);
        assert!(info.model.contains("8480"));
    }

    #[test]
    fn parses_single_socket_ryzen() {
        let sr = TempdirSysroot::new();
        sr.write("proc/cpuinfo", ryzen_fixture());
        let info = probe(&sr, 32).unwrap();
        assert_eq!(info.vendor, "AuthenticAMD");
        assert_eq!(info.cores_online, 32);
        assert_eq!(info.cores_physical, 16);
        assert_eq!(info.sockets, 1);
        assert_eq!(info.mhz, 4500);
    }

    #[test]
    fn missing_phys_core_ids_falls_back_to_online() {
        // QEMU and some VMs omit physical id / core id.
        let raw = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Generic\ncpu MHz\t: 2000.0\n\n";
        let sr = TempdirSysroot::new();
        sr.write("proc/cpuinfo", raw);
        let info = probe(&sr, 4).unwrap();
        assert_eq!(info.cores_physical, 4);
        assert_eq!(info.sockets, 1);
    }
}
