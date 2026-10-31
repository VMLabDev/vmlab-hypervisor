# macOS hypervisor verification

This report covers the hypervisor side of the macOS phase on 2026-11-01.
The guest is an unchanged linked clone of `x86_64/macos-tahoe@26.7.0`.
The template reports macOS 26.7.2, build 25G332.

## Current verification state

P2 passes. The device, firmware, and protocol suites pass. The complete P1
run and `just ci::check` have not passed yet. Successful diagnostic boots
with host `kvm.ignore_msrs=N` reached agent-ready, executed guest commands,
and delivered 1280x800 native frames. Those boots do not establish the
remaining keyboard, network, snapshot, reset, and shutdown checks.

One subsequent attempt stopped at OpenCore's prohibited-boot screen. The
same implementation booted on retry. Another attempt reached agent-ready
but exceeded the initial 90-second wait for the graphical login screen.
The acceptance test now allows five minutes and retains a failure screenshot.
Neither failed attempt is counted as a pass.

## Reference comparisons

The P2 tests compare the shipping constructors with
`docs/hwref/q35-macos-tahoe/`:

- F1: All eight PCI functions have the captured vendor, device, subsystem,
  class, interrupt pin, and BAR sizes and types. Firmware-assigned addresses
  and interrupt lines are not static constructor properties.
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
uses that value in both EAX and EBX, as the phase specification requires.
QEMU's captured EBX is 1,000,000 kHz. The implementation deliberately
follows the specification for this field.

The implementation keeps D2's catch-all policy for KVM's unknown-MSR
reason. An enumerated list would depend on the XNU version and executed
power-management paths. Invalid and filtered MSRs retain fault semantics.
Only machines requesting `msr_policy=ignore` enable userspace unknown-MSR
exits. Reads return zero, writes are dropped, and indices are logged once
per machine process. Snapshot load unions the saved log set with indices
already seen, so rewind and reset do not duplicate messages.

Diagnostic boots with host MSR ignoring disabled observed these indices:

```text
000000e7 000000e8 000003f8 000003f9 000003fa 000003fc 000003fd 000003fe
0000060d 0000061d 00000621 00000630 00000631 00000632 0000064f 000006b0
```

This list is evidence, not an allow-list.

## Snapshot and shutdown measurements

The pending S1 snapshot regression changes the destination TSC frequency
and checks rejection before RAM changes. It also introduces a large
synthetic guest-TSC jump and checks restoration of CPUID, frequency, and
the logged-MSR set. A synthetic jump does not establish behavior across a
literal host reboot. No host reboot has been performed.

The existing guest-agent shutdown vocabulary supplies `reboot` and
`powerdown`. The pending P1 run checks the S1 reset event, a new guest boot
session UUID, and clean process exit after `powerdown`. No new control
command has been added. Shutdown behavior remains unverified until that
run completes.

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

All tests use fresh overlays. Every boot uses the shared VM permits and a
24-GiB systemd scope with no swap. Host KVM settings and physical USB and
PCI devices are untouched. No QEMU C implementation source was read.
