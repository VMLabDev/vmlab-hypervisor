# timers observations

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
| `rtc-index` | `0x70` | write | 1 | 60 | 17 | 0x0, 0x80, 0x82, 0x84, 0x87-0x8d, 0x8f-0x90, 0xb2, 0xb8-0xb9, 0xbd, 0xdf |
| `mc146818_rtc_ioport_read` | `0x0f` | indexed event | - | 3 | 18 | 0x0 |
| `rtc` | `0x71` | read | 1 | 42 | 19 | 0x0, 0x2, 0x10, 0x12, 0x14, 0x20, 0x26, 0x30-0x31, 0x42, 0x50, 0x80 |
| `rtc` | `0x71` | write | 1 | 19 | 37 | 0x0, 0x2, 0x10, 0x14, 0x20, 0x26, 0x31, 0x42 |
| `mc146818_rtc_ioport_write` | `0x0f` | indexed event | - | 1 | 38 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x38` | indexed event | - | 2 | 87 | 0x30 |
| `mc146818_rtc_ioport_read` | `0x3d` | indexed event | - | 1 | 90 | 0x12 |
| `mc146818_rtc_ioport_read` | `0x08` | indexed event | - | 3 | 931 | 0x10 |
| `mc146818_rtc_ioport_read` | `0x5f` | indexed event | - | 2 | 934 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x00` | indexed event | - | 7 | 944 | 0x14 |
| `rtc-index` | `0x70` | read | 1 | 5700 | 947 | 0xff |
| `mc146818_rtc_ioport_write` | `0x0a` | indexed event | - | 1 | 984 | 0x26 |
| `mc146818_rtc_ioport_read` | `0x0b` | indexed event | - | 6 | 986 | 0x2 |
| `mc146818_rtc_ioport_write` | `0x0b` | indexed event | - | 5 | 989 | 0x2 |
| `mc146818_rtc_ioport_read` | `0x0c` | indexed event | - | 1 | 991 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x0d` | indexed event | - | 1 | 994 | 0x80 |
| `mc146818_rtc_ioport_read` | `0x0a` | indexed event | - | 7 | 997 | 0x26 |
| `mc146818_rtc_ioport_read` | `0x02` | indexed event | - | 2 | 1003 | 0x42 |
| `mc146818_rtc_ioport_read` | `0x04` | indexed event | - | 2 | 1006 | 0x20 |
| `mc146818_rtc_ioport_read` | `0x32` | indexed event | - | 1 | 1009 | 0x20 |
| `mc146818_rtc_ioport_read` | `0x10` | indexed event | - | 1 | 41165 | 0x50 |
| `mc146818_rtc_ioport_read` | `0x39` | indexed event | - | 1 | 58898 | 0x2 |
| `mc146818_rtc_ioport_read` | `0x09` | indexed event | - | 1 | 124020 | 0x26 |
| `mc146818_rtc_ioport_read` | `0x07` | indexed event | - | 1 | 124026 | 0x31 |
| `mc146818_rtc_ioport_write` | `0x00` | indexed event | - | 2 | 124033 | 0x14 |
| `mc146818_rtc_ioport_write` | `0x02` | indexed event | - | 2 | 124036 | 0x42 |
| `mc146818_rtc_ioport_write` | `0x04` | indexed event | - | 2 | 124039 | 0x20 |
| `mc146818_rtc_ioport_write` | `0x09` | indexed event | - | 2 | 124051 | 0x26 |
| `mc146818_rtc_ioport_write` | `0x08` | indexed event | - | 2 | 124054 | 0x10 |
| `mc146818_rtc_ioport_write` | `0x07` | indexed event | - | 2 | 124057 | 0x31 |
| `pcspk` | `0x61` | read | 1 | 81 | 1561507 | 0x0, 0x10, 0x20, 0x30 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `mc146818_rtc_ioport_read` | 42 | 18 |
| `mc146818_rtc_ioport_write` | 19 | 38 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
