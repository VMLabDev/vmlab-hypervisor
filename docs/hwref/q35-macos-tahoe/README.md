# macOS Tahoe QEMU reference

Captured on 2026-10-31 from `x86_64/macos-tahoe@26.7.0` through
`vmlab up`. The running guest reports macOS 26.7.2, build 25G332.
QEMU 11.1.2 ran q35 under KVM with the template's CPU, RAM, firmware,
SMC, SATA disk, VMware display, USB input, and virtio-serial agent.
An isolated lab segment supplied a transitional virtio-net NIC.
The template store was not modified.

## Findings

- F1: **No FIFO commands were observed.** All enabled `vmware_*` event
  counts are zero. The generic device trace has no `vmsvga-io` access,
  including CONFIG_DONE or SYNC. A live read of the complete 64-KiB
  FIFO BAR found every byte zero, including MIN, MAX, NEXT_CMD, and STOP.
  Boot, agent probes, pointer and keyboard input, screen updates, and
  shutdown produced no evidence of FIFO submission or 2D blits.
- F2: Mode-setting used the device's VGA-compatible VBE interface,
  **not VMware-indexed registers**. Ports 0x1ce and 0x1d0 selected
  640x480 and then 1280x800, both 32-bit. The enable register received
  0 and 0x41. All 22 VBE data accesses were decoded and reconciled with
  the full-stream counters. `IONDRVFramebuffer` bound; the guest reported
  PCI 15ad:0405 with no graphics kext loaded. D3 does not need a blit
  engine for this observed path. Its implementation must also support
  the VBE compatibility path that firmware actually used.
- F3: QEMU USB Keyboard and QEMU USB Tablet enumerated through
  AppleUSBXHCIPCI and AppleUserHIDEventDriver. Five injected characters
  appeared in the password field, proving guest keyboard consumption.
  Pointer movement and clicks were injected successfully, but the guest's
  pointer coordinates were not independently read back.
- F4: AppleAHCI drove the SATA disk. File write/read/delete and sync
  passed. Agent commands traversed virtio-serial; the virtio-net NIC
  received two ping replies from the isolated gateway with no packet loss.
  PCI IDs are transitional: 1af4:1003 and 1af4:1000.
- F5: AppleSMC bound. The trace recorded READ_KEY, WRITE_KEY, and
  GET_KEY_TYPE command writes. Observed READ_KEY names were BEMB, MSSD,
  MSSW, NATJ, OSK0, OSK1, and OSWD. No four-byte key payload followed
  the WRITE_KEY or GET_KEY_TYPE writes. Their key names and success are
  not established. No SMC response values are retained.

## MSR list deferred to vmlab-hv

The XNU unhandled-MSR list was not captured. The owner explicitly deferred
it to vmlab-hv's D2 implementation: KVM userspace MSR exits return zero
for unknown reads, drop unknown writes, and log each MSR once on macOS
machines. That list needs no privileged host logging.

This host had `ignore_msrs=Y`, `report_ignored_msrs=N`, and unreadable
unprivileged `dmesg`. No root command ran and no KVM setting changed.
This capture resolves the observed D3 FIFO question, not D2's MSR list.

## Evidence

| Files | Contents |
|---|---|
| `qemu-argv.json`, `environment.json`, `template.wcl` | Redacted launch, binary and firmware hashes, template metadata |
| `query-*.json`, `qtree.txt`, `mtree.txt`, `pci.txt`, `usb.txt` | QMP and HMP machine state, BARs, topology, devices |
| `overlay-chain.json`, `overlay-check.json` | Scratch overlay ancestry and successful offline image check |
| `acpi/`, `acpi.json` | In-memory RSDP and eight checksum-valid ACPI tables |
| `smbios-*.bin`, `smbios.json` | Two checksum-valid in-memory SMBIOS candidates, including type 2 |
| `fwcfg-files.json`, `fwcfg-smbios-*.bin` | fw_cfg inventory and QEMU's original SMBIOS blobs |
| `cmos-observed.json` | Sampled RTC-indexed bytes, not an atomic CMOS dump |
| `registers.json`, `trace-counts.json`, `event-values.json` | Full-stream counters, first-use order, bounded value sets and samples |
| `display-registers.md`, `vbe-indexed.json`, `svga-*` | Display registers, zero event counts, full zero-filled FIFO snapshot |
| `usb-registers.md`, `virtio-registers.md`, `ahci-registers.md`, `applesmc.md` | Device register and command summaries |
| `guest-static.json`, `hid-exercise.json`, `exercise.json` | Guest identity, driver binding, HID inventory, file and network probes |
| `probe-commands.json` | Exact guest probes and qtest observation method |
| `desktop.png`, `hid-input.png`, `input*.json` | Screen and input evidence, including the corrected click invocation |
| `down.json`, `stop.json`, `scope-limits.txt`, `preboot-vms.json` | Shutdown, scope limits, preboot VM count |
| `validation.json`, `sha256.json` | Evidence checks and artifact hashes |

