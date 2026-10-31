# Phase-2 performance gate (HV-D28)

First full measurement of the HV-D28 bar: *before the VM default flips,
vmlab-hv must be no slower than QEMU on boot to agent ready, and reach at
least 80 % of QEMU's virtio-blk and virtio-net throughput, measured on this
host* (`notes/29-performance.md`).

**Verdict: the gate is NOT passed.** Three of ten bars hold; seven fail.
The numbers below are the medians of 3 repeats per configuration from
`bench/history/x86_64.jsonl` (commit `92ba17149`, 2026-10-31), run by
`just bench-run-kvm` / `just bench-run-g2g` with the suite's usual noise
control (taskset pin 16-23, systemd-run memory cap, fresh overlay per
repeat). What each workload measures is in `bench/README.md`.

## Bars

| Guest | Workload | QEMU | vmlab-hv | hv/qemu | Bar | Verdict |
|---|---|---|---|---|---|---|
| ubuntu-26.04 | boot_to_agent_ready | 9.04 s ±0.9 | 16.12 s ±0.1 | 1.78x | ≤ 1.00x | **FAILS** |
| windows-10 | boot_to_agent_ready | 209.44 s ±109 | 307.12 s ±160 | 1.47x | ≤ 1.00x | **FAILS** |
| ubuntu-26.04 | blk_seq_read | 15410.6 MiB/s | 218.3 MiB/s | 0.01x | ≥ 0.80x | **FAILS** |
| ubuntu-26.04 | blk_seq_write | 177.8 MiB/s | 214.3 MiB/s | 1.21x | ≥ 0.80x | meets |
| ubuntu-26.04 | blk_rand_read_4k | 44541 iops | 51186 iops | 1.15x | ≥ 0.80x | meets |
| ubuntu-26.04 | blk_rand_write_4k | 11534 iops | 3957 iops | 0.34x | ≥ 0.80x | **FAILS** |
| ubuntu-26.04 | net_tx_udp | 30276 Mbit/s | 19895 Mbit/s | 0.66x | ≥ 0.80x | **FAILS** |
| ubuntu-26.04 | net_rx_udp | 58261 Mbit/s | 10480 Mbit/s | 0.18x | ≥ 0.80x | **FAILS** |
| ubuntu-26.04 | net_g2g_a2b_tcp | 25908 Mbit/s | 11628 Mbit/s | 0.45x | ≥ 0.80x | **FAILS** |
| ubuntu-26.04 | net_g2g_b2a_tcp | 25963 Mbit/s | 11752 Mbit/s | 0.45x | ≥ 0.80x | **FAILS** |
| ubuntu-26.04 | idle_cpu_pct (no bar) | 2.27 % | 5.20 % | 2.3x | — | reported |

Notes on noise: the two write workloads spread widely on both backends
(qemu blk_seq_write samples 159–2015 MiB/s; blk_rand_write 4.8k–20k iops
under hv) — the host was shared with other agents' VM work during the run
(load average 4–7 recorded per repeat in the history env). The failing
verdicts are all far outside that noise; the two "meets" verdicts on write
workloads should be re-read when the host is quiet. Boot and net_rx repeats
were tight (±0.1 s, ±1 Gbit/s).

## Where the time goes (failed bars)

Method: the host's `perf` userland tool is not installed and
`ptrace_scope=1` blocks `gdb` attach to non-child processes, but
`perf_event_paranoid=1` allows user-space event sampling of own-uid
processes. The profiles are leaf-instruction-pointer histograms at ~1 kHz
per thread, symbolised across the binary and its DSOs
(`/tmp/ipsample.c`, output under `bench/run/profiles/`), taken during one
vmlab-hv run of each failing workload. Two caveats: kernel frames are
excluded (the bar's host cost is therefore attributed to the userspace
hypervisor), and release inlining means a symbol names the outermost
monomorphised function.

### net_rx_udp (0.18x) — the clearest signal

72 % of all samples in the hottest two symbols, both in the virtio-net
device (`crates/vmlab-hv-devices/src/virtio/net.rs`):

| share | symbol |
|---|---|
| 47 % | `vmlab_hv_devices::virtio::net::fire_config` |
| 25 % | `vmlab_hv_devices::virtio::net::process_tx` |

The RX path spends its cycles firing virtio config-change notifications
and in queue processing — the device re-raises config interrupts far more
often than QEMU's netdev path does, and each jumbo frame walks that path.
`fire_config` is the first thing to fix for every net bar: g2g (0.45x) and
net_tx (0.66x) show the same device code hot (net_tx additionally spends
~25 % of samples in libc copy/write frames).

### blk_seq_read (0.01x)

218 MiB/s vs 15.4 GiB/s. The histogram is spread thin (top leaf 5.7 %)
across `vmlab_hv_vmm::loader`-region code, PCI and device frames — no
single hotspot; the cost is per-operation overhead through the whole
guest-exit → virtqueue → vmlab-disk stack. Two structural differences
against QEMU stand independent of sampling: vmlab-disk reads the backing
chain synchronously cluster-by-cluster (plain buffered `OpenOptions`,
no readahead batching), while QEMU's block layer serves the same reads
from large page-cached reads of the template. The tight repeats (±3 MiB/s)
say this is steady-state behaviour, not noise.

### blk_rand_write (0.34x)

Same shape as seq_read: thin spread, no single hotspot; the cost is
per-op qcow2 allocation work in vmlab-disk on the write path (reads of
this same image at 4k granularity hold 1.15x, so the gap is specific to
allocation/refcount updates, not the read path).

### boot_to_agent_ready, ubuntu (1.78x) and windows (1.47x)

Boot profiles are spread thin (top leaf 2.6 %, half the samples in libc
and unresolved DSO frames): no single boot hotspot; the cost is the
per-exit device-processing overhead spread over firmware, kernel and
systemd bring-up. The windows number carries one structural hv cost the
ubuntu one does not: vmlab-hv exits cleanly on every guest reset, so the
sysprep first boot (3 resets) respawns the process and re-runs OVMF each
time (samples 210–371 s vs QEMU's 121–230 s; the respawn path is part of
what a vmlab user waits).

## What would move the bars

1. `virtio::net::fire_config` / `process_tx` (net_rx, g2g, net_tx):
   coalesce config-change notifications and batch queue processing.
2. Bulk reads and write batching in the vmlab-disk qcow2 read/write path
   (blk_seq_read, blk_rand_write).
3. The boot gap shares the per-exit overhead with the net path; fixing
   (1) is likely to move it, and the windows respawn path is worth a look
   (three process respawns per first boot).

Re-run with `just bench-run-kvm vmlab-hv` after each change;
`just bench-report` prints every bar against D28.
