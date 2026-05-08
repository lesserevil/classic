//! PCI device enumeration via `/sys/bus/pci/devices/`. Each subdirectory
//! is a device named after its `DDDD:BB:DD.F` PCI address; the per-device
//! files we care about are `vendor`, `device`, `class`, `numa_node`, and
//! the `iommu_group` symlink (whose basename is the group id).

use std::path::Path;

use crate::discovery::{read_string, Sysroot};
use crate::schema::PciDevice;

const PCI_DEVICES_DIR: &str = "sys/bus/pci/devices";

pub fn probe<S: Sysroot + ?Sized>(sr: &S) -> std::io::Result<Vec<PciDevice>> {
    let mut entries = sr.read_dir(Path::new(PCI_DEVICES_DIR))?;
    entries.sort(); // FR-3: lexical addr ordering
    let mut out = Vec::with_capacity(entries.len());
    for addr in entries {
        match read_one(sr, &addr) {
            Ok(dev) => out.push(dev),
            Err(_e) => {
                // A device may disappear mid-walk (hot-unplug). Skip it
                // rather than failing the whole probe.
                continue;
            }
        }
    }
    Ok(out)
}

fn read_one<S: Sysroot + ?Sized>(sr: &S, addr: &str) -> std::io::Result<PciDevice> {
    let base = Path::new(PCI_DEVICES_DIR).join(addr);
    let vendor = parse_hex_u16(&read_string(sr, &base.join("vendor"))?);
    let device = parse_hex_u16(&read_string(sr, &base.join("device"))?);
    let class = parse_hex_u32(&read_string(sr, &base.join("class"))?);
    let numa_node = read_string(sr, &base.join("numa_node"))?
        .parse::<i32>()
        .unwrap_or(-1);
    let iommu_group = sr
        .read_link(&base.join("iommu_group"))
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str().map(str::to_string)))
        .and_then(|s| s.parse::<u32>().ok());
    Ok(PciDevice {
        addr: addr.to_string(),
        vendor,
        device,
        class,
        numa_node,
        iommu_group,
    })
}

fn parse_hex_u16(s: &str) -> u16 {
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap_or(0)
}

fn parse_hex_u32(s: &str) -> u32 {
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;

    fn write_pci_dev(sr: &TempdirSysroot, addr: &str, vendor: u16, device: u16, class: u32, numa: i32, iommu: Option<u32>) {
        let base = format!("sys/bus/pci/devices/{addr}");
        sr.write(format!("{base}/vendor"), format!("0x{:04x}\n", vendor));
        sr.write(format!("{base}/device"), format!("0x{:04x}\n", device));
        sr.write(format!("{base}/class"), format!("0x{:06x}\n", class));
        sr.write(format!("{base}/numa_node"), format!("{numa}\n"));
        if let Some(g) = iommu {
            // iommu_group is a symlink whose basename is the group id. The
            // target need not exist for our parser; we only read the link.
            sr.symlink(
                format!("{base}/iommu_group"),
                Path::new("..").join("..").join("..").join("kernel/iommu_groups").join(g.to_string()),
            );
        }
    }

    #[test]
    fn enumerates_devices_in_lexical_addr_order() {
        let sr = TempdirSysroot::new();
        write_pci_dev(&sr, "0000:01:00.0", 0x10DE, 0x2330, 0x030200, 0, Some(42));
        write_pci_dev(&sr, "0000:00:00.0", 0x8086, 0x3E1F, 0x060000, -1, None);
        write_pci_dev(&sr, "0000:1b:00.0", 0x10DE, 0x2330, 0x030200, 1, Some(17));
        let devs = probe(&sr).unwrap();
        assert_eq!(devs.len(), 3);
        assert_eq!(devs[0].addr, "0000:00:00.0");
        assert_eq!(devs[1].addr, "0000:01:00.0");
        assert_eq!(devs[2].addr, "0000:1b:00.0");
    }

    #[test]
    fn parses_full_fields_and_iommu_symlink() {
        let sr = TempdirSysroot::new();
        write_pci_dev(&sr, "0000:1b:00.0", 0x10DE, 0x2330, 0x030200, 1, Some(17));
        let devs = probe(&sr).unwrap();
        let d = &devs[0];
        assert_eq!(d.addr, "0000:1b:00.0");
        assert_eq!(d.vendor, 0x10DE);
        assert_eq!(d.device, 0x2330);
        assert_eq!(d.class, 0x030200);
        assert_eq!(d.numa_node, 1);
        assert_eq!(d.iommu_group, Some(17));
    }

    #[test]
    fn missing_iommu_group_yields_none() {
        let sr = TempdirSysroot::new();
        write_pci_dev(&sr, "0000:00:00.0", 0x8086, 0x3E1F, 0x060000, -1, None);
        let devs = probe(&sr).unwrap();
        assert_eq!(devs[0].iommu_group, None);
        assert_eq!(devs[0].numa_node, -1);
    }
}
