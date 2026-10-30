use super::*;
use crate::virtio::testing::{RecIrq, RecMsi};
use vm_memory::{Bytes, GuestAddress};

struct Driver {
    dev: VirtioVga,
    mem: GuestMemory,
    avail: [u16; 2],
    msi: Arc<RecMsi>,
    irq: Arc<RecIrq>,
}

impl Driver {
    fn new() -> Self {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 2 * 1024 * 1024)]).unwrap();
        let msi = Arc::new(RecMsi::default());
        let irq = Arc::new(RecIrq::default());
        let dev = VirtioVga::new(
            "vga",
            &[0x55, 0xaa, 1],
            mem.clone(),
            Some(msi.clone()),
            Some(irq.clone()),
        )
        .unwrap();
        Self {
            dev,
            mem,
            avail: [0, 0],
            msi,
            irq,
        }
    }
    fn write(&mut self, off: u64, bytes: &[u8]) {
        self.dev.bar_write(2, 0x1000 + off, bytes);
    }
    fn start(&mut self) {
        self.start_queues(2);
    }
    fn start_queues(&mut self, count: u16) {
        self.write(0x14, &[3]);
        self.write(0x08, &1u32.to_le_bytes());
        self.write(0x0c, &1u32.to_le_bytes());
        self.write(0x08, &0u32.to_le_bytes());
        self.write(0x0c, &2u32.to_le_bytes());
        self.write(0x14, &[11]);
        for q in 0..count {
            self.write(0x16, &q.to_le_bytes());
            self.write(0x18, &16u16.to_le_bytes());
            let base = 0x1000 + u64::from(q) * 0x4000;
            self.write(0x20, &base.to_le_bytes());
            self.write(0x28, &(base + 0x1000).to_le_bytes());
            self.write(0x30, &(base + 0x2000).to_le_bytes());
            self.write(0x1c, &1u16.to_le_bytes());
        }
        self.write(0x14, &[15]);
        let mut status = [0];
        self.dev.bar_read(2, 0x1014, &mut status);
        assert_eq!(status, [15]);
    }
    fn submit_raw(&mut self, q: usize, request: &[u8], response_size: u32) -> Vec<u8> {
        let base = 0x1000 + q as u64 * 0x4000;
        let req = 0x10000;
        let resp: u64 = 0x60000;
        self.mem.write_slice(request, GuestAddress(req)).unwrap();
        let mut desc = Vec::new();
        desc.extend_from_slice(&req.to_le_bytes());
        desc.extend_from_slice(&(request.len() as u32).to_le_bytes());
        desc.extend_from_slice(&(if response_size == 0 { 0u16 } else { 1 }).to_le_bytes());
        desc.extend_from_slice(&1u16.to_le_bytes());
        desc.extend_from_slice(&resp.to_le_bytes());
        desc.extend_from_slice(&response_size.to_le_bytes());
        desc.extend_from_slice(&2u16.to_le_bytes());
        desc.extend_from_slice(&0u16.to_le_bytes());
        self.mem.write_slice(&desc, GuestAddress(base)).unwrap();
        self.mem
            .write_obj(
                0u16,
                GuestAddress(base + 0x1004 + u64::from(self.avail[q] % 16) * 2),
            )
            .unwrap();
        self.avail[q] = self.avail[q].wrapping_add(1);
        self.mem
            .write_obj(self.avail[q], GuestAddress(base + 0x1002))
            .unwrap();
        self.dev
            .bar_write(2, 0x3000 + q as u64 * 4, &(q as u16).to_le_bytes());
        assert_eq!(
            self.mem
                .read_obj::<u16>(GuestAddress(base + 0x2002))
                .unwrap(),
            self.avail[q]
        );
        let len: u32 = self
            .mem
            .read_obj(GuestAddress(
                base + 0x2008 + u64::from((self.avail[q] - 1) % 16) * 8,
            ))
            .unwrap();
        assert!(len <= response_size);
        let mut out = vec![0; len as usize];
        self.mem.read_slice(&mut out, GuestAddress(resp)).unwrap();
        out
    }
    fn command(&mut self, q: usize, kind: u32, words: &[u32]) -> Vec<u8> {
        let mut request = vec![0; 24];
        request[..4].copy_from_slice(&kind.to_le_bytes());
        for word in words {
            request.extend_from_slice(&word.to_le_bytes());
        }
        self.submit_raw(q, &request, 2048)
    }
    fn ok(&mut self, kind: u32, words: &[u32]) {
        assert_eq!(code(&self.command(0, kind, words)), 0x1100);
    }
    fn create(&mut self, id: u32, w: u32, h: u32, format: u32) {
        self.ok(0x101, &[id, format, w, h]);
    }
    fn backing(&mut self, id: u32, bytes: &[u8]) {
        self.mem.write_slice(bytes, GuestAddress(0x100000)).unwrap();
        // The split deliberately falls inside a pixel.
        self.ok(
            0x106,
            &[
                id,
                2,
                0x100000,
                0,
                3,
                0,
                0x100003,
                0,
                bytes.len() as u32 - 3,
                0,
            ],
        );
    }
    fn transfer(&mut self, id: u32, w: u32, h: u32) {
        self.ok(0x105, &[0, 0, w, h, 0, 0, id, 0]);
    }
    fn vbe(&mut self, index: u16, value: u16) {
        self.dev.io_write(0x1ce, &index.to_le_bytes());
        self.dev.io_write(0x1cf, &value.to_le_bytes());
    }
}

