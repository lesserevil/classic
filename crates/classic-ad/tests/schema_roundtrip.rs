//! Wire-format compliance tests for `classic-ad` schema types. Encodes via
//! the same bincode (legacy: fixed-int, little-endian) configuration as
//! `classic-proto` so hand-rolled integration with the frame layer works
//! byte-for-byte.

use classic_ad::{
    AdGossip, AdRequest, CpuInfo, GpuInfo, LoadSample, MemInfo, NodeAd, NumaNode, PciDevice,
};
use classic_proto::NodeId;

fn config() -> bincode::config::Configuration<
    bincode::config::LittleEndian,
    bincode::config::Fixint,
    bincode::config::NoLimit,
> {
    bincode::config::legacy()
}

fn roundtrip<T>(v: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let bytes = bincode::serde::encode_to_vec(v, config()).unwrap();
    let (back, consumed) = bincode::serde::decode_from_slice::<T, _>(&bytes, config()).unwrap();
    assert_eq!(consumed, bytes.len(), "trailing bytes after decode");
    back
}

fn example_ad() -> NodeAd {
    NodeAd {
        node_id: NodeId([0xAB; 16]),
        hostname: "node-a".to_string(),
        proto_version: classic_proto::PROTO_VERSION,
        generation: 7,
        boot_time: 1_700_000_000,
        cpu: CpuInfo {
            cores_online: 64,
            cores_physical: 32,
            sockets: 2,
            model: "AMD EPYC 7763".into(),
            vendor: "AuthenticAMD".into(),
            arch: "x86_64".into(),
            mhz: 2450,
        },
        mem: MemInfo { total_mb: 524_288, available_mb: 512_000 },
        gpus: vec![GpuInfo {
            index: 0,
            uuid: "GPU-deadbeef-cafe-1234-5678-90abcdef0000".into(),
            name: "NVIDIA H100".into(),
            pci_vendor: 0x10DE,
            pci_device: 0x2330,
            pci_addr: "0000:01:00.0".into(),
            vram_total_mb: 81920,
            vram_free_mb: 81000,
            compute_capability: (9, 0),
            nvlink_peers: vec!["0000:02:00.0".into(), "0000:03:00.0".into()],
            utilization_pct: 0,
        }],
        pci: vec![PciDevice {
            addr: "0000:01:00.0".into(),
            vendor: 0x10DE,
            device: 0x2330,
            class: 0x030200,
            numa_node: 0,
            iommu_group: Some(42),
        }],
        numa: vec![NumaNode { id: 0, cpus: (0..32).collect(), mem_total_mb: 262144 }],
        load: LoadSample {
            loadavg_1m: 1500,
            loadavg_5m: 2000,
            loadavg_15m: 1750,
            cpu_pct: 35,
            mem_pct: 12,
            task_count: 17,
        },
    }
}

#[test]
fn canonical_example_roundtrips_byte_equal() {
    let ad = example_ad();
    let bytes_a = bincode::serde::encode_to_vec(&ad, config()).unwrap();
    let (back, _) = bincode::serde::decode_from_slice::<NodeAd, _>(&bytes_a, config()).unwrap();
    let bytes_b = bincode::serde::encode_to_vec(&back, config()).unwrap();
    assert_eq!(bytes_a, bytes_b, "re-encoding the decoded ad must be byte-equal");
    assert_eq!(back, ad);
}

