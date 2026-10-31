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
| `i8042-cmd` | `0x64` | read | 1 | 3392 | 41069 | 0x18, 0x1c-0x1d, 0x3d |
| `i8042-cmd` | `0x64` | write | 1 | 43 | 41072 | 0x20, 0x60, 0xa7-0xa8, 0xaa-0xab, 0xad-0xae, 0xd4 |
| `i8042-data` | `0x60` | read | 1 | 6340 | 41087 | 0x0, 0x3, 0x8-0x9, 0xe, 0x14, 0x1e, 0x28, 0x51, 0x55, 0x8e, 0x9e, 0xaa, 0xf1, 0xfa |
| `i8042-data` | `0x60` | write | 1 | 41 | 41102 | 0x0, 0x2-0x3, 0xa, 0x20, 0x30, 0x50, 0x53, 0x60-0x61, 0x64, 0x70, 0xc8, 0xe6, 0xe8, 0xed, 0xf0, 0xf2-0xf5, 0xff |

## Device events

| Event | Count | First use |
|---|---:|---:|
| `ps2_mouse_reset` | 1 | 6 |
| `ps2_kbd_reset` | 1 | 7 |
| `pckbd_kbd_read_status` | 3392 | 41068 |
| `pckbd_kbd_write_command` | 43 | 41073 |
| `pckbd_kbd_read_data` | 6340 | 41086 |
| `pckbd_kbd_write_data` | 41 | 41103 |
| `ps2_keyboard_set_translation` | 13 | 41104 |
| `ps2_write_keyboard` | 11 | 41151 |
| `ps2_reset_keyboard` | 2 | 41152 |
| `ps2_set_ledstate` | 3 | 41153 |
| `ps2_read_data` | 46 | 41156 |
| `ps2_write_mouse` | 17 | 420509 |
| `ps2_mouse_send_packet` | 3 | 51963968 |
| `ps2_keyboard_event` | 4 | 51987040 |
| `ps2_put_keycode` | 6 | 51987041 |

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
