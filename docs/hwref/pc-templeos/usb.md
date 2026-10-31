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
| `uhci` | `0xc000` | write | 2 | 4 | 41044 | 0x0, 0x2, 0xc1 |
| `usb_uhci_mmio_writew` | `0x0000` | indexed event | - | 4 | 41045 | 0x0, 0x2, 0xc1 |
| `uhci` | `0xc004` | write | 2 | 1 | 41047 | 0x0 |
| `usb_uhci_mmio_writew` | `0x0004` | indexed event | - | 1 | 41048 | 0x0 |
| `uhci` | `0xc00c` | write | 2 | 1 | 41051 | 0x40 |
| `usb_uhci_mmio_writew` | `0x000c` | indexed event | - | 1 | 41052 | 0x40 |
| `uhci` | `0xc008` | write | 2 | 1 | 41053 | 0xe000 |
| `usb_uhci_mmio_writew` | `0x0008` | indexed event | - | 1 | 41054 | 0xe000 |
| `uhci` | `0xc00a` | write | 2 | 1 | 41055 | 0x1ffd |
| `usb_uhci_mmio_writew` | `0x000a` | indexed event | - | 1 | 41056 | 0x1ffd |
| `uhci` | `0xc006` | write | 2 | 1 | 41057 | 0x0 |
| `usb_uhci_mmio_writew` | `0x0006` | indexed event | - | 1 | 41058 | 0x0 |
| `uhci` | `0xc010` | read | 2 | 2 | 41063 | 0x83 |
| `uhci` | `0xc010` | write | 2 | 4 | 41064 | 0x0, 0x4, 0x200 |
| `usb_uhci_mmio_writew` | `0x0010` | indexed event | - | 4 | 41065 | 0x0, 0x4, 0x200 |
| `uhci` | `0xc012` | read | 2 | 21 | 41106 | 0x80 |
| `uhci` | `0xc006` | read | 2 | 7 | 54493 | 0x63-0x64 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `usb_uhci_reset` | 2 | 8 |
| `usb_uhci_mmio_writew` | 13 | 41045 |
| `usb_uhci_schedule_start` | 1 | 41061 |
| `usb_uhci_mmio_readw` | 30 | 41062 |
| `usb_uhci_frame_start` | 100 | 41310 |
| `usb_uhci_qh_load` | 236 | 41311 |
| `usb_uhci_td_load` | 112 | 41313 |
| `usb_uhci_td_nextqh` | 96 | 41314 |
| `usb_uhci_queue_add` | 2 | 49339 |
| `usb_uhci_packet_add` | 16 | 49340 |
| `usb_uhci_packet_complete_success` | 16 | 49341 |
| `usb_uhci_packet_del` | 16 | 49342 |
| `usb_uhci_td_complete` | 16 | 49343 |
| `usb_uhci_frame_stop_bandwidth` | 4 | 49349 |
| `usb_uhci_queue_del` | 2 | 53485 |
| `usb_uhci_schedule_stop` | 1 | 54578 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
