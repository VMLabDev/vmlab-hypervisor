# xHCI and tablet integration reference

This module implements the phase-2 D6 controller and the P4 guest-interface
tests. It has no KVM dependencies. The machine package owns PCI placement,
MMIO routing, periodic polling, and control/display input routing.

## Controller API

```text
XhciController::new(
    id: impl Into<String>,
    mem: GuestMemory,
    msi: Arc<dyn MsiSink>,
    intx: Option<Arc<dyn IrqLine>>,
) -> XhciController
```

The constructor connects one `Tablet::default()` to `TABLET_PORT`, root port 5.
`id` is the machine's stable snapshot section key. Construction does not start
DMA. The guest enables PCI bus mastering and sets USBCMD.Run before ringing
doorbells.

`PciDevice` supplies configuration space, BAR sizing, and BAR accesses. BAR0
is 16 KiB, 64-bit, and non-prefetchable. `BusDevice` accesses use the same
BAR-relative register offsets. The controller has 64 slots, 16 interrupters,
four USB3 root ports numbered 1..4, and four USB2 ports numbered 5..8.

`poll(&mut self)` resumes NAKed transfers and bounded command/transfer ring
work. The machine calls it periodically while running and when an asynchronous
backend becomes ready. A cadence of 1 ms services the tablet's HID idle timer;
input injection also polls immediately. Each command or endpoint pass handles
at most 256 work items. Link walks and TD walks are independently bounded.

The controller implements `Snapshot` and `PciSnapshot`. Its function section
includes registers, PCI configuration, interrupt state, ring positions, slots,
endpoint state, pending events, and every attached backend. Its wrapped section
is empty because there is no separate transport-wrapped device. The machine
quiesces accesses and restores guest RAM together with device state. Restore
rejects different root-port occupancy or backend IDs. Backend snapshot errors
propagate to the caller.

## Pointer input

```rust,ignore
controller.inject_pointer(PointerEvent {
    x: 16384,
    y: 32767,
    buttons: 1,
    wheel: -1,
})?;
```

The coordinates are `u16`, clamped to 0..32767. Button bits 0..4 mean left,
right, middle, back, and forward. The signed wheel is clamped to -127..127;
positive values scroll up. The HID input report is six bytes: buttons,
little-endian X, little-endian Y, and wheel. The endpoint's maximum packet is
eight bytes, so an eight-byte receive completes with a two-byte short packet.

Injection queues input and services an outstanding interrupt receive. Without
input, the tablet NAKs until its nonzero HID idle duration expires. The queue
holds 1024 reports; a full queue returns an error without dropping an older
button or wheel event. Injection also errors if port 5 is disconnected or its
backend does not accept pointer input.

`Tablet::new(serial)` sets a stable USB serial. `Tablet::default()` uses
`28754-0000:00:01.0-1`, the serial observed at PCI 00:01.0 on QEMU's first
connector. Another PCI placement can replace the default tablet with a
`Tablet::new` instance through `detach` and `attach` before guest execution.

## Root-port backend API

`attach(port: u8, device: Box<dyn UsbDevice>) -> Result<(), DeviceError>` accepts
1-based port numbers. SuperSpeed devices use ports 1..4; other speeds use
5..8. An occupied port, invalid port, or incompatible protocol returns an error.

`detach(port: u8) -> Result<Box<dyn UsbDevice>, DeviceError>` returns ownership
of the backend and posts a port change. An invalid or empty port returns an
error. Both operations update PORTSC and generate an event when running.

`UsbDevice: Snapshot` defines `speed`, `reset`, `control`, `transfer`, and
optional pointer injection. `SetupPacket` contains decoded USB setup fields in
host byte order. `transfer` endpoint addresses use the USB direction bit,
not xHCI endpoint-context indices. OUT payloads are supplied in `data`; IN
capacity is supplied in `max_length`.

