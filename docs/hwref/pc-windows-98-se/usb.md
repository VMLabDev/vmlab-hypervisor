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
| `uhci` | `0xc000` | write | 2 | 4 | 41098 | 0x0, 0x2, 0xc1 |
| `usb_uhci_mmio_writew` | `0x0000` | indexed event | - | 4 | 41099 | 0x0, 0x2, 0xc1 |
| `uhci` | `0xc004` | write | 2 | 1 | 41101 | 0x0 |
| `usb_uhci_mmio_writew` | `0x0004` | indexed event | - | 1 | 41102 | 0x0 |
| `uhci` | `0xc00c` | write | 2 | 1 | 41105 | 0x40 |
| `usb_uhci_mmio_writew` | `0x000c` | indexed event | - | 1 | 41106 | 0x40 |
| `uhci` | `0xc008` | write | 2 | 1 | 41107 | 0xe000 |
| `usb_uhci_mmio_writew` | `0x0008` | indexed event | - | 1 | 41108 | 0xe000 |
| `uhci` | `0xc00a` | write | 2 | 1 | 41109 | 0x7fd |
| `usb_uhci_mmio_writew` | `0x000a` | indexed event | - | 1 | 41110 | 0x7fd |
| `uhci` | `0xc006` | write | 2 | 1 | 41111 | 0x0 |
| `usb_uhci_mmio_writew` | `0x0006` | indexed event | - | 1 | 41112 | 0x0 |
| `uhci` | `0xc010` | read | 2 | 2 | 41117 | 0x83 |
| `uhci` | `0xc010` | write | 2 | 4 | 41118 | 0x0, 0x4, 0x200 |
| `usb_uhci_mmio_writew` | `0x0010` | indexed event | - | 4 | 41119 | 0x0, 0x4, 0x200 |
| `uhci` | `0xc012` | read | 2 | 21 | 41160 | 0x80 |
| `uhci` | `0xc006` | read | 2 | 14 | 56708 | 0x64-0x65 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `usb_uhci_reset` | 2 | 10 |
| `usb_uhci_mmio_writew` | 13 | 41099 |
| `usb_uhci_schedule_start` | 1 | 41115 |
| `usb_uhci_mmio_readw` | 37 | 41116 |
| `usb_uhci_frame_start` | 101 | 41357 |
| `usb_uhci_qh_load` | 239 | 41358 |
| `usb_uhci_td_load` | 113 | 41360 |
| `usb_uhci_td_nextqh` | 97 | 41361 |
| `usb_uhci_queue_add` | 2 | 51497 |
| `usb_uhci_packet_add` | 16 | 51498 |
| `usb_uhci_packet_complete_success` | 16 | 51499 |
| `usb_uhci_packet_del` | 16 | 51500 |
| `usb_uhci_td_complete` | 16 | 51501 |
| `usb_uhci_frame_stop_bandwidth` | 4 | 51507 |
| `usb_uhci_queue_del` | 2 | 55634 |
| `usb_uhci_schedule_stop` | 1 | 56844 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
