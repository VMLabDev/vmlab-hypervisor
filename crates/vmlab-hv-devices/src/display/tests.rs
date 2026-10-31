use super::*;
use vm_memory::GuestMemoryBackend;

fn device() -> StandardVga {
    StandardVga::new("vga0", &[0x55, 0xaa, 1]).unwrap()
}

#[test]
fn active_black_scanout_is_distinct_from_reset_and_blanking() {
    use crate::Resettable;
    let mut dev = device();
    assert!(!dev.scanout_active());
    let initial = dev.save_state().unwrap();
    indexed(&mut dev, 0x3d4, 1, 79);
    assert!(dev.scanout_active());
    assert!(dev.frame().pixels.iter().all(|byte| *byte == 0));
    let active = dev.save_state().unwrap();
    indexed(&mut dev, 0x3c4, 1, 0x20);
    assert!(!dev.scanout_active());
    dev.restore_state(&active).unwrap();
    assert!(dev.scanout_active());
    dev.reset().unwrap();
    assert!(!dev.scanout_active());
    dev.restore_state(&initial).unwrap();
    assert!(!dev.scanout_active());
    vbe(&mut dev, 640, 480, 32);
    assert!(dev.scanout_active());
    dev.mark_framebuffer_dirty();
    dispi(&mut dev, 4, 0);
    assert!(!dev.scanout_active());
}

#[test]
fn standard_vga_mmio_edid_matches_the_captured_default_monitor() {
    let mut dev = device();
    let mut bytes = [0xff; 1024];
    dev.bar_read(2, 0, &mut bytes);
    assert_eq!(
        &bytes[..256],
        include_bytes!("../../../../docs/hwref/ubuntu-26.04-display/guest/drm/Virtual-1.edid")
    );
    assert_eq!(&bytes[256..], &[0; 768]);
    dev.bar_write(2, 0, &[0; 256]);
    let mut tail = [0xff; 8];
    dev.bar_read(2, 252, &mut tail);
    assert_eq!(&tail[..4], &bytes[252..256]);
    assert_eq!(&tail[4..], &[0; 4]);
}

#[test]
fn fragmented_damage_fits_the_display_protocol_rectangle_limit() {
    let mode = Mode {
        width: 1024,
        height: 1024,
        stride: 4096,
        format: PixelFormat::Xrgb8888,
    };
    let previous = Frame::compare(mode, vec![0; 4096 * 1024], None, (8, 1));
    let mut pixels = previous.pixels.clone();
    for y in 0..1024 {
        for x in (0..1024).step_by(16) {
            pixels[y * 4096 + x * 4] = 1;
        }
    }
    let frame = Frame::compare(mode, pixels, Some(&previous), (8, 1));
    assert!(frame.damage.len() <= u16::MAX as usize);
    assert!(frame.dirty);
}

fn indexed(dev: &mut StandardVga, port: u16, index: u8, value: u8) {
    dev.io_write(port, &[index, value]);
}

fn dispi(dev: &mut StandardVga, index: u16, value: u16) {
    dev.io_write(0x1ce, &index.to_le_bytes());
    dev.io_write(0x1cf, &value.to_le_bytes());
}

fn read_dispi(dev: &mut StandardVga, index: u16) -> u16 {
    dev.io_write(0x1ce, &index.to_le_bytes());
    let mut bytes = [0; 2];
    dev.io_read(0x1cf, &mut bytes);
    u16::from_le_bytes(bytes)
}

fn vbe(dev: &mut StandardVga, width: u16, height: u16, bpp: u16) {
    dispi(dev, 4, 0);
    dispi(dev, 1, width);
    dispi(dev, 2, height);
    dispi(dev, 3, bpp);
    dispi(dev, 4, 0x41);
}

fn pixel(frame: &Frame, x: usize, y: usize) -> [u8; 4] {
    frame.pixels[y * frame.mode.stride as usize + x * 4..][..4]
        .try_into()
        .unwrap()
}

fn text_mode(dev: &mut StandardVga) {
    // VGA mode 3 programming, including the odd/even B8000 aperture.
    dev.io_write(0x3c2, &[0x67]);
    indexed(dev, 0x3c4, 1, 1); // eight-dot cells for the included font
    indexed(dev, 0x3c4, 2, 3);
    indexed(dev, 0x3c4, 4, 2);
    indexed(dev, 0x3ce, 5, 0x10);
    indexed(dev, 0x3ce, 6, 0x0e);
    indexed(dev, 0x3ce, 8, 0xff);
    indexed(dev, 0x3d4, 0x11, 0);
    indexed(dev, 0x3d4, 1, 79);
    indexed(dev, 0x3d4, 7, 2);
    indexed(dev, 0x3d4, 9, 15);
    indexed(dev, 0x3d4, 0x12, 0x8f);
    indexed(dev, 0x3d4, 0x13, 40);
    indexed(dev, 0x3d4, 0x0a, 0x20);
}

