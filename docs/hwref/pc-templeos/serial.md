# serial observations

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
| `serial` | `0x3f9` | write | 1 | 4 | 41269 | 0x0, 0x2 |
| `serial` | `0x3f9` | read | 1 | 1 | 41272 | 0x2 |
| `serial` | `0x3fa` | read | 1 | 1 | 41274 | 0x2 |
| `serial` | `0x2f9` | write | 1 | 2 | 41277 | 0x0, 0x2 |
| `serial` | `0x2f9` | read | 1 | 1 | 41280 | 0x2 |
| `serial` | `0x2fa` | read | 1 | 1 | 41282 | 0x2 |
| `serial` | `0x3fb` | write | 1 | 2 | 25335118 | 0x3, 0x80 |
| `serial` | `0x3f8` | write | 1 | 1402 | 25335121 | 0x0-0x5, 0xa, 0xe, 0x10-0x11, 0x16, 0x19, 0x1f-0x20, 0x22, 0x24, 0x2c-0x3a, 0x3d, 0x41-0x46, 0x48-0x49, 0x4b-0x4d, 0x52-0x54, 0x56-0x58, 0x5a-0x5b, 0x5d, 0x5f, 0x61-0x69, 0x6b-0x70, 0x72-0x79, 0x7b, 0x7d, 0x8b, 0xc7 |
| `serial` | `0x3fa` | write | 1 | 1 | 25335130 | 0xc7 |
| `serial` | `0x3fc` | write | 1 | 1 | 25335132 | 0xb |
| `serial` | `0x3fd` | read | 1 | 14281522 | 25335135 | 0x60-0x61 |
| `serial` | `0x3f8` | read | 1 | 1055 | 25526693 | 0x0, 0xe, 0x12, 0x14, 0x20, 0x22, 0x25, 0x28-0x2a, 0x2c, 0x2e-0x2f, 0x31-0x36, 0x38-0x3d, 0x42-0x49, 0x4b-0x4d, 0x52-0x58, 0x5b-0x5d, 0x5f, 0x61-0x69, 0x6b-0x70, 0x72-0x79, 0x7b, 0x7d-0x7e, 0xaf |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `serial_write` | 1412 | 41270 |
| `serial_read` | 14282581 | 41271 |
| `serial_update_parameters` | 4 | 25335120 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
