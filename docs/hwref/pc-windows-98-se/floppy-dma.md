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
| `dma-cont` | `0xd` | write | 1 | 1 | 96 | 0x0 |
| `dma-cont` | `0xda` | write | 1 | 1 | 97 | 0x0 |
| `dma-cont` | `0xd6` | write | 1 | 1 | 98 | 0xc0 |
| `dma-cont` | `0xd4` | write | 1 | 1 | 99 | 0x0 |
| `dma-cont` | `0x8` | read | 1 | 1 | 23905938 | 0x0 |
| `dma-cont` | `0xd0` | read | 1 | 1 | 23905939 | 0x0 |
| `dma-cont` | `0xc` | write | 1 | 1 | 23905940 | 0xf |
| `dma-cont` | `0xd8` | write | 1 | 1 | 23905941 | 0xf |
| `dma-page` | `0x87` | read | 1 | 1 | 23905942 | 0x0 |
| `dma-chan` | `0x0` | read | 1 | 2 | 23905943 | 0x0 |
| `dma-chan` | `0x1` | read | 1 | 2 | 23905945 | 0x0 |
| `dma-page` | `0x83` | read | 1 | 1 | 23905947 | 0x0 |
| `dma-chan` | `0x2` | read | 1 | 2 | 23905948 | 0x0 |
| `dma-chan` | `0x3` | read | 1 | 2 | 23905950 | 0x0 |
| `dma-page` | `0x81` | read | 1 | 1 | 23905952 | 0x0 |
| `dma-chan` | `0x4` | read | 1 | 2 | 23905953 | 0x0 |
| `dma-chan` | `0x5` | read | 1 | 2 | 23905955 | 0x0 |
| `dma-page` | `0x82` | read | 1 | 1 | 23905957 | 0x0 |
| `dma-chan` | `0x6` | read | 1 | 2 | 23905958 | 0x0 |
| `dma-chan` | `0x7` | read | 1 | 2 | 23905960 | 0x0 |
| `dma-page` | `0x8f` | read | 1 | 1 | 23905962 | 0x0 |
| `dma-chan` | `0xc0` | read | 1 | 2 | 23905963 | 0x0 |
| `dma-chan` | `0xc2` | read | 1 | 2 | 23905965 | 0x0 |
| `dma-page` | `0x8b` | read | 1 | 1 | 23905967 | 0x0 |
| `dma-chan` | `0xc4` | read | 1 | 2 | 23905968 | 0x0 |
| `dma-chan` | `0xc6` | read | 1 | 2 | 23905970 | 0x0 |
| `dma-page` | `0x89` | read | 1 | 1 | 23905972 | 0x0 |
| `dma-chan` | `0xc8` | read | 1 | 2 | 23905973 | 0x0 |
| `dma-chan` | `0xca` | read | 1 | 2 | 23905975 | 0x0 |
| `dma-page` | `0x8a` | read | 1 | 1 | 23905977 | 0x0 |
| `dma-chan` | `0xcc` | read | 1 | 2 | 23905978 | 0x0 |
| `dma-chan` | `0xce` | read | 1 | 2 | 23905980 | 0x0 |
| `fdc` | `0x3f2` | write | 1 | 6 | 25898467 | 0x8, 0xc |
| `fdc_ioport_write` | `0x02` | indexed event | - | 6 | 25898468 | 0x8, 0xc |
| `fdc_ioport_read` | `0x04` | indexed event | - | 27 | 25898469 | 0x80, 0xd0 |
| `fdc` | `0x3f4` | read | 1 | 27 | 25898470 | 0x80, 0xd0 |
| `fdc` | `0x3f5` | write | 1 | 5 | 25898471 | 0x8, 0xe |
| `fdc_ioport_write` | `0x05` | indexed event | - | 5 | 25898472 | 0x8, 0xe |
| `fdc_ioport_read` | `0x05` | indexed event | - | 18 | 25898475 | 0x0-0x1, 0x60, 0xc0-0xc3, 0xff |
| `fdc` | `0x3f5` | read | 1 | 18 | 25898476 | 0x0-0x1, 0x60, 0xc0-0xc3, 0xff |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `fdc_ioport_write` | 11 | 25898468 |
| `fdc_ioport_read` | 45 | 25898469 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
