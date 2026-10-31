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
| `rtc-index` | `0x70` | write | 1 | 1375 | 15 | 0x0, 0x2, 0x4, 0x6-0xb, 0x80, 0x82, 0x84, 0x88, 0x8a-0x8d, 0x8f-0x90, 0xb2, 0xb8-0xb9, 0xbd, 0xdf |
| `rtc` | `0x71` | read | 1 | 1373 | 17 | 0x0-0x9, 0x10-0x19, 0x20-0x29, 0x30-0x39, 0x40-0x49, 0x50-0x59, 0x80 |
| `rtc` | `0x71` | write | 1 | 3 | 35 | 0x0, 0x2, 0x26 |
| `rtc-index` | `0x70` | read | 1 | 6030 | 859 | 0xff |
| `pcspk` | `0x61` | read | 1 | 2 | 328794 | 0x0, 0x10 |
| `pcspk` | `0x61` | write | 1 | 2 | 328795 | 0x0, 0x10 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `mc146818_rtc_ioport_read` | 1373 | 16 |
| `mc146818_rtc_ioport_write` | 3 | 36 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