#[test]
fn unchanged_vram_and_register_writes_do_not_damage_scanout() {
    let mut dev = device();
    text_mode(&mut dev);
    dev.legacy_write(0x18000, &[b'A', 0x1f]);
    dev.frame();
    dev.legacy_write(0x18000, &[b'A', 0x1f]);
    indexed(&mut dev, 0x3d4, 0x0a, 0x20);
    dev.mark_framebuffer_dirty();
    assert!(!dev.frame().dirty);
}

#[test]
fn text_character_attribute_and_cursor_damage_stays_in_changed_cells() {
    let mut dev = device();
    text_mode(&mut dev);
    dev.legacy_write(0x18000, &[b'A', 0x1f, b'B', 0x1f]);
    dev.frame();
    dev.legacy_write(0x18002, &[b'C', 0x1f]);
    let frame = dev.frame();
    assert_eq!(
        frame.damage,
        vec![Damage {
            x: 8,
            y: 0,
            width: 8,
            height: 16
        }]
    );
    dev.legacy_write(0x18003, &[0x2f]);
    assert_eq!(dev.frame().damage, frame.damage);
    indexed(&mut dev, 0x3d4, 0x0a, 14);
    indexed(&mut dev, 0x3d4, 0x0b, 15);
    assert_eq!(
        dev.frame().damage,
        vec![Damage {
            x: 0,
            y: 0,
            width: 8,
            height: 16
        }]
    );
    indexed(&mut dev, 0x3d4, 0x0f, 1);
    assert_eq!(
        dev.frame().damage,
        vec![Damage {
            x: 0,
            y: 0,
            width: 16,
            height: 16
        }]
    );
    indexed(&mut dev, 0x3d4, 0x0a, 0x20);
    assert_eq!(dev.frame().damage, frame.damage);
    assert!(dev.frame().damage.is_empty());
}

#[test]
fn graphics_damage_tracks_visible_tiles_and_ignores_hidden_vram() {
    let mut dev = device();
    vbe(&mut dev, 35, 19, 32);
    dev.frame();
    dev.bar_write(0, (18 * 35 + 34) * 4, &[1, 2, 3, 0]);
    assert_eq!(
        dev.frame().damage,
        vec![Damage {
            x: 32,
            y: 16,
            width: 3,
            height: 3
        }]
    );
    dev.bar_write(0, 100000, &[1, 2, 3, 0]);
    assert!(!dev.frame().dirty);
    for _ in 0..100 {
        dev.mark_framebuffer_dirty();
        assert!(!dev.frame().dirty);
    }
}

#[test]
fn rendering_uses_a_detached_vram_copy_and_returns_to_live_reads() {
    let mut dev = device();
    dev.set_byte(100, 0x31);
    dev.render_vram.resize(0x40000, 0);
    dev.vram
        .read_slice(&mut dev.render_vram, GuestAddress(0))
        .unwrap();
    dev.set_byte(100, 0x42);
    assert_eq!(dev.byte(100), 0x31);
    dev.render_vram.clear();
    assert_eq!(dev.byte(100), 0x42);
    text_mode(&mut dev);
    dev.frame();
    assert!(dev.render_vram.is_empty());
    dev.set_byte(100, 0x53);
    assert_eq!(dev.byte(100), 0x53);
}

#[test]
fn linear_framebuffer_mapping_shares_bar_pixels_and_snapshot_storage() {
    let mut dev = device();
    vbe(&mut dev, 640, 480, 32);
    dev.frame();
    let memory = dev.linear_framebuffer();
    let host = memory.get_host_address(GuestAddress(0)).unwrap() as usize;
    assert_eq!(host & 4095, 0);
    memory
        .write_slice(&[30, 20, 10, 0], GuestAddress(0))
        .unwrap();
    dev.mark_framebuffer_dirty();
    let frame = dev.frame();
    assert!(frame.dirty);
    assert_eq!(pixel(&frame, 0, 0), [30, 20, 10, 0]);
    assert!(!dev.frame().dirty);
    let saved = dev.save_state().unwrap();
    dev.bar_write(0, 0, &[3, 2, 1, 0]);
    dev.restore_state(&saved).unwrap();
    let mut bytes = [0; 4];
    memory.read_slice(&mut bytes, GuestAddress(0)).unwrap();
    assert_eq!(bytes, [30, 20, 10, 0]);
}

