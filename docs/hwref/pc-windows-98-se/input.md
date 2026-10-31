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
| `i8042-cmd` | `0x64` | read | 1 | 272 | 41123 | 0x18, 0x1c-0x1d, 0x3d |
| `i8042-cmd` | `0x64` | write | 1 | 70 | 41126 | 0x20, 0x60, 0xa7, 0xaa-0xab, 0xad-0xae, 0xd4 |
| `i8042-data` | `0x60` | read | 1 | 134 | 41141 | 0x0-0x1, 0x3, 0x8-0x9, 0xf, 0x1d, 0x2a, 0x31, 0x38, 0x3e, 0x45, 0x47, 0x4d, 0x55, 0x5b, 0x71, 0x81, 0x8f, 0x9d, 0xaa, 0xb1, 0xb6, 0xb8, 0xbe, 0xc5, 0xc7, 0xcd, 0xdb-0xdc, 0xe0-0xe1, 0xfa |
| `i8042-data` | `0x60` | write | 1 | 62 | 41156 | 0x2-0x3, 0x20, 0x28, 0x30, 0x43, 0x50, 0x60-0x61, 0x64, 0x70, 0xc8, 0xe6, 0xe8, 0xf0, 0xf2-0xf5, 0xff |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `ps2_mouse_reset` | 1 | 7 |
| `ps2_kbd_reset` | 1 | 8 |
| `pckbd_kbd_read_status` | 272 | 41122 |
| `pckbd_kbd_write_command` | 70 | 41127 |
| `pckbd_kbd_read_data` | 134 | 41140 |
| `pckbd_kbd_write_data` | 62 | 41157 |
| `ps2_keyboard_set_translation` | 42 | 41158 |
| `ps2_write_keyboard` | 5 | 41205 |
| `ps2_reset_keyboard` | 2 | 41206 |
| `ps2_set_ledstate` | 2 | 41207 |
| `ps2_read_data` | 130 | 41210 |
| `ps2_write_mouse` | 15 | 24848622 |
| `ps2_mouse_send_packet` | 6 | 43999188 |
| `ps2_keyboard_event` | 62 | 46877938 |
| `ps2_put_keycode` | 116 | 46877939 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
