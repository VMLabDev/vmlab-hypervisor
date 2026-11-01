# macOS hypervisor verification

This report covers the hypervisor side of the macOS phase on 2026-11-01.
The guest is an unchanged linked clone of `x86_64/macos-tahoe@26.7.0`.
The template reports macOS 26.7.2, build 25G332.

Owner decision D4 makes vmlab-hv the only macOS runtime. QEMU captures remain
hardware references. The owner keeps this host at `kvm.ignore_msrs=N`.

## Current verification state

P2 and P4 pass. One complete P1 run passed, but its repeat in final
`just ci::check` failed after all guest checks, when agent powerdown left a
unsuccessful process status. The exit code or signal is unknown because the
test omitted the status and stderr from its
assertion. That diagnostic is corrected; the shutdown failure is not fixed.
The CI binary finished with 38 passing tests and this one failure. The full
workspace gate stopped there and has not passed.

The owner requested a benchmark window after CI started. The existing run
finished, and no new VM may boot until `BENCH WINDOW CLOSED`. The CI process
and its future children were restricted to CPUs 0-7. Further builds use
`CARGO_BUILD_JOBS=4` under `taskset -c 0-7`.
The complete P1 run passed with host `kvm.ignore_msrs=N` before boot and
after shutdown. It verified agent execution, visible 1280x800 native frames,
keyboard glyphs, network ping, online snapshot file rollback, reboot with
a new boot-session UUID, unchanged TSC frequency, and clean agent powerdown.
The run took 137.44 seconds, including the shared VM permit wait.

One subsequent attempt stopped at OpenCore's prohibited-boot screen. The
same implementation booted on retry. Another attempt reached agent-ready
but exceeded the initial 90-second wait for the graphical login screen.
The acceptance test now allows five minutes and retains a failure screenshot.
Neither failed attempt is counted as a pass.
The longer retry also failed. macOS requested an unsolicited chipset reset
before keyboard input, which returned the display to a boot mode. The guest
reset is under investigation. WindowServer is blocked in `IOKitWaitQuiet`.
The registry shows the unmatched SMC device and boot CPU as busy.

A fresh fake-key QEMU capture exposed a missing command-acceptance bit in
the SMC model. READ_KEY must report 0x0c after the command, then 0x04 after
each key byte. The differential test failed with 0x04 at the first step and
passes after the correction. A second protocol defect kept GET_KEY_TYPE
busy after its four key bytes. macOS reads this fixed-size response without
sending a length byte. The corrected implementation supplies the implicit
length, and a regression test covers both fixed-size commands and snapshots.
Fresh live boots reach the graphical login screen.

The xHCI interrupt path also ignored the endpoint service interval. A new
controller test reproduced two keyboard reports completing within one
64-ms service interval. It passes after interval enforcement, including a
snapshot between reports. Another regression covers standalone Event Data
TDs, which must complete without consuming a USB report. All 47 USB tests pass.
The native input test also waits 100 ms after the pointer click before
sending keys, so the guest can focus the password field. It observes five
masked glyphs, then replaces them with three. Earlier runs sent keys
immediately after queuing the click and produced no glyphs. They are not
counted as passes.

The parallel device suite also hit an unchanged console event-order test
failure. That test passes alone. The final serial device suite passes 390 tests,
with one existing ignored test. No console implementation changed.

After the failed CI run, formatting, workspace clippy with `-D warnings`,
the glyph-detector fixture, and the serial device suite pass under the CPU
restriction. A real child process exiting with status 17 verifies that the
corrected shutdown diagnostic retains both its exit status and stderr.
The macOS shutdown reproduction awaits the benchmark-window release.

The first full `just ci::check` run failed two binary acceptance tests.
The macOS run stopped before its agent handshake. The unchanged QEMU Ubuntu
standard-VGA reference also timed out waiting for its agent. The other 37
binary tests passed. This CI run predates the SMC and interval corrections.
The QEMU standard-VGA reference passes on an isolated rerun in 21.81 seconds.

## Reference comparisons

The P2 tests compare the shipping constructors with
`docs/hwref/q35-macos-tahoe/`:

- F1: All eight PCI functions have the captured vendor, device, subsystem,
  class, interrupt pin, BAR sizes, widths, and prefetchability.
  Firmware-assigned addresses and interrupt lines are not static constructor properties.
- F2: FACP, APIC, HPET, MCFG, WAET, and DSDT match after pointer relocation
  and checksum normalization. OVMF generates BGRT at runtime.
