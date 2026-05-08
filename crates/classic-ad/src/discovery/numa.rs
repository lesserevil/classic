//! NUMA topology probe via `/sys/devices/system/node/node*`.

use std::collections::BTreeSet;
use std::path::Path;

use crate::discovery::{read_string, Sysroot};
use crate::schema::NumaNode;

const NODE_DIR: &str = "sys/devices/system/node";

pub fn probe<S: Sysroot + ?Sized>(sr: &S) -> std::io::Result<Vec<NumaNode>> {
    let entries = match sr.read_dir(Path::new(NODE_DIR)) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()), // non-NUMA systems lack this dir
    };
    let mut nodes: Vec<NumaNode> = Vec::new();
    let mut node_ids: Vec<u32> = entries
        .iter()
        .filter_map(|name| name.strip_prefix("node").and_then(|n| n.parse::<u32>().ok()))
        .collect();
    node_ids.sort_unstable();
    for id in node_ids {
        let cpulist_raw = read_string(sr, &Path::new(NODE_DIR).join(format!("node{id}/cpulist")))
            .unwrap_or_default();
        let mem_total_mb = read_string(sr, &Path::new(NODE_DIR).join(format!("node{id}/meminfo")))
            .ok()
            .and_then(|raw| parse_node_mem_total_kb(&raw))
            .map(|kb| kb / 1024)
            .unwrap_or(0);
        nodes.push(NumaNode {
            id,
            cpus: parse_cpulist(&cpulist_raw),
            mem_total_mb,
        });
    }

    // FR-4: ensure each online CPU appears in at most one node. If the
    // sysfs export accidentally double-lists a CPU (rare, but defensive),
    // de-duplicate by keeping its first appearance and dropping it from
    // later nodes.
    let mut seen = BTreeSet::<u32>::new();
    for node in nodes.iter_mut() {
        node.cpus.retain(|c| seen.insert(*c));
    }
    Ok(nodes)
}

/// Parse a Linux cpulist string like `"0-7,16-23"` or `"0,2,4-7"` into a
/// sorted unique `Vec<u32>`. Out-of-order or overlapping ranges are
/// flattened.
pub(crate) fn parse_cpulist(raw: &str) -> Vec<u32> {
    let mut out: BTreeSet<u32> = BTreeSet::new();
    for chunk in raw.split(',') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = chunk.split_once('-') {
            let lo: u32 = match lo.trim().parse() { Ok(v) => v, Err(_) => continue };
            let hi: u32 = match hi.trim().parse() { Ok(v) => v, Err(_) => continue };
            if hi >= lo {
                for c in lo..=hi {
                    out.insert(c);
                }
            }
        } else if let Ok(v) = chunk.parse::<u32>() {
            out.insert(v);
        }
    }
    out.into_iter().collect()
}

fn parse_node_mem_total_kb(raw: &str) -> Option<u64> {
    for line in raw.lines() {
        // Lines look like "Node 0 MemTotal:       262144000 kB".
        if let Some(rest) = line.split("MemTotal:").nth(1) {
            return rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;

    #[test]
    fn cpulist_parsing_flattens_ranges() {
        assert_eq!(parse_cpulist("0-3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpulist("0-3,7,9-10"), vec![0, 1, 2, 3, 7, 9, 10]);
        assert_eq!(parse_cpulist(""), Vec::<u32>::new());
        assert_eq!(parse_cpulist("0,2,4"), vec![0, 2, 4]);
    }

    #[test]
    fn enumerates_two_node_topology() {
        let sr = TempdirSysroot::new();
        sr.write("sys/devices/system/node/node0/cpulist", "0-7,16-23\n");
        sr.write("sys/devices/system/node/node0/meminfo", "Node 0 MemTotal:       131072000 kB\n");
        sr.write("sys/devices/system/node/node1/cpulist", "8-15,24-31\n");
        sr.write("sys/devices/system/node/node1/meminfo", "Node 1 MemTotal:       131072000 kB\n");
        let nodes = probe(&sr).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id, 0);
        assert_eq!(nodes[0].cpus.len(), 16);
        assert_eq!(nodes[0].cpus.first(), Some(&0));
        assert_eq!(nodes[1].id, 1);
        assert_eq!(nodes[1].cpus.len(), 16);
        assert_eq!(nodes[0].mem_total_mb, 131072000 / 1024);
    }

    #[test]
    fn each_cpu_appears_in_exactly_one_node() {
        let sr = TempdirSysroot::new();
        // Deliberate overlap: CPU 7 listed in both nodes (FR-4 says we
        // de-duplicate; first occurrence wins).
        sr.write("sys/devices/system/node/node0/cpulist", "0-7\n");
        sr.write("sys/devices/system/node/node0/meminfo", "Node 0 MemTotal:       1000 kB\n");
        sr.write("sys/devices/system/node/node1/cpulist", "7-15\n");
        sr.write("sys/devices/system/node/node1/meminfo", "Node 1 MemTotal:       1000 kB\n");
        let nodes = probe(&sr).unwrap();
        let mut seen = std::collections::HashSet::new();
        for n in &nodes {
            for c in &n.cpus {
                assert!(seen.insert(*c), "cpu {} appeared twice", c);
            }
        }
    }

    #[test]
    fn missing_node_dir_yields_empty_topology() {
        let sr = TempdirSysroot::new();
        let nodes = probe(&sr).unwrap();
        assert!(nodes.is_empty());
    }
}