#[test]
fn p4_text_console_pixels_font_upload_scroll_and_cursor() {
    let mut dev = device();
    text_mode(&mut dev);
    dev.legacy_write(0x18000, &[b'A', 0x1f, b' ', 0x2e]);
    let frame = dev.frame();
    assert_eq!(
        frame.mode,
        Mode {
            width: 640,
            height: 400,
            stride: 2560,
            format: PixelFormat::Xrgb8888
        }
    );
    assert!(frame.dirty);
    assert_eq!(pixel(&frame, 2, 1), [255, 255, 255, 0]);
    assert_eq!(pixel(&frame, 0, 0), [170, 0, 0, 0]);
    assert_eq!(pixel(&frame, 8, 0), [0, 170, 0, 0]);
    assert!(!dev.frame().dirty);
    let mut text = [0; 4];
    dev.legacy_read(0x18000, &mut text);
    assert_eq!(text, [b'A', 0x1f, b' ', 0x2e]);

    // vgacon/SeaVGABIOS load a font through plane 2 in planar mode.
    indexed(&mut dev, 0x3c4, 2, 4);
    indexed(&mut dev, 0x3c4, 4, 6);
    indexed(&mut dev, 0x3ce, 5, 0);
    indexed(&mut dev, 0x3ce, 6, 4);
    dev.legacy_write(b'A' as u64 * 32, &[0x80; 16]);
    text_mode(&mut dev);
    let frame = dev.frame();
    assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 0]);
    assert_eq!(pixel(&frame, 1, 0), [170, 0, 0, 0]);

    // Hardware scroll moves the start address by one text row.
    dev.legacy_write(0x18000 + 160, &[b'A', 0x4f]);
    indexed(&mut dev, 0x3d4, 0x0d, 80);
    assert_eq!(pixel(&dev.frame(), 1, 0), [0, 0, 170, 0]);
    indexed(&mut dev, 0x3d4, 0x0f, 80);
    indexed(&mut dev, 0x3d4, 0x0a, 14);
    indexed(&mut dev, 0x3d4, 0x0b, 15);
    assert_eq!(pixel(&dev.frame(), 7, 15), [255, 255, 255, 0]);
}

#[test]
fn p4_bochs_drm_mmio_modeset_linear_scanout_and_pan() {
    let mut dev = device();
    // bochs-drm uses the flat DISPI MMIO registers, not indexed I/O.
    for (index, value) in [(4, 0u16), (1, 800), (2, 600), (3, 32), (4, 0x41)] {
        dev.bar_write(2, 0x500 + index * 2, &value.to_le_bytes());
    }
    dev.bar_write(0, 0, &[0x12, 0x34, 0x56, 0xff]);
    let frame = dev.frame();
    assert_eq!(
        (frame.mode.width, frame.mode.height, frame.mode.stride),
        (800, 600, 3200)
    );
    assert_eq!(pixel(&frame, 0, 0), [0x12, 0x34, 0x56, 0]);
    assert!(!dev.frame().dirty);
    dispi(&mut dev, 6, 1024);
    dispi(&mut dev, 8, 3);
    dispi(&mut dev, 9, 2);
    dev.bar_write(0, (2 * 1024 + 3) * 4, &[1, 2, 3, 0]);
    let frame = dev.frame();
    assert!(frame.dirty);
    assert_eq!(pixel(&frame, 0, 0), [1, 2, 3, 0]);
    assert_eq!(read_dispi(&mut dev, 7), 4096);
}