- F3: The complete fw_cfg SMBIOS table and anchor match, including type 2.
- F4: The fw_cfg file inventory and e820 match with two omissions.
  `genroms/kvmvapic.bin` belongs to QEMU's paravirtual APIC implementation.
  The empty `etc/tpm/log` entry has no consumer without a TPM.
- F5: The NIC's optional PXE ROM is absent. OpenCore boots from AHCI.

The SMC capture in `smc-e820.json` uses an explicitly fake OSK and never
queries OSK0 or OSK1. It establishes MSSD=3, NATJ=0, and missing-key errors
for BEMB, MSSW, and OSWD. Key enumeration and type queries are supported as
Linux protocol extensions beyond that QEMU capture.

## CPU and MSR decisions

The named `Skylake-Client-v4` model uses all 48 captured leaf/subleaf
records in `cpuid.json`. APIC IDs, topology, and cache sharing follow the
requested vCPU count. KVM supplies OSXSAVE from the guest's CR4.

The reported virtual TSC is 4,291,934 kHz on this host. Leaf `0x40000010`
uses `KVM_GET_TSC_KHZ` in EAX and KVM's 1,000,000-kHz local APIC rate in
EBX, matching the capture. The owner corrected the original specification,
which incorrectly required both fields to contain TSC kHz. Live verification
confirms that this correction alone does not prevent the guest reset. The
KVM test also pins a different TSC rate and verifies that EBX remains fixed.

The implementation keeps D2's catch-all policy for KVM's unknown-MSR
reason. An enumerated list would depend on the XNU version and executed
power-management paths. Invalid and filtered MSRs retain fault semantics.
Only machines requesting `msr_policy=ignore` enable userspace unknown-MSR
exits. Reads return zero, writes are dropped, and indices are logged once
per machine process. Snapshot load unions the saved log set with indices
already seen, so rewind and reset do not duplicate messages.

The successful P1 run with host MSR ignoring disabled observed these indices:

```text
000000e7 000000e8 000003f8 000003f9 000003fa 000003fc 000003fd 000003fe
0000060d 0000061d 00000621 00000630 00000631 00000632 0000064f 000006b0
```

This list is evidence, not an allow-list.

## Snapshot and shutdown measurements

The S1 snapshot regression passed with the named CPU model. It changes the
destination TSC frequency and verifies rejection before RAM changes. It
also introduces a large synthetic guest-TSC jump and verifies restoration
of CPUID, frequency, and the logged-MSR set. A synthetic jump does not
establish behavior across a literal host reboot. No host reboot has been
performed.

The existing guest-agent shutdown vocabulary supplies `reboot` and
`powerdown`. P1 verifies the S1 reset event, a new guest boot-session UUID,
and clean process exit after `powerdown` in the standalone run. The CI repeat
passed reset but exited unsuccessfully after `powerdown`; its cause is unknown.
No new control command has been added.

## Secret handling and safety

The live test loads `host.apple_smc_osk` with the WCL parser at runtime.
`VMLAB_HV_TEST_APPLE_SMC_OSK` is the environment alternative. Missing keys,
templates, or KVM access produce explicit skip reasons.

The launch description is an anonymous memfd. Debug output and description
dumps redact the OSK. Snapshot device state stores the SMC transaction
cursor, not OSK bytes or read-response buffers. Restore uses the key from
the current description. SMC port values are excluded from I/O diagnostics.

The task brief forbids the OSK in every snapshot file. The specification
requires complete RAM snapshots and explicitly forbids it in device state.
These requirements differ because guest RAM can contain OSK responses.
The implementation follows the specification. The live acceptance test
automatically removes its private snapshot directory, including on failure.
The temporary overlays and RAM snapshots from both lifecycle runs were
removed and cannot be recovered. The original template is unchanged.

All tests use fresh overlays. Every boot uses the shared VM permits and a
24-GiB systemd scope with no swap. Host KVM settings and physical USB and
PCI devices are untouched. No QEMU C implementation source was read.

## Shared interfaces

- I1: `MachineDescription` adds the CPU timing, MSR policy, SMC, USB keyboard,
  and SMBIOS flags. `DisplayDevice` adds `VmwareSvga`.
- I2: `UsbDevice::inject_key` accepts qcodes and press state. Its default
  returns an unsupported-input error, so existing backends need no change.
- I3: ACPI `Platform` adds `applesmc` and `amd_hypertransport`.
  `SmbiosConfig` adds `baseboard`.
- I4: xHCI device snapshots use `VHVXHC03` to retain interrupt service waits.
  Restore also accepts `VHVXHC02`, with no outstanding wait.
