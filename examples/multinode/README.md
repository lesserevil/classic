# Multi-node demo

End-to-end demo of a three-node Classic cluster. The point of this tree
is to prove the system works: clone the repo, build release binaries,
put three config files in place, start `classicd` on each host, and
watch five scenarios print `OK`.

The plan-08 design doc (`plans/08-multinode-demo.md`) is the source of
truth for what each scenario does and what counts as success.

## Topology

```
Subnet 10.42.0.0/24 (Ethernet)

  node-a 10.42.0.10  dev workstation, no GPU,   classicd + classic CLI
  node-b 10.42.0.11  1x NVIDIA >= 16 GiB VRAM,  classicd, NVML present
  node-c 10.42.0.12  CPU-only worker,           classicd
```

All three peers are wired statically into each other's `[[peer]]`
blocks. The daemon listens on port `7421`. Every config under
`config/` is a verbatim copy of plan-08's documented schema; if
`classicd`'s parser hasn't caught up yet, a `--check-config` flag is
the canonical validator (or `cargo test -p classic-node` exercises
the same parse path).

## Prerequisites

On every node:

- Linux 6.1+ on x86_64 (Ubuntu 24.04 tested).
- A working build of the `classicd` and `classic` binaries from
  `cargo build --release` at the repo root, installed into
  `/usr/local/bin/`.
- `/var/lib/classicd` writable by the user running `classicd`.

On node-b (the GPU host) only:

- One NVIDIA GPU with at least 16 GiB VRAM.
- `libnvidia-ml.so.1` on the default linker path. (The CI smoke run
  uses an `LD_PRELOAD` stub; real hardware uses the vendor library.)
- CUDA toolkit if you intend to rebuild `programs/cuda_hello/`. For
  CI runs the demo program is built host-only with `CLASSIC_NVML_STUB=1`.

## Walkthrough

```bash
# 1. Build the binaries.
cd <repo root>
cargo build --release

# 2. Drop binaries into /usr/local/bin/ on every node.
sudo install -m 0755 target/release/{classicd,classic} /usr/local/bin/

# 3. Copy the appropriate config to /etc/classic/config.toml on each host.
sudo install -d /etc/classic
sudo install -m 0644 examples/multinode/config/node-X.toml /etc/classic/config.toml

# 4. Start the daemons. (Use systemd / your init of choice in production.)
sudo classicd --config /etc/classic/config.toml &

# 5. From node-a, run the first scenario.
make -C examples/multinode setup        # verifies prereqs
make -C examples/multinode scenario-1   # HW-affinity placement; expects OK
```

The `setup` target validates that every peer is reachable, every
`classicd` is up, and NVML is visible on node-b. On any failure it
exits non-zero with a remediation hint.

`make all-real-hardware` runs all five scenarios in sequence on real
hardware. `cargo xtask demo-smoke` runs scenarios 1, 2, 3 against a
Docker Compose stack with NVML stubbed; this is what CI executes.

## Scenarios

1. `scenarios/01-hw-affinity.sh` — `classic spawn --requires "..."`
   places `cuda_hello` on node-b (the only host whose ad matches the
   GPU predicate) and prints `hello from gpu 0 on node-b`.
2. `scenarios/02-no-match.sh` — predicate matches nothing in the
   cluster; CLI exits non-zero with `no node matches`; no child runs
   anywhere.
3. `scenarios/03-service-lookup.sh` — coord on node-b, worker on
   node-c. The worker mails the coord via service-name resolution and
   the coord prints the hello.
4. `scenarios/04-namespace-bind.sh` — node-a runs `pci_lspci_loop`
   against a bind-mount of node-b's `/dev/pci/`; the BDF set matches
   what `lspci` on node-b reports.
5. `scenarios/05-pack-group.sh` — `classic submit group.toml` with a
   PACK strategy of 2 members; both members land on node-b with
   measurable concurrent overlap.

## Troubleshooting

### Peer unreachable

`make setup` reports `peer 10.42.0.11:7421 unreachable`.

Confirm the daemon is running (`pgrep classicd` on that host),
confirm the listen address matches the peer entries on the other two
nodes, and confirm port 7421 is not firewalled (try `nc -zv <peer> 7421`
from a third node). On AWS / GCP VMs the most common cause is a
missing security group rule for TCP 7421 within the subnet.

### NVML missing

`make setup` reports `node-b has no NVML library on linker path`.

Either install the proprietary NVIDIA driver (which ships
`libnvidia-ml.so.1`), set `LD_LIBRARY_PATH` so it's discoverable, or —
for testing — `LD_PRELOAD=/usr/local/lib/nvml-stub.so` to use the CI
stub. The plan's stub returns one fake 24 GiB device so the predicate
in scenario 1 still matches.

### GPU passthrough not configured

`scenario-1` reports `cuda_hello` failed with `CUDA_ERROR_NO_DEVICE`.

If node-b is a VM, GPU passthrough must be enabled (PCI passthrough on
QEMU/KVM, or vGPU on hypervisors that support it). Confirm with
`nvidia-smi` on node-b — if that fails, no Classic config change will
help.

### libnvidia-ml ABI mismatch

`classicd` on node-b logs `nvmlInit_v2: unknown symbol` or aborts at
startup with `version GLIBC_NVML_...` errors.

The driver version installed on the host doesn't match the headers
the daemon was built against. Either rebuild `classicd` on the same
host or pin to a driver version that matches your toolkit.

### Port 7421 firewalled

`classicd` on the affected node logs `accept: connection reset` for
peers it's never able to dial. `nc -zv` from another peer fails.

Either open TCP 7421 in the host firewall (`ufw allow 7421/tcp` on
Ubuntu, equivalent rule in `iptables` / cloud security group), or
override `listen` to a port that is reachable, and update the matching
`[[peer]].addr` lines on the other two nodes in the same change.

### Hostname not resolvable from peers

`make setup` reports `peer reachable but classicd handshake failed`,
and `classicd` logs include `peer hint resolution failed`.

The `hint` field in `[[peer]]` is currently informational; the dialed
address is `addr`. If you've replaced the static IPs with hostnames
(common on lab networks), add the hosts to `/etc/hosts` on every node
or set up DNS — the daemon does not resolve hostnames itself in v1.

## Layout

```
examples/multinode/
├── README.md                        this file
├── Makefile                         setup / scenario-1..5 / all-real-hardware / clean
├── config/{node-a,node-b,node-c}.toml
├── lib/assert.sh                    shared bash helpers
├── programs/
│   ├── cuda_hello/                  CUDA hello-world for scenario 1
│   └── pci_lspci_loop/              libc lspci-over-9p for scenario 4
├── scenarios/01..05-*.sh            one file per scenario
└── ci/                              Compose harness for `cargo xtask demo-smoke`
```

The `xtask` driver lives at the repo root (`xtask/`) and is invoked
via `cargo xtask demo-smoke`. It builds release binaries, builds the
NVML stub, brings up the Compose stack, runs `ci/smoke.sh`, and tears
the stack down.