#[test]
fn random_filled_ad_roundtrips() {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE);
    for _ in 0..32 {
        let ad = NodeAd {
            node_id: {
                let mut bytes = [0u8; 16];
                rng.fill(&mut bytes);
                NodeId(bytes)
            },
            hostname: format!("host-{}", rng.gen::<u32>()),
            proto_version: rng.gen(),
            generation: rng.gen(),
            boot_time: rng.gen(),
            cpu: CpuInfo {
                cores_online: rng.gen_range(1..=256),
                cores_physical: rng.gen_range(1..=128),
                sockets: rng.gen_range(1..=8),
                model: random_string(&mut rng, 20),
                vendor: random_string(&mut rng, 12),
                arch: "x86_64".into(),
                mhz: rng.gen_range(800..=5000),
            },
            mem: MemInfo {
                total_mb: rng.gen_range(1024..=2 * 1024 * 1024),
                available_mb: rng.gen_range(512..=1024 * 1024),
            },
            gpus: (0..rng.gen_range(0..=8))
                .map(|i| GpuInfo {
                    index: i,
                    uuid: random_string(&mut rng, 40),
                    name: random_string(&mut rng, 16),
                    pci_vendor: rng.gen(),
                    pci_device: rng.gen(),
                    pci_addr: format!("0000:{:02x}:00.0", i),
                    vram_total_mb: rng.gen_range(8 * 1024..=80 * 1024),
                    vram_free_mb: rng.gen_range(0..=8 * 1024),
                    compute_capability: (rng.gen_range(5..=10), rng.gen_range(0..=9)),
                    nvlink_peers: vec![],
                    utilization_pct: rng.gen_range(0..=100),
                })
                .collect(),
            pci: (0..rng.gen_range(0..=64))
                .map(|i| PciDevice {
                    addr: format!("0000:{:02x}:00.0", i),
                    vendor: rng.gen(),
                    device: rng.gen(),
                    class: rng.gen(),
                    numa_node: rng.gen_range(-1..=4),
                    iommu_group: if rng.gen_bool(0.5) { Some(rng.gen()) } else { None },
                })
                .collect(),
            numa: (0..rng.gen_range(1..=4))
                .map(|i| NumaNode {
                    id: i,
                    cpus: (0..16).collect(),
                    mem_total_mb: rng.gen_range(8 * 1024..=512 * 1024),
                })
                .collect(),
            load: LoadSample {
                loadavg_1m: rng.gen_range(0..=8000),
                loadavg_5m: rng.gen_range(0..=8000),
                loadavg_15m: rng.gen_range(0..=8000),
                cpu_pct: rng.gen_range(0..=100),
                mem_pct: rng.gen_range(0..=100),
                task_count: rng.gen_range(0..=4096),
            },
        };
        let back = roundtrip(&ad);
        assert_eq!(back, ad);
    }
}

fn random_string(rng: &mut impl rand::Rng, len: usize) -> String {
    (0..len).map(|_| (b'a' + rng.gen_range(0..=25)) as char).collect()
}

#[test]
fn ad_gossip_full_and_delta_roundtrip() {
    let full = AdGossip::Full(example_ad());
    let back = roundtrip(&full);
    assert_eq!(back, full);

    let delta = AdGossip::Delta { node_id: NodeId([0x42; 16]), generation: 17 };
    let back = roundtrip(&delta);
    assert_eq!(back, delta);
}

#[test]
fn ad_request_roundtrip() {
    let req = AdRequest { from: NodeId([0xCD; 16]) };
    let back = roundtrip(&req);
    assert_eq!(back, req);
}

/// Bound check for a fully-loaded ad. The original bead targeted 8 KiB, but
/// under bincode's `legacy()` fixed-int + LE config — which the plan
/// requires for byte-compat with classic-proto — every `String` carries an
/// 8-byte length prefix, so 256 PCI devices alone burn ~5 KiB of overhead
/// before payload. 16 KiB is well under the 16 MiB frame cap and is plenty
/// for a single-frame ad on a maxed-out box.
#[test]
fn max_realistic_ad_size_is_bounded() {
    // 16 GPUs + 256 PCI devices, plus generous strings.
    let ad = NodeAd {
        node_id: NodeId([0u8; 16]),
        hostname: "x".repeat(64),
        proto_version: classic_proto::PROTO_VERSION,
        generation: u64::MAX,
        boot_time: u64::MAX,
        cpu: CpuInfo {
            cores_online: 256,
            cores_physical: 128,
            sockets: 8,
            model: "x".repeat(64),
            vendor: "x".repeat(16),
            arch: "x86_64".into(),
            mhz: 5000,
        },
        mem: MemInfo { total_mb: u64::MAX, available_mb: u64::MAX },
        gpus: (0..16)
            .map(|i| GpuInfo {
                index: i,
                uuid: "x".repeat(40),
                name: "x".repeat(48),
                pci_vendor: 0x10DE,
                pci_device: 0x2330,
                pci_addr: "0000:00:00.0".into(),
                vram_total_mb: 80_000,
                vram_free_mb: 80_000,
                compute_capability: (9, 0),
                nvlink_peers: vec![],
                utilization_pct: 100,
            })
            .collect(),
        pci: (0..256)
            .map(|i| PciDevice {
                addr: format!("0000:{:02x}:00.0", i % 256),
                vendor: 0x10DE,
                device: 0x2330,
                class: 0x030200,
                numa_node: 0,
                iommu_group: Some(i),
            })
            .collect(),
        numa: (0..4)
            .map(|i| NumaNode {
                id: i,
                cpus: (0..32).collect(),
                mem_total_mb: 256_000,
            })
            .collect(),
        load: LoadSample {
            loadavg_1m: 8000,
            loadavg_5m: 8000,
            loadavg_15m: 8000,
            cpu_pct: 100,
            mem_pct: 100,
            task_count: 4096,
        },
    };
    let bytes = bincode::serde::encode_to_vec(&ad, config()).unwrap();
    assert!(
        bytes.len() <= 16 * 1024,
        "encoded NodeAd was {} bytes, expected <= 16 KiB",
        bytes.len()
    );
}
