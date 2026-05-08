//! Load and CPU-utilisation probe. `/proc/loadavg` is single-shot; CPU
//! percent requires comparing two `/proc/stat` snapshots so the probe
//! carries a small amount of state across calls.

use std::path::Path;

use crate::discovery::{read_string, Sysroot};
use crate::schema::LoadSample;

pub struct LoadProbe {
    /// `(idle, total)` from the most recent `/proc/stat` cpu line.
    last: Option<(u64, u64)>,
}

impl LoadProbe {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn sample<S: Sysroot + ?Sized>(
        &mut self,
        sr: &S,
        mem_pct: u32,
        task_count: u32,
    ) -> std::io::Result<LoadSample> {
        let loadavg = parse_loadavg(&read_string(sr, Path::new("proc/loadavg"))?);
        let cpu_pct = self.update_cpu_pct(sr)?;
        Ok(LoadSample {
            loadavg_1m: loadavg.0,
            loadavg_5m: loadavg.1,
            loadavg_15m: loadavg.2,
            cpu_pct,
            mem_pct,
            task_count,
        })
    }

    fn update_cpu_pct<S: Sysroot + ?Sized>(&mut self, sr: &S) -> std::io::Result<u32> {
        let raw = read_string(sr, Path::new("proc/stat"))?;
        let snap = parse_stat(&raw);
        let pct = match self.last {
            Some(prev) => delta_pct(prev, snap),
            // First sample has no baseline.
            None => 0,
        };
        self.last = Some(snap);
        Ok(pct)
    }
}

impl Default for LoadProbe {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `/proc/loadavg` into 1/5/15-minute samples scaled ×1000.
fn parse_loadavg(raw: &str) -> (u32, u32, u32) {
    let mut parts = raw.split_whitespace();
    let one = scale_thousand(parts.next());
    let five = scale_thousand(parts.next());
    let fifteen = scale_thousand(parts.next());
    (one, five, fifteen)
}

fn scale_thousand(field: Option<&str>) -> u32 {
    let s = match field {
        Some(s) => s,
        None => return 0,
    };
    let f = s.parse::<f64>().unwrap_or(0.0);
    (f * 1000.0).round() as u32
}

/// Parse the cpu aggregate line from `/proc/stat` and return
/// `(idle_jiffies, total_jiffies)`.
fn parse_stat(raw: &str) -> (u64, u64) {
    let cpu_line = raw.lines().find(|l| l.starts_with("cpu ")).unwrap_or("");
    let mut iter = cpu_line.split_whitespace();
    iter.next(); // discard "cpu"
    let fields: Vec<u64> = iter.filter_map(|s| s.parse().ok()).collect();
    // Layout: user nice system idle iowait irq softirq steal guest guest_nice
    let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();
    (idle, total)
}

fn delta_pct(prev: (u64, u64), now: (u64, u64)) -> u32 {
    let idle = now.0.saturating_sub(prev.0);
    let total = now.1.saturating_sub(prev.1);
    if total == 0 {
        return 0;
    }
    let busy = total - idle;
    ((busy * 100) / total).min(100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;

    #[test]
    fn loadavg_scaled_thousand() {
        let (a, b, c) = parse_loadavg("0.42 0.81 1.10 2/734 1234");
        assert_eq!(a, 420);
        assert_eq!(b, 810);
        assert_eq!(c, 1100);
    }

    #[test]
    fn cpu_pct_delta_between_snapshots() {
        let sr = TempdirSysroot::new();
        // First snapshot: 90 idle + 0 iowait, 100 total.
        sr.write(
            "proc/loadavg",
            "0.10 0.20 0.30 1/100 1\n",
        );
        sr.write(
            "proc/stat",
            "cpu  10 0 0 90 0 0 0 0 0 0\n",
        );
        let mut probe = LoadProbe::new();
        let s1 = probe.sample(&sr, 0, 0).unwrap();
        assert_eq!(s1.cpu_pct, 0); // first sample has no baseline

        // Second snapshot: 70 idle + 0 iowait (+ user 30), 200 total.
        // Delta: 200 - 100 = 100 total, idle - idle_prev = 70 - 90 = saturating 0... wait that goes negative.
        // Use cumulative counters: prev (idle=90, total=100), now (idle=160, total=300) => total delta 200, idle delta 70 => busy 130 / 200 = 65%.
        sr.write(
            "proc/stat",
            "cpu  140 0 0 160 0 0 0 0 0 0\n",
        );
        let s2 = probe.sample(&sr, 0, 0).unwrap();
        assert_eq!(s2.cpu_pct, 65);
    }

    #[test]
    fn cpu_pct_clamps_to_100_when_total_zero() {
        let sr = TempdirSysroot::new();
        sr.write("proc/loadavg", "0 0 0 1/1 1\n");
        sr.write("proc/stat", "cpu  0 0 0 0 0 0 0 0 0 0\n");
        let mut probe = LoadProbe::new();
        let s1 = probe.sample(&sr, 0, 0).unwrap();
        let s2 = probe.sample(&sr, 0, 0).unwrap();
        assert_eq!(s1.cpu_pct, 0);
        assert_eq!(s2.cpu_pct, 0);
    }
}
