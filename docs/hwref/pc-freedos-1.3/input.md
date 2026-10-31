# input observations

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
| `i8042-cmd` | `0x64` | read | 1 | 116 | 41124 | 0x18, 0x1c-0x1d, 0x3d |
| `i8042-cmd` | `0x64` | write | 1 | 46 | 41127 | 0x60, 0xa7, 0xaa-0xab, 0xad, 0xd4 |
| `i8042-data` | `0x60` | read | 1 | 21 | 41142 | 0x0, 0x55, 0xaa, 0xfa |
| `i8042-data` | `0x60` | write | 1 | 47 | 41157 | 0x2-0x3, 0x20, 0x30, 0x43, 0x50, 0x60-0x61, 0x64, 0x70, 0xe8, 0xf0, 0xf2-0xf5, 0xff |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `ps2_mouse_reset` | 1 | 7 |
| `ps2_kbd_reset` | 1 | 8 |
| `pckbd_kbd_read_status` | 116 | 41123 |
| `pckbd_kbd_write_command` | 46 | 41128 |
| `pckbd_kbd_read_data` | 21 | 41141 |
| `pckbd_kbd_write_data` | 47 | 41158 |
| `ps2_keyboard_set_translation` | 33 | 41159 |
| `ps2_write_keyboard` | 5 | 41206 |
| `ps2_reset_keyboard` | 2 | 41207 |
| `ps2_set_ledstate` | 2 | 41208 |
| `ps2_read_data` | 19 | 41211 |
| `ps2_write_mouse` | 9 | 325866 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
