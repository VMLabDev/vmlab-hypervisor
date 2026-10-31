use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use parking_lot::Mutex;
use vmlab_hv_devices::chipset::{
    lpc::{Lpc, PowerAction},
    rtc::Rtc,
};
use vmlab_hv_devices::{BusDevice, IrqLine, PciDevice, Resettable, Snapshot};
use vmlab_hv_vmm::{
    pc::Piix3,
    q35::{Mch, Pam, PirqRouter},
};

const PCI_CAPTURE: &str =
    include_str!("../../../docs/hwref/pc-seabios-legacy/guest/lspci-nnnvvvxxxx.txt");

#[derive(Default)]
struct Line(AtomicBool);

impl IrqLine for Line {
    fn set_level(&self, value: bool) {
        self.0.store(value, Ordering::SeqCst);
    }
}

impl Line {
    fn high(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn pci_byte(device: &mut dyn PciDevice, offset: usize) -> u8 {
    device.config_read(offset / 4).to_le_bytes()[offset % 4]
}

fn pci_write(device: &mut dyn PciDevice, offset: usize, bytes: &[u8]) {
    for (index, &byte) in bytes.iter().enumerate() {
        let offset = offset + index;
        device.config_write(offset / 4, (offset % 4) as u64, &[byte]);
    }
}

fn capture_config(bdf: &str) -> [u8; 256] {
    let function = PCI_CAPTURE
        .split("\n\n")
        .find(|section| section.split_whitespace().next() == Some(bdf))
        .unwrap();
    let mut bytes = [0; 256];
    let mut seen = [false; 16];
    for line in function.lines() {
        let Some((offset, values)) = line.split_once(": ") else {
            continue;
        };
        if offset.len() != 2 {
            continue;
        }
        let Ok(offset) = usize::from_str_radix(offset, 16) else {
            continue;
        };
        assert_eq!(offset % 16, 0);
        let values = values
            .split_whitespace()
            .map(|value| u8::from_str_radix(value, 16).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 16);
        assert!(!seen[offset / 16]);
        bytes[offset..offset + 16].copy_from_slice(&values);
        seen[offset / 16] = true;
    }
    assert!(seen.into_iter().all(|seen| seen));
    bytes
}

fn replay_config(device: &mut dyn PciDevice, bdf: &str) {
    let expected = capture_config(bdf);
    for (reg, bytes) in expected.as_chunks::<4>().0.iter().enumerate() {
        device.config_write(reg, 0, bytes);
    }
    for (offset, expected) in expected.into_iter().enumerate() {
        assert_eq!(
            pci_byte(device, offset),
            expected,
            "{bdf} config byte {offset:#04x}"
        );
    }
}

struct PmFixture {
    device: Lpc,
    irq9: Arc<Line>,
    actions: Arc<Mutex<Vec<PowerAction>>>,
}

impl PmFixture {
    fn new() -> Self {
        let irq9 = Arc::new(Line::default());
        let actions = Arc::new(Mutex::new(Vec::new()));
        let sink = actions.clone();
        let device = Lpc::piix4(
            irq9.clone(),
            Arc::new(move |action| sink.lock().push(action)),
        );
        Self {
            device,
            irq9,
            actions,
        }
    }

    fn enable(&mut self, base: u16) {
        pci_write(&mut self.device, 0x40, &(base | 1).to_le_bytes());
        pci_write(&mut self.device, 0x80, &[1]);
        assert!(self.device.pm_enabled());
        assert_eq!(self.device.pm_base(), base);
    }

    fn read<const N: usize>(&mut self, port: u16) -> [u8; N] {
        let mut bytes = [0; N];
        assert!(
            self.device.read_port(port, &mut bytes),
            "unclaimed read {port:#x}"
        );
        bytes
    }

    fn write(&mut self, port: u16, bytes: &[u8]) {
        assert!(
            self.device.write_port(port, bytes),
            "unclaimed write {port:#x}"
        );
    }
}

#[test]
fn pc_chipset_pci_state_replays_the_recorded_guest_programming() {
    replay_config(&mut Piix3::default(), "00:01.0");
    replay_config(&mut PmFixture::new().device, "00:01.3");
    let pam = Arc::new(Mutex::new(Pam::new(&[0x5a; 0x40000], None, 0).unwrap()));
    replay_config(&mut Mch::i440fx(pam, 256 << 20), "00:00.0");
}

#[test]
fn piix4_identity_and_write_masks_do_not_expose_ich9_registers() {
    let mut f = PmFixture::new();
    assert_eq!(f.device.config_read(0), 0x7113_8086);
    assert_eq!(f.device.config_read(1), 0x0280_0000);
    assert_eq!(f.device.config_read(2), 0x0680_0003);
    assert_eq!(f.device.config_read(0x2c / 4), 0x1100_1af4);
    assert_eq!(f.device.config_read(0x3c / 4), 0x0100);
    assert!(f.device.bars().is_empty());
    pci_write(&mut f.device, 0, &[0xff; 12]);
    pci_write(&mut f.device, 0x2c, &[0xff; 4]);
    assert_eq!(f.device.config_read(0), 0x7113_8086);
    assert_eq!(f.device.config_read(1), 0x0280_0507);
    assert_eq!(f.device.config_read(2), 0x0680_0003);
    assert_eq!(f.device.config_read(0x2c / 4), 0x1100_1af4);
    pci_write(&mut f.device, 0x40, &[0xff; 4]);
    assert_eq!(f.device.config_read(0x40 / 4), u32::MAX);
    pci_write(&mut f.device, 0x44, &[0xff]);
    assert!(!f.device.pm_enabled());
    pci_write(&mut f.device, 0x80, &[0xff]);
    assert!(f.device.pm_enabled());
    assert_eq!(pci_byte(&mut f.device, 0x80), 0xff);
    pci_write(&mut f.device, 0x90, &[0xff; 2]);
    assert_eq!(f.device.smbus_base(), Some(0xfff0));
    pci_write(&mut f.device, 0xd2, &[0]);
    assert_eq!(f.device.smbus_base(), None);
    pci_write(&mut f.device, 0x5b, &[2]);
    assert!(f.device.piix4_smi_enabled());
    pci_write(&mut f.device, 0x5b, &[0]);
    assert!(!f.device.piix4_smi_enabled());
}

#[test]
fn piix4_pm_decode_obeys_enable_and_64_byte_relocation() {
    let mut f = PmFixture::new();
    assert!(!f.device.read_port(0x608, &mut [0; 4]));
    pci_write(&mut f.device, 0x40, &0x601u16.to_le_bytes());
    assert!(!f.device.read_port(0x608, &mut [0; 4]));
    f.enable(0x600);
    f.device.advance(1_000_000_000);
    assert_eq!(u32::from_le_bytes(f.read::<4>(0x608)), 3_579_545);
    pci_write(&mut f.device, 0x40, &0x641u16.to_le_bytes());
    assert_eq!(f.device.pm_base(), 0x640);
    assert!(!f.device.read_port(0x608, &mut [0; 4]));
    assert!(!f.device.write_port(0x604, &[0, 0]));
    assert_eq!(u32::from_le_bytes(f.read::<4>(0x648)), 3_579_545);
    pci_write(&mut f.device, 0x80, &[0]);
    assert!(!f.device.read_port(0x648, &mut [0; 4]));
    f.write(0xb2, &[0xf1]);
    assert_eq!(f.read::<1>(0xb2), [0xf1]);
}

#[test]
fn piix4_apm_sci_power_button_and_write_one_clear() {
    let mut f = PmFixture::new();
    f.enable(0x600);
    f.write(0x602, &0x100u16.to_le_bytes());
    f.device.power_button();
    assert_eq!(u16::from_le_bytes(f.read::<2>(0x600)) & 0x100, 0x100);
    assert!(!f.irq9.high());
    f.write(0xb2, &[0xf1]);
    assert_eq!(u16::from_le_bytes(f.read::<2>(0x604)) & 1, 1);
    assert!(f.irq9.high());
    f.write(0xb2, &[0xf0]);
    assert!(!f.irq9.high());
    assert_eq!(u16::from_le_bytes(f.read::<2>(0x600)) & 0x100, 0x100);
    f.write(0xb2, &[0xf1]);
    assert!(f.irq9.high());
    f.write(0x600, &0x001u16.to_le_bytes());
    assert!(f.irq9.high());
    f.write(0x600, &0x100u16.to_le_bytes());
    assert!(!f.irq9.high());
    f.device.power_button();
    assert!(f.irq9.high());
    f.write(0x602, &[0, 0]);
    assert!(!f.irq9.high());
    assert_eq!(u16::from_le_bytes(f.read::<2>(0x600)) & 0x100, 0x100);
}

#[test]
fn piix4_gpe0_is_at_afe0_and_round_trips_pending_sci() {
    let mut f = PmFixture::new();
    f.enable(0x640);
    f.write(0xb2, &[0xf1]);
    f.device.raise_gpe(3);
    f.device.raise_gpe(12);
    assert_eq!(f.read::<2>(0xafe0), [8, 16]);
    assert!(!f.irq9.high());
    f.write(0xafe2, &[8, 16]);
    assert!(f.irq9.high());
    let state = f.device.save_state().unwrap();
    f.write(0xafe0, &[8, 16]);
    assert!(!f.irq9.high());
    f.device.restore_state(&state).unwrap();
    assert!(f.irq9.high());
    assert_eq!(f.read::<4>(0xafe0), [8, 16, 8, 16]);
    f.write(0xafe0, &[8, 0]);
    assert!(f.irq9.high());
    f.write(0xafe1, &[16]);
    assert!(!f.irq9.high());
    assert_eq!(f.read::<2>(0xafe0), [0, 0]);
    assert_eq!(f.read::<2>(0x660), [0, 0]);
}

#[test]
fn piix4_sleep_types_follow_pc_firmware_at_relocated_pm_base() {
    let mut f = PmFixture::new();
    f.enable(0x640);
    for typ in [1u16, 2, 0, 7] {
        f.write(0x644, &(0x2000 | typ << 10).to_le_bytes());
        assert_eq!(u16::from_le_bytes(f.read::<2>(0x644)) & 0x2000, 0);
    }
    assert_eq!(
        *f.actions.lock(),
        [
            PowerAction::Suspend,
            PowerAction::Hibernate,
            PowerAction::PowerOff
        ]
    );
    f.write(0xcf9, &[2]);
    f.write(0xcf9, &[6]);
    f.write(0xcf9, &[6]);
    assert_eq!(f.actions.lock().last(), Some(&PowerAction::Reset));
    assert_eq!(f.actions.lock().len(), 4);
}

fn cmos(rtc: &mut Rtc, index: u8) -> u8 {
    rtc.write(0, &[index]);
    let mut byte = [0];
    rtc.read(1, &mut byte);
    byte[0]
}

#[test]
fn cmos_boot_fallback_matches_every_gate_capture_with_or_without_cdrom() {
    let captures = [
        (
            "freedos",
            1,
            include_str!("../../../docs/hwref/pc-freedos-1.3/cmos-observed.json"),
        ),
        (
            "windows98",
            1,
            include_str!("../../../docs/hwref/pc-windows-98-se/cmos-observed.json"),
        ),
        (
            "windows7",
            2,
            include_str!("../../../docs/hwref/pc-windows-7/cmos-observed.json"),
        ),
        (
            "templeos",
            2,
            include_str!("../../../docs/hwref/pc-templeos/cmos-observed.json"),
        ),
    ];
    for (name, cpus, json) in captures {
        let capture: serde_json::Value = serde_json::from_str(json).unwrap();
        for cd_first in [false, true] {
            let mut rtc = Rtc::new(512 << 20, 0, Arc::new(Line::default()));
            rtc.configure_pc([Some(1_048_576), None, None, None], cd_first, cpus);
            for (index, key) in [(0x38, "0x38"), (0x3d, "0x3d"), (0x5f, "0x5f")] {
                let expected = capture[key]["read_values"].as_array().unwrap();
                assert_eq!(expected.len(), 1);
                let expected =
                    u8::from_str_radix(expected[0].as_str().unwrap().trim_start_matches("0x"), 16)
                        .unwrap();
                assert_eq!(
                    cmos(&mut rtc, index),
                    expected,
                    "{name} CMOS{index:#x}, CD={cd_first}"
                );
            }
            assert_eq!(cmos(&mut rtc, 0x10), 0);
            assert_eq!(cmos(&mut rtc, 0x14), 6);
        }
    }
}

#[test]
fn cmos_geometry_memory_checksum_and_reset_use_indexed_io() {
    let mut rtc = Rtc::new(8 << 30, 0, Arc::new(Line::default()));
    rtc.set_memory_layout(3 << 30, 5 << 30);
    rtc.configure_pc(
        [Some(100 * 16 * 63), Some(1 << 32), Some(64 * 16 * 63), None],
        false,
        4,
    );
    assert_eq!(cmos(&mut rtc, 0x12), 0xff);
    for unit in 0..2u8 {
        assert_eq!(cmos(&mut rtc, 0x19 + unit), 47);
        let base = 0x1b + unit * 9;
        let cylinders = if unit == 0 { 100u16 } else { 16383 };
        assert_eq!(
            u16::from_le_bytes([cmos(&mut rtc, base), cmos(&mut rtc, base + 1)]),
            cylinders
        );
        assert_eq!(cmos(&mut rtc, base + 2), 16);
        assert_eq!(cmos(&mut rtc, base + 3), 0xff);
        assert_eq!(cmos(&mut rtc, base + 4), 0xff);
        assert_eq!(cmos(&mut rtc, base + 5), 0xc8);
        assert_eq!(
            u16::from_le_bytes([cmos(&mut rtc, base + 6), cmos(&mut rtc, base + 7)]),
            cylinders
        );
        assert_eq!(cmos(&mut rtc, base + 8), 63);
    }
    assert_eq!(
        u16::from_le_bytes([cmos(&mut rtc, 0x15), cmos(&mut rtc, 0x16)]),
        640
    );
    assert_eq!(
        u16::from_le_bytes([cmos(&mut rtc, 0x17), cmos(&mut rtc, 0x18)]),
        65535
    );
    assert_eq!(
        u16::from_le_bytes([cmos(&mut rtc, 0x34), cmos(&mut rtc, 0x35)]),
        0xbf00
    );
    assert_eq!(
        [
            cmos(&mut rtc, 0x5b),
            cmos(&mut rtc, 0x5c),
            cmos(&mut rtc, 0x5d)
        ],
        [0, 0x40, 1]
    );
    let sum: u16 = (0x10..0x2e)
        .map(|index| u16::from(cmos(&mut rtc, index)))
        .sum();
    assert_eq!(
        u16::from_be_bytes([cmos(&mut rtc, 0x2e), cmos(&mut rtc, 0x2f)]),
        sum
    );
    let before: Vec<_> = (0x10..0x60).map(|index| cmos(&mut rtc, index)).collect();
    rtc.reset().unwrap();
    assert_eq!(
        (0x10..0x60)
            .map(|index| cmos(&mut rtc, index))
            .collect::<Vec<_>>(),
        before
    );
    let mut secondary_only = Rtc::new(64 << 20, 0, Arc::new(Line::default()));
    secondary_only.configure_pc([None, None, Some(1_048_576), None], false, 1);
    assert_eq!(cmos(&mut secondary_only, 0x12), 0);
    assert_eq!(cmos(&mut secondary_only, 0x19), 0);
}

#[test]
fn i440fx_pam_offsets_shadow_each_bank_and_restore_state_without_q35_aliases() {
    let pam = Arc::new(Mutex::new(Pam::new(&[0x5a; 0x40000], None, 0).unwrap()));
    let mut bridge = Mch::i440fx(pam.clone(), 256 << 20);
    assert_eq!(bridge.config_read(0), 0x1237_8086);
    assert_eq!(bridge.config_read(1), 0);
    assert_eq!(bridge.config_read(2), 0x0600_0002);
    assert_eq!(pci_byte(&mut bridge, 0x57), 32);
    assert_eq!(bridge.ecam_window(), None);
    let mut byte = [0];
    for bank in 0..13u8 {
        let (offset, register, shift) = if bank == 12 {
            (0x30000, 0x59, 4)
        } else {
            (
                u64::from(bank) * 0x4000,
                0x5a + usize::from(bank) / 2,
                (bank % 2) * 4,
            )
        };
        let previous = pci_byte(&mut bridge, register) & !(3 << shift);
        pci_write(&mut bridge, register, &[previous | 2 << shift]);
        pam.lock().write(offset, &[bank]);
        pam.lock().read(offset, &mut byte);
        assert_eq!(byte[0], if bank < 8 { 0xff } else { 0x5a });
        pci_write(&mut bridge, register, &[previous | 1 << shift]);
        pam.lock().read(offset, &mut byte);
        assert_eq!(byte, [bank]);
        pam.lock().write(offset, &[0xfe]);
        pam.lock().read(offset, &mut byte);
        assert_eq!(byte, [bank]);
    }
    let state = bridge.save_state().unwrap();
    pci_write(&mut bridge, 0x90, &[0x30]);
    pci_write(&mut bridge, 0x9d, &[0x40]);
    assert_eq!(pci_byte(&mut bridge, 0x90), 0x30);
    assert_eq!(pci_byte(&mut bridge, 0x9d), 0x40);
    assert_eq!(bridge.ecam_window(), None);
    pci_write(&mut bridge, 0x59, &[0x30]);
    pam.lock().write(0x30000, &[0xfe]);
    bridge.restore_state(&state).unwrap();
    pam.lock().read(0x30000, &mut byte);
    assert_eq!(byte, [12]);
    assert_eq!(bridge.save_state().unwrap(), state);
    assert!(bridge.restore_state(&state[..100]).is_err());
    assert!(bridge.error.is_none());
}

#[test]
fn i440fx_smram_uses_72h_and_preserves_qemu_register_writes() {
    let pam = Arc::new(Mutex::new(Pam::new(&[0; 0x40000], None, 0).unwrap()));
    let mut bridge = Mch::i440fx(pam, 512 << 20);
    assert_eq!(pci_byte(&mut bridge, 0x72), 2);
    pci_write(&mut bridge, 0x72, &[0x4a]);
    assert_eq!(pci_byte(&mut bridge, 0x72), 0x4a);
    pci_write(&mut bridge, 0x72, &[0x5a]);
    assert_eq!(pci_byte(&mut bridge, 0x72), 0x5a);
    pci_write(&mut bridge, 0x72, &[0x4a]);
    assert_eq!(pci_byte(&mut bridge, 0x72), 0x4a);
    let state = bridge.save_state().unwrap();
    bridge.restore_state(&state).unwrap();
    pci_write(&mut bridge, 0x72, &[2]);
    assert_eq!(pci_byte(&mut bridge, 0x72), 2);
    assert!(bridge.error.is_none());
}

#[test]
fn piix3_pirq_routes_share_level_and_disable_both_legacy_destinations() {
    let outputs: [Arc<Line>; 24] = std::array::from_fn(|_| Arc::new(Line::default()));
    let router = PirqRouter::pc(std::array::from_fn(|index| {
        outputs[index].clone() as Arc<dyn IrqLine>
    }));
    let mut bridge = Piix3::default();
    let line = |device| {
        PirqRouter::line(
            &router,
            vmlab_hv_proto::machine::PciAddress {
                bus: 0,
                device,
                function: 0,
            },
            1,
        )
        .unwrap()
    };
    let a = line(2);
    let b = line(6);
    pci_write(&mut bridge, 0x61, &[11]);
    router.lock().set_route(1, bridge.route(1));
    a.set_level(true);
    b.set_level(true);
    a.set_level(false);
    assert!(outputs[11].high());
    assert!(!outputs[17].high());
    pci_write(&mut bridge, 0x61, &[0x8b]);
    router.lock().set_route(1, bridge.route(1));
    assert!(outputs.iter().all(|line| !line.high()));
    pci_write(&mut bridge, 0x61, &[10]);
    router.lock().set_route(1, bridge.route(1));
    assert!(outputs[10].high());
    assert!(!outputs[11].high());
    b.set_level(false);
    assert!(outputs.iter().all(|line| !line.high()));
}
