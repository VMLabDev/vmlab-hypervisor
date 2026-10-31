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
| `i8042-cmd` | `0x64` | read | 1 | 1070 | 41255 | 0x18, 0x1c-0x1d, 0x3d |
| `i8042-cmd` | `0x64` | write | 1 | 193 | 41258 | 0x20, 0x60, 0xa7-0xa8, 0xaa-0xab, 0xad-0xae, 0xd1, 0xd4, 0xfe-0xff |
| `i8042-data` | `0x60` | read | 1 | 266 | 41273 | 0x0-0x1, 0x3-0x4, 0xc, 0x11-0x14, 0x16, 0x1c-0x1d, 0x1f-0x21, 0x23, 0x27, 0x2a-0x2b, 0x2d-0x2e, 0x30, 0x32, 0x34-0x35, 0x39, 0x55, 0x5b, 0x64, 0x70-0x71, 0x73, 0x81, 0x8c, 0x91-0x94, 0x96, 0x9c-0x9d, 0x9f-0xa1, 0xa3, 0xa7, 0xaa-0xab, 0xad-0xae, 0xb2, 0xb4-0xb5, 0xb9, 0xdb, 0xe0, 0xfa |
| `i8042-data` | `0x60` | write | 1 | 132 | 41288 | 0x0, 0x2-0x3, 0x8, 0x20, 0x30, 0x40, 0x43, 0x50, 0x60-0x61, 0x64, 0x70, 0xc8, 0xdf, 0xe6, 0xe8-0xe9, 0xed, 0xf0, 0xf2-0xf5, 0xff |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `ps2_mouse_reset` | 3 | 7 |
| `ps2_kbd_reset` | 3 | 8 |
| `pckbd_kbd_read_status` | 1070 | 41254 |
| `pckbd_kbd_write_command` | 193 | 41259 |
| `pckbd_kbd_read_data` | 266 | 41272 |
| `pckbd_kbd_write_data` | 132 | 41289 |
| `ps2_keyboard_set_translation` | 32 | 41290 |
| `ps2_write_keyboard` | 36 | 41337 |
| `ps2_reset_keyboard` | 6 | 41338 |
| `ps2_set_ledstate` | 10 | 41339 |
| `ps2_read_data` | 246 | 41342 |
| `pckbd_outport_write` | 4 | 63472 |
| `ps2_write_mouse` | 60 | 41624514 |
| `ps2_keyboard_event` | 124 | 78500465 |
| `ps2_put_keycode` | 188 | 78500466 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
