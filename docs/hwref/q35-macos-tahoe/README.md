# macos-tahoe capture blocked

No macOS VM was started for this capture on 2026-10-31.
The current vmlab host-config parser reports `apple_smc_osk.is_some() = false`.
The owner must set `host.apple_smc_osk` in `~/.config/vmlab/config.wcl`
before the macOS capture can proceed. No OSK value was printed or saved.

The installed vmlab executable predates this validation requirement.
Its successful `vmlab validate` result was not used as evidence that the
key is configured. The presence check called `HostConfig::load_default()`
through the locally compiled vmlab library and printed only a boolean.

## Missing observations

- M1: VMware SVGA II registers and FIFO commands are unmeasured.
  The FIFO verdict is **unknown**, not "no commands".
- M2: XNU's unhandled MSR list is unmeasured. Host `ignore_msrs` is `Y`,
  `report_ignored_msrs` is `N`, and unprivileged `dmesg` is unreadable.
  A subsequent capture needs owner-authorized access to a KVM MSR trace
  or ignored-MSR kernel logging. This task changed neither host setting.
- M3: xHCI/HID, AHCI, transitional virtio, SMC command/key traffic,
  static machine state, and guest exercises remain uncaptured.

This directory records a blocked prerequisite. It is not a hardware reference
and cannot satisfy phase-macos D2 or D3.