The two SMBIOS candidates retain both firmware and OpenCore-era content.
Memory scanning alone does not identify the active EFI configuration-table
pointer. ACPI was read from populated guest memory, not the unrelocated
fw_cfg table blob. A full CPUID leaf inventory and ACPI disassembly were
not captured. No alternate runtime display mode was requested.

## Method, bounds, and safety

The scratch lab was `/tmp/maccap.8E3Qjc`. It used the current locally
compiled vmlab binary, isolated runtime and state directories, a writable
overlay, and an isolated segment without NAT. Only observation flags were
added to QEMU: event tracing, an extra QMP socket, and a qtest socket with
its protocol log directed to `/dev/null`. qtest supplied read-only memory
observation plus fw_cfg selector and RTC-index sampling.
Its generic I/O events carry `cpu -1` and remain in the counters.

The shared VM permit and a process count allowed one capture VM alongside
one other project VM. The Android emulator was excluded. The capture
process tree ran under `systemd-run --user --scope -q -p MemoryMax=24G
-p MemorySwapMax=0`. The recorded limits are 25769803776 and 0.
The guest shut down through `vmlab down`; the isolated supervisor was
then stopped. No capture VM or daemon remains running. No host PCI or
USB device was claimed, reset, or rebound.

The sanitized raw prefix is untracked under the worktree's `.scratch/`.
It is capped at 64 MiB. Summarization continued after that cap and through
shutdown. The 4,033,207 input events reconcile with 4,032,503 retained
event counts and 704 SMC accesses. Raw-prefix counts and omitted-line
counts reconcile in `validation.json`. Register value sets and event
argument samples are limited to 256 entries, with truncation indicators.
First-use indices include firmware, boot, exercises, and shutdown.
KVM RAM writes bypass device callbacks, so this is not a complete CPU
bus trace. FIFO inactivity is supported by both its zero snapshot and
the absence of VMware register submissions, not callback tracing alone.
Trace overhead invalidates timing and throughput comparisons.

QEMU was used as a black box. The interface references were its installed
trace-event definitions, its qtest protocol documentation
(https://www.qemu.org/docs/master/devel/testing/qtest.html), and VMware's
public register definitions as distributed in Linux vmwgfx
(https://kernel.googlesource.com/pub/scm/linux/kernel/git/sashal/linux/+/d64b3932531cbc0c9d6bdb744446934435e9d6af/drivers/gpu/drm/vmwgfx/svga_reg.h).
One web search incidentally returned a QEMU qtest-source documentation
excerpt. That result was not opened or used. The protocol came from the
official documentation above. No QEMU implementation code was copied or
translated.

## Secret-handling incident

SMC data values were discarded before the raw prefix or summaries reached
disk. Launch argv was redacted before persistence. However, QEMU's
`info qtree` returned the OSK property, and the initial local `qtree.txt`
contained it until the secret scan redacted it. This violated the brief's
requirement that it never enter any artifact. The value was not displayed
in tool output or committed. The current artifact is redacted.

The final scan loaded the configured key through vmlab's real
`HostConfig::load_default()` parser. It checked artifact and scratch files
for the full value, its first eight bytes, and both 32-byte halves without
printing any value. That scan passed. Protocol key names OSK0 and OSK1
are retained intentionally. The scan excludes guest disk and firmware
images; no whole guest-memory dump was persisted.

No shared Rust interface or device implementation changed. The evidence
checks validate this QEMU reference; they do not replace vmlab-hv S1
acceptance tests.

## Repository verification

The initial `just ci::check` passed formatting and clippy but failed four
tests with `waiting for a test VM slot` timeouts. The failing tests were
Ubuntu AHCI, native display/input, and both QEMU display references.
`ci-initial.json` retains the diagnostics. That run did not complete the
remaining workspace test binaries.

The retry used `RUST_TEST_THREADS=1` to reduce contention while retaining
the shared two-VM permit limit. All 16 binary acceptance tests, including
the four initial failures, passed. The GPU test and the subsequent test
binaries through the process-guard unit tests passed. The run then failed
both audio integration tests before either could boot: one timed out
waiting for shared host VM capacity, the other waiting for a permit.
`ci-serial.json` retains those diagnostics. Cargo did not run the remaining
workspace test binaries after that failure.

**The repository completion gate remains unpassed.** Both CI attempts
exited 101. They made no Rust source changes. Another full `just ci::check`
run with available shared VM capacity is still required. The capture's
independent evidence checks passed; they do not turn either CI failure
into a pass.

Both runs used four Cargo build jobs and a 24-GiB scope. Real USB and VFIO
opt-ins were unset, so no real-device test is claimed. All capture and CI
jobs have exited and their Herdr panes have been closed. Other sessions'
VMs and permits were left untouched.

`vmlab validate` passed for the scratch lab. Standalone `wcl check`
could not resolve `<vmlab.wcl>` because that schema is embedded in vmlab,
not registered with the standalone WCL CLI. `wcl-validation.json` records
both results. No schema failure is being treated as a successful check.
