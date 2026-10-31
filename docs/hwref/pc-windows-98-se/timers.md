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
| `rtc-index` | `0x70` | write | 1 | 2370 | 17 | 0x0, 0xf-0x2f, 0x37, 0x80, 0x82, 0x84, 0x87-0x8d, 0x8f-0x90, 0xb2, 0xb8-0xb9, 0xbd, 0xdf |
| `mc146818_rtc_ioport_read` | `0x0f` | indexed event | - | 6 | 18 | 0x0 |
| `rtc` | `0x71` | read | 1 | 2366 | 19 | 0x0-0x2, 0x4-0x6, 0x9, 0x10-0x13, 0x17, 0x20-0x26, 0x28-0x29, 0x2f-0x31, 0x3f, 0x41, 0x44-0x45, 0x50, 0x59, 0x80, 0xc8, 0xf0, 0xff |
| `rtc` | `0x71` | write | 1 | 5 | 37 | 0x0, 0x2, 0x26 |
| `mc146818_rtc_ioport_write` | `0x0f` | indexed event | - | 1 | 38 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x38` | indexed event | - | 2 | 86 | 0x30 |
| `mc146818_rtc_ioport_read` | `0x3d` | indexed event | - | 1 | 89 | 0x12 |
| `mc146818_rtc_ioport_read` | `0x08` | indexed event | - | 258 | 930 | 0x10 |
| `mc146818_rtc_ioport_read` | `0x5f` | indexed event | - | 2 | 933 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x00` | indexed event | - | 262 | 943 | 0x0-0x1, 0x4-0x5, 0x9, 0x10-0x13, 0x17, 0x21-0x26, 0x28-0x29, 0x59 |
| `rtc-index` | `0x70` | read | 1 | 5902 | 946 | 0xff |
| `mc146818_rtc_ioport_write` | `0x0a` | indexed event | - | 2 | 983 | 0x26 |
| `mc146818_rtc_ioport_read` | `0x0b` | indexed event | - | 258 | 985 | 0x2 |
| `mc146818_rtc_ioport_write` | `0x0b` | indexed event | - | 2 | 988 | 0x2 |
| `mc146818_rtc_ioport_read` | `0x0c` | indexed event | - | 1 | 990 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x0d` | indexed event | - | 1 | 993 | 0x80 |
| `mc146818_rtc_ioport_read` | `0x0a` | indexed event | - | 513 | 996 | 0x26 |
| `mc146818_rtc_ioport_read` | `0x02` | indexed event | - | 257 | 1002 | 0x44-0x45 |
| `mc146818_rtc_ioport_read` | `0x04` | indexed event | - | 257 | 1005 | 0x20 |
| `mc146818_rtc_ioport_read` | `0x32` | indexed event | - | 2 | 1008 | 0x20 |
| `mc146818_rtc_ioport_read` | `0x10` | indexed event | - | 2 | 41164 | 0x50 |
| `mc146818_rtc_ioport_read` | `0x39` | indexed event | - | 1 | 59255 | 0x2 |
| `mc146818_rtc_ioport_read` | `0x09` | indexed event | - | 256 | 268156 | 0x26 |
| `mc146818_rtc_ioport_read` | `0x07` | indexed event | - | 256 | 268162 | 0x31 |
| `mc146818_rtc_ioport_read` | `0x2d` | indexed event | - | 1 | 25923435 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x2c` | indexed event | - | 1 | 25923438 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x2b` | indexed event | - | 1 | 25923441 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x2a` | indexed event | - | 1 | 25923444 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x29` | indexed event | - | 1 | 25923447 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x28` | indexed event | - | 1 | 25923450 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x27` | indexed event | - | 1 | 25923453 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x26` | indexed event | - | 1 | 25923456 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x25` | indexed event | - | 1 | 25923459 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x24` | indexed event | - | 1 | 25923462 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x23` | indexed event | - | 1 | 25923465 | 0x3f |
| `mc146818_rtc_ioport_read` | `0x22` | indexed event | - | 1 | 25923468 | 0x10 |
| `mc146818_rtc_ioport_read` | `0x21` | indexed event | - | 1 | 25923471 | 0x41 |
| `mc146818_rtc_ioport_read` | `0x20` | indexed event | - | 1 | 25923474 | 0xc8 |
| `mc146818_rtc_ioport_read` | `0x1f` | indexed event | - | 1 | 25923477 | 0xff |
| `mc146818_rtc_ioport_read` | `0x1e` | indexed event | - | 1 | 25923480 | 0xff |
| `mc146818_rtc_ioport_read` | `0x1d` | indexed event | - | 1 | 25923483 | 0x10 |
| `mc146818_rtc_ioport_read` | `0x1c` | indexed event | - | 1 | 25923486 | 0x10 |
| `mc146818_rtc_ioport_read` | `0x1b` | indexed event | - | 1 | 25923489 | 0x41 |
| `mc146818_rtc_ioport_read` | `0x1a` | indexed event | - | 1 | 25923492 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x19` | indexed event | - | 1 | 25923495 | 0x2f |
| `mc146818_rtc_ioport_read` | `0x18` | indexed event | - | 1 | 25923498 | 0xff |
| `mc146818_rtc_ioport_read` | `0x17` | indexed event | - | 1 | 25923501 | 0xff |
| `mc146818_rtc_ioport_read` | `0x16` | indexed event | - | 1 | 25923504 | 0x2 |
| `mc146818_rtc_ioport_read` | `0x15` | indexed event | - | 1 | 25923507 | 0x80 |
| `mc146818_rtc_ioport_read` | `0x14` | indexed event | - | 1 | 25923510 | 0x6 |
| `mc146818_rtc_ioport_read` | `0x13` | indexed event | - | 1 | 25923513 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x12` | indexed event | - | 1 | 25923516 | 0xf0 |
| `mc146818_rtc_ioport_read` | `0x11` | indexed event | - | 1 | 25923519 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x2e` | indexed event | - | 1 | 25923525 | 0x0 |
| `mc146818_rtc_ioport_read` | `0x2f` | indexed event | - | 1 | 25923528 | 0x0 |
| `pcspk` | `0x61` | read | 1 | 312 | 31711743 | 0x0, 0x10, 0x20, 0x30 |
| `pcspk` | `0x61` | write | 1 | 160 | 46877946 | 0x0, 0x10, 0x20, 0x30, 0x80, 0x90, 0xa0, 0xb0 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `mc146818_rtc_ioport_read` | 2366 | 18 |
| `mc146818_rtc_ioport_write` | 5 | 38 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
