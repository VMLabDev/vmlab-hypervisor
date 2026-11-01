# macOS hypervisor verification

This report covers the hypervisor side of the macOS phase on 2026-11-01.
The guest is an unchanged linked clone of `x86_64/macos-tahoe@26.7.0`.
The template reports macOS 26.7.2, build 25G332.

Owner decision D4 makes vmlab-hv the only macOS runtime. QEMU captures remain
hardware references. The owner keeps this host at `kvm.ignore_msrs=N`.

## Current verification state

The live macOS boot test (`macos_tahoe_p1_agent_display_keyboard_network_reset_snapshot_stop`) is opt-in while macOS is parked under https://github.com/VMLabDev/vmlab/issues/200: run it with `VMLAB_HV_MACOS_ACCEPTANCE=1`. A later gate run failed it with the agent handshake timing out while both vCPUs were still in firmware or OpenCore, one of the intermittent boot stalls that issue tracks. The macOS device and unit tests stay in the default gate.

P1, P2, P4, and `just ci::check` pass. Eight complete lifecycle runs passed
with host `kvm.ignore_msrs=N`. The final workspace gate used
`RUST_TEST_THREADS=1`, `CARGO_BUILD_JOBS=4`, and `VMLAB_HV_TEST_VM_LIMIT=4`
inside the 24-GiB scope. It passed all 40 binary acceptance tests and 391
device tests, with one existing ignored device test. The firmware, protocol,
TCG, audio, S1, filesystem-restore, sharing, VMM, and documentation suites
also passed. The log is `.scratch/macos-ci-interval.log`; its exit status is 0.

On 2026-11-01, the owner explicitly deferred literal host-reboot restore
verification to an owner-coordinated reboot. The owner is away, and a reboot
would end every active session. This deferred measurement does not block
merge. The synthetic TSC-jump evidence below does not establish cross-reboot
behavior.

An earlier CI repeat failed after all guest checks when
agent powerdown left an unsuccessful process status. The exit code or
signal is unknown because the test omitted the status and stderr from its
assertion. That diagnostic is corrected; the shutdown failure is not fixed.
That CI binary finished with 38 passing tests and this one failure. A later
run finished with 39 passing tests and an initial keyboard failure. Its
failure matches an isolated run of the same CI-built binary: the password
field remained empty. After the focus correction, all 40 binary acceptance
tests passed in full CI. Formatting and clippy also passed. That workspace
run subsequently failed in the unchanged HDA audio integration test with an
ALSA capture EIO. Isolated repeats reproduce intermittent playback and
capture EIO, and one repeat produced insufficient PCM after snapshot load.
Other isolated repeats pass. The audio cause remains unknown. The final
workspace gate passes, but that does not resolve the earlier failures.

The audio test now retains PCM status, HDA interrupt counts, kernel messages,
and background-player stderr on failure. A temporary direct-MMIO diagnostic
used a vectorized guest read that triggered an invalid opcode; that probe was
removed. These changes improve diagnostics only. No HDA behavior, PCM buffer
size, or acceptance assertion changed. Five scalar-probe repeats passed, but
those passes do not establish a fix for the earlier failures.

The owner closed the intervening benchmark window and raised the host limit
to four project VMs. Commit `fb507f3` is merged. Direct builds retain
`CARGO_BUILD_JOBS=4` under `taskset -c 0-7`.

The successful runs verified agent execution, visible 1280x800 native frames,
keyboard glyphs, network ping, online snapshot file rollback, reboot with
a new boot-session UUID, unchanged TSC frequency, and clean agent powerdown.
Standalone runs took 137.44, 86.99, 100.10, 97.02, 145.49, and 132.75 seconds,
including shared VM permit waits. Two more passes were in full CI. Subsequent
runs did not reproduce the shutdown failure. Runs from the 100.10-second pass
onward check keyboard input before networking, as the spec orders, and again
after reset. Runs from the 97.02-second pass onward confirm focus with a
visible glyph before each key sequence.