#[test]
fn p4_seavgabios_dispi_probe_caps_banks_and_noclear() {
    let mut dev = device();
    dispi(&mut dev, 0, 0xb0c0);
    assert_eq!(read_dispi(&mut dev, 0), 0xb0c0);
    dispi(&mut dev, 0, 0xb0c5);
    dispi(&mut dev, 4, 2);
    assert_eq!(read_dispi(&mut dev, 1), 4096);
    assert_eq!(read_dispi(&mut dev, 2), 2160);
    assert_eq!(read_dispi(&mut dev, 3), 32);
    assert_eq!(read_dispi(&mut dev, 10), 256);
    vbe(&mut dev, 640, 480, 8);
    dispi(&mut dev, 5, 1);
    dev.legacy_write(0, &[9, 10, 11]);
    let mut bytes = [0; 3];
    dev.bar_read(0, 65536, &mut bytes);
    assert_eq!(bytes, [9, 10, 11]);
    dispi(&mut dev, 4, 0);
    dispi(&mut dev, 4, 0xc1);
    dev.legacy_read(0, &mut bytes);
    assert_eq!(bytes, [9, 10, 11]);
    dispi(&mut dev, 4, 0);
    dispi(&mut dev, 4, 0x41);
    dev.bar_read(0, 65536, &mut bytes);
    assert_eq!(bytes, [0; 3]);
}

#[test]
fn p4_vbe_pixel_formats_dac_mask_and_endianness() {
    let mut dev = device();
    for (bpp, bytes, expected) in [
        (15, vec![0x00, 0x7c], [0, 0, 255, 0]),
        (16, vec![0xe0, 0x07], [0, 255, 0, 0]),
        (24, vec![2, 3, 4], [2, 3, 4, 0]),
        (32, vec![5, 6, 7, 8], [5, 6, 7, 0]),
    ] {
        vbe(&mut dev, 8, 2, bpp);
        let stride = 8 * bytes.len();
        dev.bar_write(0, stride as u64, &bytes);
        assert_eq!(pixel(&dev.frame(), 0, 1), expected, "bpp {bpp}");
    }
    dev.bar_write(2, 0x604, &0xbebebebeu32.to_le_bytes());
    dev.bar_write(0, 0, &[0, 10, 20, 30]);
    assert_eq!(pixel(&dev.frame(), 0, 0), [30, 20, 10, 0]);
    vbe(&mut dev, 8, 2, 8);
    dev.io_write(0x3c8, &[42]);
    for value in [63, 0, 21] {
        dev.io_write(0x3c9, &[value]);
    }
    dev.bar_write(0, 0, &[42]);
    assert_eq!(pixel(&dev.frame(), 0, 0), [85, 0, 255, 0]);
    dev.io_write(0x3c7, &[42]);
    let mut rgb = [0; 3];
    for byte in &mut rgb {
        dev.io_read(0x3c9, std::slice::from_mut(byte));
    }
    assert_eq!(rgb, [63, 0, 21]);
    dev.io_write(0x3c6, &[0]);
    assert_eq!(pixel(&dev.frame(), 0, 0), [0; 4]);
}

#[test]
fn p4_planar_write_modes_latches_and_read_compare() {
    let mut dev = device();
    indexed(&mut dev, 0x3c4, 2, 15);
    indexed(&mut dev, 0x3c4, 4, 6);
    indexed(&mut dev, 0x3ce, 6, 5);
    indexed(&mut dev, 0x3ce, 5, 2);
    dev.legacy_write(0, &[5]);
    let mut bytes = [0; 4];
    dev.bar_read(0, 0, &mut bytes);
    assert_eq!(bytes, [255, 0, 255, 0]);
    let mut read = [0];
    dev.legacy_read(0, &mut read);
    indexed(&mut dev, 0x3ce, 5, 1);
    dev.legacy_write(1, &[0]);
    dev.bar_read(0, 4, &mut bytes);
    assert_eq!(bytes, [255, 0, 255, 0]);
    indexed(&mut dev, 0x3ce, 5, 8);
    indexed(&mut dev, 0x3ce, 2, 5);
    indexed(&mut dev, 0x3ce, 7, 15);
    dev.legacy_read(1, &mut read);
    assert_eq!(read, [255]);
    indexed(&mut dev, 0x3ce, 2, 6);
    dev.legacy_read(1, &mut read);
    assert_eq!(read, [0]);

    indexed(&mut dev, 0x3ce, 5, 0);
    indexed(&mut dev, 0x3ce, 1, 15);
    indexed(&mut dev, 0x3ce, 0, 2);
    indexed(&mut dev, 0x3ce, 8, 0x0f);
    dev.legacy_write(0, &[0xff]);
    dev.bar_read(0, 0, &mut bytes);
    assert_eq!(bytes, [0xf0, 0x0f, 0xf0, 0]);
    indexed(&mut dev, 0x3ce, 5, 3);
    dev.legacy_write(2, &[0x03]);
    dev.bar_read(0, 8, &mut bytes);
    assert_eq!(bytes, [0xfc, 0x03, 0xfc, 0]);
}

