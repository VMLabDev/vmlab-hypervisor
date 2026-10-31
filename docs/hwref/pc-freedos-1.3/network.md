# network observations

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
| `pcnet-io` | `0xc02e` | read | 1 | 2 | 1561441 | 0x57 |
| `pcnet-io` | `0xc02f` | read | 1 | 2 | 1561444 | 0x57 |
| `pcnet-io` | `0xc020` | read | 1 | 5 | 1561447 | 0x52 |
| `pcnet-io` | `0xc021` | read | 1 | 5 | 1561450 | 0x54 |
| `pcnet-io` | `0xc022` | read | 1 | 5 | 1561453 | 0x0 |
| `pcnet-io` | `0xc023` | read | 1 | 5 | 1561456 | 0x9 |
| `pcnet-io` | `0xc024` | read | 1 | 5 | 1561459 | 0x78 |
| `pcnet-io` | `0xc025` | read | 1 | 5 | 1561462 | 0xdf |
| `pcnet-io` | `0xc026` | read | 1 | 1 | 1561465 | 0x0 |
| `pcnet-io` | `0xc027` | read | 1 | 1 | 1561468 | 0x0 |
| `pcnet-io` | `0xc028` | read | 1 | 1 | 1561471 | 0x0 |
| `pcnet-io` | `0xc029` | read | 1 | 2 | 1561474 | 0x11 |
| `pcnet-io` | `0xc02a` | read | 1 | 1 | 1561477 | 0x0 |
| `pcnet-io` | `0xc02b` | read | 1 | 1 | 1561480 | 0x0 |
| `pcnet-io` | `0xc02c` | read | 1 | 1 | 1561483 | 0xc5 |
| `pcnet-io` | `0xc02d` | read | 1 | 1 | 1561486 | 0x2 |
| `pcnet-io` | `0xc032` | write | 2 | 59 | 1561496 | 0x0-0x7 |
| `pcnet-io` | `0xc030` | write | 2 | 31 | 1561498 | 0x0-0x2, 0x4, 0x40, 0x42, 0x48, 0x1b0, 0x2b0, 0x6b0, 0x44b4 |
| `pcnet-io` | `0xc034` | read | 2 | 1 | 1561502 | 0x0 |
| `pcnet-io` | `0xc030` | read | 2 | 33 | 1561506 | 0x4, 0x73, 0x7b, 0x181, 0x1f3, 0x2fb, 0x6fb |
| `pcnet-io` | `0xc036` | read | 2 | 1 | 1561543 | 0x2 |
| `pcnet-io` | `0xc036` | write | 2 | 5 | 1561544 | 0x2, 0x81, 0xb0, 0xc0, 0x4088 |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `pcnet_s_reset` | 2 | 9 |
| `pcnet_ioport_read` | 78 | 1561439 |
| `pcnet_aprom_readb` | 43 | 1561440 |
| `pcnet_ioport_write` | 95 | 1561497 |
| `pcnet_init` | 1 | 1561578 |
| `pcnet_rlen_tlen` | 1 | 1561579 |
| `pcnet_ss32_rdra_tdra` | 1 | 1561580 |
| `pcnet_isr_change` | 16 | 1561603 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