`UsbTransfer::Ack(Vec<u8>)` returns IN bytes or an empty OUT acknowledgement.
`Nak` keeps the TD queued at its original dequeue pointer. `Stall` halts the
endpoint and produces a transfer error event. Backend calls must not block.
An asynchronous backend retains its in-flight work across NAKs and completes
it when `poll` retries. Physical device backends can refuse online snapshots
through their `Snapshot` implementation.

## Supported guest operations and limits

The command ring implements Enable/Disable Slot, Address Device, Configure
Endpoint, Evaluate Context, Reset/Stop Endpoint, Set TR Dequeue Pointer, Reset
Device, and No Op. Control, bulk, and interrupt endpoints use 32-byte contexts.
Transfer rings support Links, cycle toggling, scatter/gather buffers, immediate
OUT data, Event Data, short packets, and endpoint halt/recovery. MSI-X masks,
the pending-bit array, interrupt moderation, and optional legacy INTx are implemented.

Each interrupter's event ring supports up to 16 segments, each 16..4096 TRBs. Full rings
retain pending events rather than overwriting guest-owned entries. Transfers
are limited to 1 MiB and 256 TRBs per TD. Invalid DMA or cyclic Link walks
halt the controller with USBSTS.HCE, recoverable through HCRST.

Optional streams are not advertised. Isochronous endpoints and USB hubs are
outside the built-in tablet path; unsupported endpoint contexts and commands
return errors. Host passthrough is a later package, not an attached physical
backend in this implementation.

## Specifications and black-box observations

Register and ring definitions follow Intel xHCI 1.2b, with HCIVERSION 1.0:
https://cdrdv2-public.intel.com/625472/625472_xHCI_Rev1_2b.pdf

USB class requests and HID reports follow USB HID 1.11:
https://www.usb.org/sites/default/files/hid1_11.pdf

QEMU 11.1.2 was measured without reading its implementation:

```sh
printf 'info pci\ninfo qtree\nquit\n' |
  qemu-system-x86_64 -machine q35 -accel tcg -S -m 128 -nodefaults \
  -display none -device qemu-xhci,id=xhci \
  -device usb-tablet,bus=xhci.0 -monitor stdio
```

The observed PCI identity was `1b36:000d`, revision 1, class `0c0330`, subsystem
`1af4:1100`, with 64-bit BAR0. Qtest config-space probes measured a `ffffc004`
BAR mask, MSI-X capability at `0x90`, table at BAR0+`0x3000`, PBA at `0x3800`,
and PCI Express capability at `0xa0`. MMIO probes measured operational
registers at `0x40`, runtime registers at `0x1000`, doorbells at `0x2000`,
USB2 ports 5..8, and USB3 ports 1..4. All 16 interrupters have independent
event rings, MSI-X entries, masks, pending bits, moderation and snapshot state.
QEMU also advertises streams; stream support remains a hardware-parity gap.

A Linux 6.12.114 probe booted under QEMU/KVM with `-cpu host`, the same two
USB devices, and a disposable Alpine initramfs. USBDEVFS_CONTROL descriptor
reads observed tablet `0627:0001`, USB 2.0, manufacturer `QEMU`, product
`QEMU USB Tablet`, one HID interface, endpoint `0x81`, maximum packet 8, and
high-speed interval 4. The 74-byte report descriptor is preserved as
`TABLET_REPORT_DESCRIPTOR`. Device/configuration, string, qualifier,
other-speed, BOS, and HID descriptors are encoded in `tablet.rs` from those
reads. No QEMU source or crosvm code was copied.

## Verification

`cargo test -p vmlab-hv-devices usb::` drives the public PCI/MMIO and guest-DMA
interfaces, rather than calling command/transfer helpers. The P4 tests cover
enumeration and descriptors, injected reports, snapshots, interrupt masks,
ring wrapping/backpressure, link cycles, scatter/gather, Event Data, endpoint
recovery, malformed input, and DMA above 4 GiB.

The machine integration's Linux 6.12.114 hardware probe enumerates the tablet
through this controller. P1 template acceptance also boots Debian 13 and
Ubuntu 26.04.3 through the q35 machine. Full display/input acceptance remains P3.
