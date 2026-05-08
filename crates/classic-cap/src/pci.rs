//! PCI BDF -> `(major, minor)` resolution. Reads
//! `/sys/bus/pci/devices/<bdf>/uevent`, looking for the `MAJOR=`/`MINOR=`
//! lines the kernel emits for char devices.

use std::path::Path;

use crate::broker::BdfAddr;
use crate::cgroup::Sysroot;

/// Parse `<sysroot>/sys/bus/pci/devices/<bdf>/uevent` and return
/// `(major, minor)` if the device exposes a char node, else `None`.
/// Never panics — missing file or unparseable content yields `None`.
pub fn resolve_bdf<S: Sysroot + ?Sized>(sr: &S, bdf: BdfAddr) -> Option<(u32, u32)> {
    let rel = uevent_rel(bdf);
    let raw = sr.read_to_string(&rel).ok()?;
    parse_uevent(&raw)
}

fn uevent_rel(bdf: BdfAddr) -> std::path::PathBuf {
    Path::new("sys/bus/pci/devices").join(format!("{}", bdf)).join("uevent")
}

fn parse_uevent(raw: &str) -> Option<(u32, u32)> {
    let mut major: Option<u32> = None;
    let mut minor: Option<u32> = None;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("MAJOR=") {
            major = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("MINOR=") {
            minor = v.trim().parse().ok();
        }
    }
    Some((major?, minor?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct TempSr {
        dir: TempDir,
    }
    impl Sysroot for TempSr {
        fn root(&self) -> &Path {
            self.dir.path()
        }
    }

    fn fixture(uevent_body: &str, bdf: BdfAddr) -> (TempSr, PathBuf) {
        let dir = TempDir::new().unwrap();
        let sr = TempSr { dir };
        let rel = uevent_rel(bdf);
        let abs = sr.root().join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, uevent_body).unwrap();
        (sr, abs)
    }

    #[test]
    fn parses_major_minor() {
        let bdf = BdfAddr { domain: 0, bus: 0x1b, device: 0, function: 0 };
        let body = "DRIVER=nvidia\nPCI_CLASS=030200\nMAJOR=195\nMINOR=0\n";
        let (sr, _) = fixture(body, bdf);
        assert_eq!(resolve_bdf(&sr, bdf), Some((195, 0)));
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let sr = TempSr { dir };
        let bdf = BdfAddr { domain: 0, bus: 0, device: 0, function: 0 };
        assert_eq!(resolve_bdf(&sr, bdf), None);
    }

    #[test]
    fn missing_minor_returns_none() {
        let bdf = BdfAddr { domain: 0, bus: 0xc0, device: 0, function: 0 };
        let body = "DRIVER=mlx5_core\nMAJOR=10\n";
        let (sr, _) = fixture(body, bdf);
        assert_eq!(resolve_bdf(&sr, bdf), None);
    }

    #[test]
    fn handles_garbage_values() {
        let bdf = BdfAddr { domain: 0, bus: 0, device: 0, function: 0 };
        let body = "MAJOR=not-a-number\nMINOR=0\n";
        let (sr, _) = fixture(body, bdf);
        assert_eq!(resolve_bdf(&sr, bdf), None);
    }
}
