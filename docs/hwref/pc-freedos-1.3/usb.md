# usb observations

Counts cover the full trace stream, including lines omitted from the bounded raw log.
Addresses retain QEMU trace notation. Use `mtree.txt` to interpret regions and aliases.
A range in the value column includes every integer between its endpoints.
Aperture rows combine framebuffer byte addresses; they are not indexed register decodes.
Physical-memory snapshots can add observer reads to legacy VGA aperture rows.
Those aperture counts are not guest-only counts. Host fw_cfg inventory reads are excluded.
First use is the 1-based line number across all enabled events, including firmware.

## Register accesses

| Region or event | Address or index | Operation | Bytes | Count | First use | Values |
|---|---|---|---:|---:|---:|---|
| `uhci` | `0xc000` | write | 2 | 4 | 41099 | 0x0, 0x2, 0xc1 |
| `usb_uhci_mmio_writew` | `0x0000` | indexed event | - | 4 | 41100 | 0x0, 0x2, 0xc1 |
| `uhci` | `0xc004` | write | 2 | 1 | 41102 | 0x0 |
| `usb_uhci_mmio_writew` | `0x0004` | indexed event | - | 1 | 41103 | 0x0 |
| `uhci` | `0xc00c` | write | 2 | 1 | 41106 | 0x40 |
| `usb_uhci_mmio_writew` | `0x000c` | indexed event | - | 1 | 41107 | 0x40 |
| `uhci` | `0xc008` | write | 2 | 1 | 41108 | 0xe000 |
| `usb_uhci_mmio_writew` | `0x0008` | indexed event | - | 1 | 41109 | 0xe000 |
| `uhci` | `0xc00a` | write | 2 | 1 | 41110 | 0x3fd |
| `usb_uhci_mmio_writew` | `0x000a` | indexed event | - | 1 | 41111 | 0x3fd |
| `uhci` | `0xc006` | write | 2 | 1 | 41112 | 0x0 |
| `usb_uhci_mmio_writew` | `0x0006` | indexed event | - | 1 | 41113 | 0x0 |
| `uhci` | `0xc010` | read | 2 | 2 | 41118 | 0x83 |
| `uhci` | `0xc010` | write | 2 | 4 | 41119 | 0x0, 0x4, 0x200 |
| `usb_uhci_mmio_writew` | `0x0010` | indexed event | - | 4 | 41120 | 0x0, 0x4, 0x200 |
| `uhci` | `0xc012` | read | 2 | 21 | 41161 | 0x80 |
| `uhci` | `0xc006` | read | 2 | 35 | 56267 | 0x65-0x66 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `usb_uhci_reset` | 2 | 10 |
| `usb_uhci_mmio_writew` | 13 | 41100 |
| `usb_uhci_schedule_start` | 1 | 41116 |
| `usb_uhci_mmio_readw` | 58 | 41117 |
| `usb_uhci_frame_start` | 102 | 41358 |
| `usb_uhci_qh_load` | 242 | 41359 |
| `usb_uhci_td_load` | 114 | 41361 |
| `usb_uhci_td_nextqh` | 98 | 41362 |
| `usb_uhci_queue_add` | 2 | 51304 |
| `usb_uhci_packet_add` | 16 | 51305 |
| `usb_uhci_packet_complete_success` | 16 | 51306 |
| `usb_uhci_packet_del` | 16 | 51307 |
| `usb_uhci_td_complete` | 16 | 51308 |
| `usb_uhci_frame_stop_bandwidth` | 4 | 51314 |
| `usb_uhci_queue_del` | 2 | 55131 |
| `usb_uhci_schedule_stop` | 1 | 56451 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
