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
| `rtc-index` | `0x70` | write | 1 | 180 | 18 | 0x0, 0x2, 0x4, 0x6-0xd, 0xf, 0x15, 0x32, 0x80, 0x82, 0x84, 0x87-0x8d, 0x8f-0x90, 0xb2, 0xb8-0xb9, 0xbd, 0xdf |
| `rtc` | `0x71` | read | 1 | 168 | 20 | 0x0-0x2, 0x7, 0x10, 0x12, 0x17-0x18, 0x20, 0x24, 0x26, 0x28-0x29, 0x30-0x31, 0x41, 0x43, 0x47, 0x49, 0x50, 0x53, 0x55-0x56, 0x58, 0x80 |
| `rtc` | `0x71` | write | 1 | 15 | 38 | 0x0, 0x2, 0xa, 0x26 |
| `rtc-index` | `0x70` | read | 1 | 11431 | 1037 | 0xff |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `mc146818_rtc_ioport_read` | 168 | 19 |
| `mc146818_rtc_ioport_write` | 15 | 39 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