One subsequent attempt stopped at OpenCore's prohibited-boot screen. The
same implementation booted on retry. Another attempt reached agent-ready
but exceeded the initial 90-second wait for the graphical login screen.
The acceptance test now allows five minutes and retains a failure screenshot.
Neither failed attempt is counted as a pass.
The longer retry also failed. macOS requested an unsolicited chipset reset
before keyboard input, which returned the display to a boot mode. In that
run, WindowServer blocked in `IOKitWaitQuiet`, and the registry showed the
unmatched SMC device and boot CPU as busy.

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
configured service interval. It passes after interval enforcement, including a
snapshot between reports. Another regression covers standalone Event Data
TDs, which must complete without consuming a USB report. The keyboard's
high-speed descriptor originally requested 64 ms. The captured guest inventory
requires 8 ms, so its bInterval is now 7. A descriptor-parser regression
reproduced the discrepancy and passes after the correction. All 48 USB tests
and the complete live macOS lifecycle pass with the corrected interval.
The native input test waits for the password field and holds the pointer
button for 50 ms. It then requires one visible probe glyph to confirm focus,
retrying the focus request for at most 20 seconds. After clearing the probe,
it requires exactly five masked glyphs, then replaces them with exactly three.
The earlier one-shot click and fixed 100-ms wait failed in both full CI and
an isolated CI-binary run. Those failures are not counted as passes.

The parallel device suite also hit an unchanged console event-order test
failure. That test passes alone. The serial device suite before the interval
regression passed 390 tests, with one existing ignored test. The final serial
suite passes 391 tests, with the same ignored test. No console implementation
changed.

After the failed CI run, formatting, workspace clippy with `-D warnings`,
the glyph-detector fixture, and the serial device suite pass under the CPU
restriction. A real child process exiting with status 17 verifies that the
corrected shutdown diagnostic retains both its exit status and stderr.
All three post-window macOS shutdown reproductions passed. The earlier
failure remains unexplained.

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
which incorrectly required both fields to contain TSC kHz. An intermediate
live run still reset during boot after the timing correction. The
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

The S1 regression
`macos_snapshot_preserves_cpuid_msr_log_and_refuses_changed_tsc_frequency`
passed with the named CPU model. It changes the destination TSC frequency
and verifies rejection before RAM changes. With the original frequency
restored, it sets `IA32_TSC` to `1 << 60` and loads the snapshot. The test
verifies rollback of that synthetic jump, restoration of CPUID and the
pinned frequency, and preservation of the union of logged MSR indices.

No host reboot has been performed. The owner explicitly deferred that
measurement on 2026-11-01 to an owner-coordinated reboot because the owner
is away and restarting the host would end every active session. The synthetic
regression and passing same-host P1 round-trips remain the available evidence;
they do not establish behavior across a host reboot.

The existing guest-agent shutdown vocabulary supplies `reboot` and
`powerdown`. P1 verifies the S1 reset event, a new guest boot-session UUID,
and clean process exit after `powerdown` in standalone and full-CI runs.
One earlier CI repeat passed reset but exited unsuccessfully after
`powerdown`; its cause is unknown. No new control command has been added.

## Secret handling and safety

The live test loads `host.apple_smc_osk` with the WCL parser at runtime.
`VMLAB_HV_TEST_APPLE_SMC_OSK` is the environment alternative. Missing keys,
templates, or KVM access produce explicit skip reasons.

The launch description is an anonymous memfd. Debug output and description
dumps redact the OSK. Snapshot device state stores the SMC transaction
cursor, not OSK bytes or read-response buffers. Restore uses the key from
the current description. SMC port values are excluded from I/O diagnostics.

Malformed SMC sections originally exposed supplied values in Serde errors.
Fake-key regressions reproduced this through both the public protocol type
and the real CLI. SMC deserialization now returns a value-free schema error.
CLI JSON errors report the file, line, and column without input values.
All 30 wire tests and the CLI regression pass. No real key was used in
these malformed-input tests.

The task brief forbids the OSK in every snapshot file. The specification
requires complete RAM snapshots and explicitly forbids it in device state.
These requirements differ because guest RAM can contain OSK responses.
The implementation follows the specification. The live acceptance test
automatically removes its private snapshot directory, including on failure.
The private snapshot directories used by the lifecycle runs were removed
and cannot be recovered. Pre-snapshot failure screenshots and overlays remain
for diagnosis. The original template is unchanged.

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
