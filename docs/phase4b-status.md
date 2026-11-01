# Phase 4b status

Paused by the owner on 2026-11-01 so modern x86 work can take priority.
The branch is `wp/p4b-nics`. Nothing was merged. All jobs and VMs owned by
this worktree were stopped, and the process check found none remaining.

## Implemented

- Intel 82540EM: PCI/MMIO/I/O, MAC EEPROM, PHY/MDIC, legacy and extended
  descriptors, RX/TX rings, filters, VLAN handling, checksum/segmentation
  offload, interrupts and delay timers, checked DMA, reset and snapshots.
- AMD Am79C970A: PROM, word/dword I/O, CSR/BCR access, initialization,
  software styles 0/1/2/3, descriptor chains, filters, interrupts, checked
  DMA, reset and snapshots.
- Both use a bounded, reconnecting stream-socket backend with a snapshot
  barrier. Option ROMs are absent unless supplied. The pc builder accepts
  both models and retains virtio as the existing protocol default.
- No existing device-trait or wire-format signatures changed. The existing
  `DeviceFactory::virtio_net` entry point now dispatches by NIC model.
- A separately committed control-client fix restores the socket read timeout
  after `wait_event_timeout`. Its leaked timeout blocked a Windows snapshot.

## Verified

- Thirty NIC-related unit/backend tests passed, including the existing
  virtio-net tests. Coverage includes EEPROM, rings, all PCnet styles,
  checksum/segmentation offload, VLANs, interrupt timing, reset, snapshots,
  fragmented socket framing, reconnects and invalid DMA.
- FreeDOS 1.3 on a fresh overlay passed S1 packet-driver installation, DHCP,
  ping, snapshot rollback with a reboot guard, and ping after in-place reset.
  Its final image passed `qemu-img check`.
- Windows 7 7600.0.1 passed setup completion, an initial in-place reboot,
  DHCP and ping through e1000. Testing before setup completion was premature.
- P2 passed the S1 firmware differential for all four pc captures with NICs
  included, plus all 256 PCI configuration bytes and BAR sizing against the
  legacy superset capture. Allowed NIC differences are assigned BAR addresses,
  IRQ routing, BIOS-only bus-master state, and omission of the captured ROM.
- NIC wire/default and validation tests passed. The control timeout regression
  failed before its fix and passed afterward. Latest fmt and workspace Clippy
  with `-D warnings` passed.

## Not completed

- Windows network snapshot/restore and the subsequent reset require a full
  rerun after the control-client fix. The final run was stopped on request.
- `just ci::check` was interrupted during macOS acceptance. Earlier disk S2
  and modern x86 checks passed, but the full gate is not recorded as green.
- Windows 98 PCnet guest networking remains assigned to phase 4d. Floppy,
  ISA DMA and UHCI remain phase 4c; Cirrus remains phase 4d.

## Resume

Run `cargo test -p vmlab-hv-tests --test pc_nics -- --nocapture --test-threads=1`,
then `just ci::check`. Keep the host-wide four-VM limit and memory scopes.
Diagnostic logs remain at `/var/tmp/p4b-final-guests.log`,
`/var/tmp/p4b-p2-s1.log`, `/var/tmp/p4b-ci.log`, and `/tmp/p4b-windows.log`.