#[test]
fn p4_legacy_planar_and_chain4_graphics_pixels() {
    let mut dev = device();
    indexed(&mut dev, 0x3c4, 1, 1);
    indexed(&mut dev, 0x3c4, 2, 15);
    indexed(&mut dev, 0x3c4, 4, 6);
    indexed(&mut dev, 0x3ce, 6, 5);
    indexed(&mut dev, 0x3ce, 5, 2);
    dev.legacy_write(0, &[4]);
    assert_eq!(pixel(&dev.frame(), 0, 0), [0, 0, 170, 0]);
    // Mode 13h has chain-four CPU addressing and a 320x200 scanout.
    indexed(&mut dev, 0x3c4, 4, 14);
    indexed(&mut dev, 0x3ce, 5, 0x40);
    indexed(&mut dev, 0x3d4, 9, 1);
    dev.legacy_write(0, &[1, 2, 3, 4]);
    let frame = dev.frame();
    assert_eq!((frame.mode.width, frame.mode.height), (320, 200));
    assert_eq!(pixel(&frame, 0, 0), [170, 0, 0, 0]);
    assert_eq!(pixel(&frame, 1, 0), [0, 170, 0, 0]);
}

#[test]
fn p4_pci_identity_bar_probes_rom_and_mmio_registers() {
    let mut dev = device();
    assert_eq!(dev.config_read(0), 0x11111234);
    assert_eq!(dev.config_read(2), 0x03000002);
    assert_eq!(dev.config_read(11), 0x11001af4);
    for (reg, expected) in [(4, 0xff000008), (6, 0xfffff000), (12, 0xfffe0001)] {
        dev.config_write(reg, 0, &[255; 4]);
        assert_eq!(dev.config_read(reg), expected);
    }
    dev.config_write(0, 0, &[0; 4]);
    assert_eq!(dev.config_read(0), 0x11111234);
    dev.config_write(4, 0, &0xfd000000u32.to_le_bytes());
    assert_eq!(dev.config_read(4), 0xfd000008);
    let mut signature = [0; 3];
    dev.bar_read(ROM_BAR, 0, &mut signature);
    assert_eq!(signature, [0x55, 0xaa, 1]);
    dev.bar_write(ROM_BAR, 0, &[0; 3]);
    dev.bar_read(ROM_BAR, 0, &mut signature);
    assert_eq!(signature, [0x55, 0xaa, 1]);
    dev.bar_write(2, 0x404, &[2, 15]);
    dev.io_write(0x3c4, &[2]);
    let mut byte = [0];
    dev.io_read(0x3c5, &mut byte);
    assert_eq!(byte, [15]);
    let mut ext = [0; 4];
    dev.bar_read(2, 0x600, &mut ext);
    assert_eq!(u32::from_le_bytes(ext), 8);
    dev.bar_read(2, 0x604, &mut ext);
    assert_eq!(u32::from_le_bytes(ext), 0x1e1e1e1e);
}

