# floppy-dma observations

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
| `dma-cont` | `0xd` | write | 1 | 3 | 99 | 0x0 |
| `dma-cont` | `0xda` | write | 1 | 3 | 100 | 0x0 |
| `dma-cont` | `0xd6` | write | 1 | 3 | 101 | 0xc0 |
| `dma-cont` | `0xd4` | write | 1 | 3 | 102 | 0x0 |
| `fdc` | `0x3f2` | write | 1 | 10 | 41624688 | 0x8, 0xc, 0x1c |
| `fdc` | `0x3f4` | read | 1 | 364 | 41624693 | 0x80, 0x90, 0xd0 |
| `fdc` | `0x3f5` | write | 1 | 178 | 41624694 | 0x0-0x1, 0x8, 0xf-0x10, 0x12, 0x18, 0x80 |
| `fdc` | `0x3f5` | read | 1 | 102 | 41624703 | 0x0-0x1, 0x20, 0x41, 0x90, 0xc0-0xc3 |
| `fdc` | `0x3f7` | read | 1 | 40 | 42363010 | 0x80 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `fdc_ioport_write` | 188 | 41624689 |
| `fdc_ioport_read` | 506 | 41624692 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
