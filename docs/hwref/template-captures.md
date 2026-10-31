# Phase 4 template captures

QEMU 11.1.2 observations collected on 2026-10-31 for phase-4 D1 and the
macOS capture prerequisite. These references describe the owner's actual
templates. They do not replace S1 acceptance tests on vmlab-hv.

| Template directory | Boot and agent | Display | Storage | NIC | Shutdown |
|---|---|---|---|---|---|
| `pc-freedos-1.3` | DOS agent prompt | VBE request, text-mode restore; return code not checked | Disk and CD readback | PCnet driver, DHCP, two ping replies | Guest poweroff |
| `pc-windows-98-se` | Desktop and agent | Two 16-bit modes, GDI blits, 2918 BitBLT starts | Disk and CD readback | No active PCnet interface | Host quit after two failed stop attempts |
| `pc-windows-7` | Desktop and agent | Interactive modes and blits passed | Disk and CD readback | e1000 DHCP and two ping replies | Guest poweroff |
| `pc-templeos` | Desktop and HolyC agent | 640x480 observed; no alternate mode | File write/read/delete; no CD attached | No NIC attached | Guest poweroff |
| `q35-macos-tahoe` | Agent-ready, macOS 26.7.2 | VBE modes and framebuffer; no SVGA FIFO activity | SATA file readback | Transitional virtio-net, two ping replies | Guest poweroff |

The macOS follow-up used the now-configured `host.apple_smc_osk` and the
current locally compiled vmlab binary. Its FIFO verdict is no commands:
all VMware event counts were zero and the entire FIFO BAR was zero.
Mode-setting used the VGA-compatible VBE ports instead of VMware-indexed
registers. The owner deferred the XNU MSR list to vmlab-hv's D2 userspace
MSR exits and once-per-MSR logging; no privileged host changes were made.
The macOS README records a secret-handling incident: the initial local
QMP qtree artifact contained the OSK property before redaction. No value
was displayed in tool output or committed; the final secret scan passed.

The macOS lab was `/tmp/maccap.8E3Qjc`, isolated from the earlier four
captures. It added a qtest observation socket alongside tracing and QMP.
Its directory contains register and command summaries, static hardware
state, checksum-valid ACPI and SMBIOS data, input evidence, and an overlay
check. See its README for interpretation limits and deferred observations.

## Provenance and safety

Commands came from `vmlab up` in
`/home/wil/dev/vmlab/.scratch/hwref-captures.MTzhVI`. The capture added
only trace flags and a second QMP socket. NICs used an isolated vmlab segment.
Fixture CDs were lab attachments, not changes to the template store.
The per-template `qemu-argv.json` records the exact command, including
template-derived CPU, memory, agent transport, and device ordering.

The capture used one VM at a time. A shared test-VM permit kept another
session's VM slot available; process counts were also checked before boots.
`scope-limits.txt` records `MemoryMax=25769803776` and `MemorySwapMax=0`.
All capture VMs and the isolated capture daemons were stopped afterwards.
No host PCI or USB device was claimed, rebound, reset, or passed through.
QEMU was used only as a black box; no QEMU C source was read.

## Artifact contract

Each completed directory contains exact argv, executable and metadata hashes,
the overlay chain and image check, HMP machine state, PCI JSON, guest-installed
ACPI and SMBIOS binaries, fw_cfg inventory, RTC value samples, and exercise
logs. `sha256.json` covers those committed artifacts. `exercise.c` records
the original probe used on Windows or DOS; no third-party implementation
code was copied into these probes.

Device Markdown tables summarize port and MMIO registers, read/write counts,
value sets, and first-use order. `registers.json` is the machine-readable form.
`event-values.json` retains bounded argument samples and command opcodes.
The newer captures also include indexed-register counters and command counts.
The Windows 98 BitBLT control-field sets are complete even though its geometry
samples are truncated.

Raw logs stay untracked under the capture worktree's `.scratch/` directory.
Committed HMP text and guest logs use LF endings with trailing whitespace
removed. Their original text remains in `.scratch/`; JSON data and binary
hardware tables retain their captured content.
Each raw prefix is capped at 64 MiB. The stream is summarized beyond that cap;
`trace-bound.json` records omitted raw lines. Value sets and argument samples
are limited to 256 distinct entries and carry truncation flags. First-use
indices include firmware, boot, guest exercises, and shutdown.

## Interpretation limits

Physical-memory observation can add reads to legacy VGA aperture rows.
Those rows combine framebuffer locations and are not guest-only counters.
Host fw_cfg inventory reads ran with memory-region tracing disabled.
Entries with `cpu -1` are retained; these are QEMU event traces, not
architectural CPU traces. KVM's in-kernel PIC/PIT paths can bypass QEMU's
device events. The capture therefore does not claim a complete CPU bus trace.

`cmos-observed.json` records sampled RTC-indexed values. It is not an atomic
dump of every CMOS byte. ACPI tables were read from the installed guest tables
through QMP, rather than through guest tools. Their checksums were validated.
The guest tools did not supply a complete ACPI disassembly or CPUID inventory.

Windows 98 still needs a working PCnet driver path and a demonstrated clean
shutdown. TempleOS pointer consumption was not proven by the immediate guest
coordinate check. FreeDOS live CD replacement lost agent responsiveness in
an earlier run, which ended in host quit; its successful boot-attached CD
run does not resolve that failure. Trace overhead invalidates boot-time and
throughput comparisons. No shared Rust interface or device implementation
changed in this capture task.

## Verification

`just ci::check` passed in the capture worktree: formatting, clippy with
`-D warnings`, and workspace tests. The run used a 24-GiB scope, four Cargo
build jobs, and the shared two-VM permit limit. Existing target artifacts
were reused through `CARGO_TARGET_DIR`; no Rust source or shared interface
changed. Real USB/VFIO opt-ins were unset, so no real-device check is claimed.

`template-capture-validation.json` records independent checks of the four
completed artifact sets. Full register totals reconcile with event totals,
raw-prefix counts reconcile with omitted-line counts, and every raw prefix
is within 64 MiB. ACPI and SMBIOS checksums passed, fw_cfg names and selectors
are unique, and all four overlays passed `qemu-img check`. Per-directory
SHA-256 manifests cover the committed evidence. WCL schema checks passed for
the scratch lab configuration.

These checks validate the recorded evidence. They do not resolve the guest
failures or missing observations described above.

The macOS follow-up has its own `validation.json` and SHA-256 manifest.
Its independent artifact checks passed. Its repository gate did not:
the first `just ci::check` run hit four VM-permit timeouts; a serialized
retry passed those tests but hit host-capacity and permit timeouts in the
audio integration tests. Both CI runs exited 101, and later workspace
test binaries were not run. The macOS README and `ci-*.json` retain the
details. The earlier four-capture CI success does not cover this follow-up.