#[test]
fn captured_config_after_firmware_bar_allocation() {
    let mut dev = device();
    dev.config_write(1, 0, &0x0103u16.to_le_bytes());
    dev.config_write(4, 0, &0xfd000008u32.to_le_bytes());
    dev.config_write(6, 0, &0xfebd7000u32.to_le_bytes());
    dev.config_write(12, 0, &0xfebc0000u32.to_le_bytes());
    // First 64 bytes from the captured lspci hex dump. The rest are zero.
    let expected = [
        0x34, 0x12, 0x11, 0x11, 0x03, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
        0x00, 0x08, 0x00, 0x00, 0xfd, 0x00, 0x00, 0x00, 0x00, 0x00, 0x70, 0xbd, 0xfe, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf4,
        0x1a, 0x00, 0x11, 0x00, 0x00, 0xbc, 0xfe, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    for (index, bytes) in expected.as_chunks::<4>().0.iter().enumerate() {
        assert_eq!(dev.config_read(index), u32::from_le_bytes(*bytes));
    }
    for index in 16..64 {
        assert_eq!(dev.config_read(index), 0);
    }
    assert_eq!(
        dev.bars()
            .iter()
            .map(|bar| (bar.index, bar.size))
            .collect::<Vec<_>>(),
        [(0, 16 * 1024 * 1024), (2, 4096), (ROM_BAR, 128 * 1024)]
    );
}

#[test]
fn monochrome_crtc_protection_attribute_flipflop_and_memory_decode() {
    let mut dev = device();
    dev.io_write(0x3c2, &[0]);
    indexed(&mut dev, 0x3b4, 0x11, 0x80);
    indexed(&mut dev, 0x3b4, 1, 0);
    let mut byte = [0];
    dev.io_read(0x3b5, &mut byte);
    assert_eq!(byte, [79]);
    dev.io_read(0x3d5, &mut byte);
    assert_eq!(byte, [255]);
    dev.io_read(0x3ba, &mut byte);
    dev.io_write(0x3c0, &[0x22]);
    dev.io_write(0x3c0, &[42]);
    dev.io_read(0x3c1, &mut byte);
    assert_eq!(byte, [42]);
    dev.io_read(0x3ba, &mut byte);
    dev.io_write(0x3c0, &[0x21]);
    dev.io_read(0x3c1, &mut byte);
    assert_eq!(byte, [1]);
    indexed(&mut dev, 0x3ce, 6, 0x0a);
    dev.legacy_write(0x10000, &[b'Z', 0x0f]);
    let mut cell = [0; 2];
    dev.legacy_read(0x10000, &mut cell);
    assert_eq!(cell, [b'Z', 0x0f]);
    dev.legacy_write(0x18000, &[b'A', 0x10]);
    dev.legacy_read(0x10000, &mut cell);
    assert_eq!(cell, [b'Z', 0x0f]);
    dev.legacy_read(0x18000, &mut cell);
    assert_eq!(cell, [255; 2]);
}

#[test]
fn p4_snapshot_restores_registers_vram_and_damage_atomically() {
    let mut dev = device();
    vbe(&mut dev, 32, 16, 32);
    dev.bar_write(0, 0, &[7, 8, 9, 0]);
    dev.config_write(4, 0, &0xfd000008u32.to_le_bytes());
    let state = dev.save_state().unwrap();
    let mut restored = device();
    restored.restore_state(&state).unwrap();
    assert_eq!(restored.state_id(), "vga0");
    assert_eq!(restored.save_state().unwrap(), state);
    let frame = restored.frame();
    assert!(frame.dirty);
    assert_eq!(pixel(&frame, 0, 0), [7, 8, 9, 0]);
    assert_eq!(restored.config_read(4), 0xfd000008);
    assert!(restored.restore_state(&state[..state.len() - 1]).is_err());
    let mut corrupt = state.clone();
    corrupt[8 + 256 + REG_SIZE + 32 + 10] = 3;
    assert!(restored.restore_state(&corrupt).is_err());
    assert_eq!(restored.save_state().unwrap(), state);
}

#[test]
fn malformed_guest_access_is_bounded() {
    let mut dev = device();
    for index in 0..=255 {
        indexed(&mut dev, 0x3c4, index, 255);
        indexed(&mut dev, 0x3ce, index, 255);
        indexed(&mut dev, 0x3d4, index, 255);
    }
    dispi(&mut dev, 0xffff, 65535);
    assert_eq!(read_dispi(&mut dev, 0xffff), 0);
    for offset in [u64::MAX, 0x1ffff, VRAM_SIZE as u64 - 1] {
        let mut bytes = [0; 8];
        dev.legacy_write(offset, &[255; 8]);
        dev.legacy_read(offset, &mut bytes);
        dev.bar_write(0, offset, &[255; 8]);
        dev.bar_read(0, offset, &mut bytes);
        dev.bar_write(2, offset, &[255; 8]);
        dev.bar_read(2, offset, &mut bytes);
    }
    dev.config_write(usize::MAX, u64::MAX, &[255; 8]);
    assert_eq!(dev.config_read(usize::MAX), 0);
    let mode = dev.frame().mode;
    assert!(mode.width <= 2304 && mode.height <= 1024);
}

#[test]
fn host_seavgabios_rom_if_installed() {
    let path = "/usr/share/qemu/vgabios-stdvga.bin";
    let rom = match std::fs::read(path) {
        Ok(rom) => rom,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP: SeaVGABIOS not installed at {path}");
            return;
        }
        Err(err) => panic!("reading {path}: {err}"),
    };
    let mut dev = StandardVga::new("vga0", &rom).unwrap();
    let mut read = vec![0; rom.len()];
    dev.bar_read(ROM_BAR, 0, &mut read);
    assert_eq!(read, rom);
    assert_eq!(read.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
}
