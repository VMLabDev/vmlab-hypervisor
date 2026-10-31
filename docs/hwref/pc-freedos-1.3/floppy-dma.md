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
| `dma-cont` | `0xd` | write | 1 | 1 | 97 | 0x0 |
| `dma-cont` | `0xda` | write | 1 | 1 | 98 | 0x0 |
| `dma-cont` | `0xd6` | write | 1 | 1 | 99 | 0xc0 |
| `dma-cont` | `0xd4` | write | 1 | 1 | 100 | 0x0 |
| `dma-chan` | `0x4` | read | 1 | 2 | 1561564 | 0x0 |
| `dma-chan` | `0x5` | read | 1 | 2 | 1561565 | 0x0 |
| `dma-chan` | `0x4` | write | 1 | 2 | 1561566 | 0x0, 0x4 |
| `dma-chan` | `0x5` | write | 1 | 2 | 1561567 | 0x0 |
| `dma-chan` | `0x0` | write | 1 | 2 | 1561568 | 0x0 |
| `dma-chan` | `0x1` | write | 1 | 2 | 1561569 | 0x0 |
| `dma-chan` | `0x2` | write | 1 | 2 | 1561570 | 0x0 |
| `dma-chan` | `0x3` | write | 1 | 2 | 1561571 | 0x0 |

## Device events

| Event | Count | First use |
|---|---:|---:|

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
