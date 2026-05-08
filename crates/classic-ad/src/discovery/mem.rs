//! Memory probe. Reads `/proc/meminfo`. Falls back to
//! `MemFree + Buffers + Cached` when `MemAvailable` is missing
//! (pre-3.14 kernels).

use std::path::Path;

use crate::discovery::{read_string, Sysroot};
use crate::schema::MemInfo;

pub fn probe<S: Sysroot + ?Sized>(sr: &S) -> std::io::Result<MemInfo> {
    let raw = read_string(sr, Path::new("proc/meminfo"))?;
    Ok(parse(&raw))
}

fn parse(raw: &str) -> MemInfo {
    let mut total_kb: u64 = 0;
    let mut available_kb: Option<u64> = None;
    let mut free_kb: u64 = 0;
    let mut buffers_kb: u64 = 0;
    let mut cached_kb: u64 = 0;
    for line in raw.lines() {
        let line = line.trim();
        let (key, rest) = match line.split_once(':') {
            Some(kv) => (kv.0.trim(), kv.1.trim()),
            None => continue,
        };
        // Values look like "16385020 kB".
        let kb: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        match key {
            "MemTotal" => total_kb = kb,
            "MemAvailable" => available_kb = Some(kb),
            "MemFree" => free_kb = kb,
            "Buffers" => buffers_kb = kb,
            "Cached" => cached_kb = kb,
            _ => {}
        }
    }
    let available_kb = available_kb.unwrap_or(free_kb + buffers_kb + cached_kb);
    MemInfo {
        total_mb: total_kb / 1024,
        available_mb: available_kb / 1024,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;

    #[test]
    fn parses_modern_meminfo_with_memavailable() {
        let raw = "\
            MemTotal:       16385020 kB\n\
            MemFree:         8000000 kB\n\
            MemAvailable:   12000000 kB\n\
            Buffers:          200000 kB\n\
            Cached:          3000000 kB\n";
        let sr = TempdirSysroot::new();
        sr.write("proc/meminfo", raw);
        let info = probe(&sr).unwrap();
        assert_eq!(info.total_mb, 16385020 / 1024);
        assert_eq!(info.available_mb, 12000000 / 1024);
    }

    #[test]
    fn falls_back_when_memavailable_absent() {
        let raw = "\
            MemTotal:       16385020 kB\n\
            MemFree:         8000000 kB\n\
            Buffers:          200000 kB\n\
            Cached:          3000000 kB\n";
        let sr = TempdirSysroot::new();
        sr.write("proc/meminfo", raw);
        let info = probe(&sr).unwrap();
        assert_eq!(info.available_mb, (8000000 + 200000 + 3000000) / 1024);
    }
}