fn code(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

#[test]
fn firmware_latency_timer_write_is_ignored_as_in_the_oracle() {
    let mut d = Driver::new();
    d.dev.config_write(3, 1, &[0x40]);
    assert_eq!(d.dev.config_read(3), 0);
    d.dev.config_write(3, 0, &u32::MAX.to_le_bytes());
    assert_eq!(d.dev.config_read(3), 0xff);
}

#[test]
fn captured_pci_identity_capabilities_and_bar_probes() {
    let mut d = Driver::new();
    let capture =
        include_str!("../../../../../docs/hwref/q35-ovmf-win11/guest/lspci-nnnvvvxxxx.txt");
    let function = capture
        .split("00:04.0 Class [0300]")
        .nth(1)
        .unwrap()
        .split("\n\n")
        .next()
        .unwrap();
    let mut expected = [0u8; 256];
    for line in function.lines() {
        let Some((addr, data)) = line.split_once(": ") else {
            continue;
        };
        let Ok(addr) = usize::from_str_radix(addr, 16) else {
            continue;
        };
        for (i, byte) in data.split_whitespace().enumerate() {
            expected[addr + i] = u8::from_str_radix(byte, 16).unwrap();
        }
    }
    // Firmware allocation, command and interrupt line are dynamic.
    for range in [4..6, 0x10..0x28, 0x30..0x34, 0x3c..0x3d] {
        expected[range].fill(0);
    }
    expected[0x10] = 8;
    expected[0x18] = 12;
    let mut actual = [0; 256];
    for reg in 0..64 {
        actual[reg * 4..reg * 4 + 4].copy_from_slice(&d.dev.config_read(reg).to_le_bytes());
    }
    assert_eq!(actual, expected);
    for (reg, mask) in [
        (4, 0xff800008),
        (6, 0xffffc00c),
        (7, 0xffffffff),
        (8, 0xfffff000),
        (12, 0xfffe0001),
    ] {
        d.dev.config_write(reg, 0, &u32::MAX.to_le_bytes());
        assert_eq!(d.dev.config_read(reg), mask);
    }
    assert_eq!(
        d.dev
            .bars()
            .iter()
            .map(|b| (b.index, b.size))
            .collect::<Vec<_>>(),
        [(0, 8 << 20), (2, 16384), (4, 4096), (6, 131072)]
    );
}

#[test]
fn vga_vbe_framebuffer_legacy_window_and_rom() {
    let mut d = Driver::new();
    d.vbe(1, 2);
    d.vbe(2, 1);
    d.vbe(3, 32);
    d.vbe(4, 0x41);
    d.dev.bar_write(0, 0, &[1, 2, 3, 0, 4, 5, 6, 0]);
    let frame = d.dev.frame();
    assert_eq!((frame.mode.width, frame.mode.height), (2, 1));
    assert_eq!(frame.pixels, [1, 2, 3, 0, 4, 5, 6, 0]);
    assert!(frame.dirty);
    assert!(!d.dev.frame().dirty);
    d.dev.legacy_write(4, &[7, 8, 9, 0]);
    assert_eq!(&d.dev.frame().pixels[4..], &[7, 8, 9, 0]);
    let mut rom = [0; 3];
    d.dev.bar_read(ROM_BAR, 0, &mut rom);
    assert_eq!(rom, [0x55, 0xaa, 1]);
    d.dev.io_write(0x1ce, &10u16.to_le_bytes());
    let mut size = [0; 2];
    d.dev.io_read(0x1cf, &mut size);
    assert_eq!(u16::from_le_bytes(size), 128);
    d.dev.bar_read(2, 0x514, &mut size);
    assert_eq!(u16::from_le_bytes(size), 128);
}

#[test]
fn queue_driven_scanout_fences_damage_snapshot_and_reset() {
    let mut d = Driver::new();
    d.start();
    let info = d.command(0, 0x100, &[]);
    assert_eq!(code(&info), 0x1101);
    assert_eq!(code(&info[32..]), 1024);
    d.create(1, 3, 2, 2);
    let pixels: Vec<u8> = (0..24).collect();
    d.backing(1, &pixels);
    d.transfer(1, 3, 2);
    d.ok(0x103, &[1, 0, 2, 2, 0, 1]);
    let frame = d.dev.frame();
    assert_eq!((frame.mode.width, frame.mode.height), (2, 2));
    assert_eq!(
        frame.pixels,
        [4, 5, 6, 0, 8, 9, 10, 0, 16, 17, 18, 0, 20, 21, 22, 0]
    );
    assert!(frame.dirty);
    assert!(!d.dev.frame().dirty);
    d.ok(0x104, &[0, 0, 1, 1, 1, 0]);
    assert!(!d.dev.frame().dirty);
    let mut request = vec![0; 48];
    request[..4].copy_from_slice(&0x104u32.to_le_bytes());
    request[4] = 1;
    request[8..16].copy_from_slice(&123456789u64.to_le_bytes());
    for (i, word) in [1u32, 0, 2, 2, 1, 0].iter().enumerate() {
        request[24 + i * 4..28 + i * 4].copy_from_slice(&word.to_le_bytes());
    }
    let response = d.submit_raw(0, &request, 24);
    assert_eq!(response[4], 1);
    assert_eq!(&response[8..16], &request[8..16]);
    assert!(d.dev.frame().dirty);
    assert!(*d.irq.level.lock());
    let state = d.dev.save_state().unwrap();
    d.write(0x14, &[0]);
    assert_eq!(d.dev.mode().width, 720);
    assert!(!d.dev.cursor().visible);
    d.dev.restore_state(&state).unwrap();
    assert_eq!(d.dev.frame().pixels, frame.pixels);
    // Queue cursors survive restore; old commands are not replayed.
    d.ok(0x107, &[1, 0]);
    assert_eq!(
        code(&d.command(0, 0x105, &[0, 0, 3, 2, 0, 0, 1, 0])),
        0x1205
    );
    d.ok(0x102, &[1, 0]);
    assert!(d.dev.frame().pixels.iter().all(|p| *p == 0));
    d.write(0x14, &[0]);
    assert_eq!(d.dev.mode().width, 720);
    assert!(d.dev.frame().dirty);
}

#[test]
fn preferred_size_edid_events_and_cursor_queue() {
    let mut d = Driver::new();
    d.start();
    d.dev.request_size(1920, 1080).unwrap();
    assert!(d.dev.request_size(0, 1080).is_err());
    let mut cfg = [0; 16];
    d.dev.bar_read(2, 0x2000, &mut cfg);
    assert_eq!(code(&cfg), 1);
    assert_eq!(code(&cfg[8..]), 1);
    let info = d.command(0, 0x100, &[]);
    assert_eq!(code(&info[32..]), 1920);
    let e = d.command(0, 0x10a, &[0, 0]);
    assert_eq!(code(&e), 0x1104);
    assert_eq!(code(&e[24..]), 128);
    assert_eq!(e[32..160].iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    d.dev.bar_write(2, 0x2004, &1u32.to_le_bytes());
    d.dev.bar_read(2, 0x2000, &mut cfg);
    assert_eq!(code(&cfg), 0);
    d.create(9, 64, 64, 1);
    let mut pixels = vec![0; 16384];
    pixels[..4].copy_from_slice(&[100, 50, 200, 128]);
    d.backing(9, &pixels);
    d.transfer(9, 64, 64);
    assert_eq!(
        code(&d.command(1, 0x300, &[0, 30, 40, 0, 9, 2, 3, 0])),
        0x1100
    );
    let c = d.dev.cursor();
    assert!(c.visible);
    assert_eq!((c.x, c.y, c.hot_x, c.hot_y), (30, 40, 2, 3));
    assert_eq!(&c.pixels[..4], &[50, 25, 100, 128]);
    let mut request = vec![0; 56];
    request[..4].copy_from_slice(&0x301u32.to_le_bytes());
    request[28..32].copy_from_slice(&80u32.to_le_bytes());
    assert!(d.submit_raw(1, &request, 0).is_empty());
    assert_eq!(d.dev.cursor().x, 80);
    assert_eq!(d.dev.cursor().pixels, c.pixels);
    let state = d.dev.save_state().unwrap();
    d.write(0x14, &[0]);
    d.dev.restore_state(&state).unwrap();
    assert!(d.dev.cursor().visible);
    d.command(1, 0x300, &[0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(!d.dev.cursor().visible);
}

#[test]
fn malformed_requests_return_errors_and_msix_routes_queue_interrupts() {
    let mut d = Driver::new();
    d.start();
    assert_eq!(code(&d.command(0, 0x101, &[0, 2, 1, 1])), 0x1205);
    assert_eq!(code(&d.command(0, 0x101, &[1, 999, 1, 1])), 0x1205);
    assert_eq!(code(&d.command(0, 0x102, &[999, 0])), 0x1203);
    assert_eq!(code(&d.command(0, 0x103, &[0, 0, 1, 1, 1, 0])), 0x1202);
    assert_eq!(code(&d.command(0, 0x101, &[])), 0x1205);
    d.create(1, 2, 2, 2);
    assert_eq!(
        code(&d.command(0, 0x103, &[u32::MAX, 0, 2, 2, 0, 1])),
        0x1205
    );
    assert_eq!(
        code(&d.command(0, 0x106, &[1, 1, u32::MAX, u32::MAX, 4, 0])),
        0x1205
    );
    d.dev.bar_write(4, 0, &0xfee00000u64.to_le_bytes());
    d.dev.bar_write(4, 8, &0x51u32.to_le_bytes());
    d.dev.bar_write(4, 12, &0u32.to_le_bytes());
    d.dev.config_write(0x98 / 4, 2, &0x8002u16.to_le_bytes());
    d.write(0x16, &0u16.to_le_bytes());
    d.write(0x1a, &0u16.to_le_bytes());
    d.command(0, 0x100, &[]);
    assert_eq!(*d.msi.messages.lock(), [(0xfee00000, 0x51)]);
    assert_eq!(d.dev.config_read(0x98 / 4) >> 16, 0x8002);
    d.dev
        .config_write(0x98 / 4, 0, &0x00028411u32.to_le_bytes());
    assert_eq!(d.dev.config_read(0x98 / 4) >> 16, 2);
}

#[test]
fn firmware_can_use_only_control_queue() {
    let mut d = Driver::new();
    d.start_queues(1);
    assert_eq!(code(&d.command(0, 0x100, &[])), 0x1101);
    let state = d.dev.save_state().unwrap();
    d.write(0x14, &[0]);
    d.dev.restore_state(&state).unwrap();
    assert_eq!(code(&d.command(0, 0x100, &[])), 0x1101);
}

#[test]
fn vga_text_planar_and_snapshot_match_standard_vga() {
    let mut d = Driver::new();
    let mut standard = StandardVga::new("reference", &[0x55, 0xaa, 1]).unwrap();
    for (port, bytes) in [(0x3c2, vec![0x67]), (0x3c4, vec![1, 1])] {
        d.dev.io_write(port, &bytes);
        standard.io_write(port, &bytes);
    }
    d.dev.legacy_write(0x18000, &[b'A', 0x1f, b' ', 0x2e]);
    standard.legacy_write(0x18000, &[b'A', 0x1f, b' ', 0x2e]);
    let frame = d.dev.frame();
    assert_eq!((frame.mode.width, frame.mode.height), (640, 400));
    assert_eq!(frame.pixels, standard.frame().pixels);
    let state = d.dev.save_state().unwrap();
    d.dev.legacy_write(0x18000, &[b'B', 0]);
    d.dev.restore_state(&state).unwrap();
    assert_eq!(d.dev.frame().pixels, frame.pixels);
    for (port, index, value) in [(0x3c4, 2, 15), (0x3c4, 4, 6), (0x3ce, 6, 5), (0x3ce, 5, 2)] {
        d.dev.io_write(port, &[index, value]);
        standard.io_write(port, &[index, value]);
    }
    d.dev.legacy_write(0, &[4]);
    standard.legacy_write(0, &[4]);
    assert_eq!(d.dev.frame().pixels, standard.frame().pixels);
}

#[test]
fn pci_configuration_access_capability_and_seavgabios_rom() {
    let mut d = Driver::new();
    d.dev.config_write(0x88 / 4, 0, &[2]);
    d.dev.config_write(0x8c / 4, 0, &0x1012u32.to_le_bytes());
    d.dev.config_write(0x90 / 4, 0, &2u32.to_le_bytes());
    assert_eq!(d.dev.config_read(0x94 / 4), 2);
    if let Ok(rom) = std::fs::read("/usr/share/qemu/vgabios-virtio.bin") {
        let mut dev = VirtioVga::new("rom", &rom, d.mem, None, None).unwrap();
        let mut bytes = vec![0; rom.len()];
        dev.bar_read(ROM_BAR, 0, &mut bytes);
        assert_eq!(bytes, rom);
    } else {
        eprintln!("SKIP host SeaVGABIOS ROM: /usr/share/qemu/vgabios-virtio.bin missing");
    }
}
