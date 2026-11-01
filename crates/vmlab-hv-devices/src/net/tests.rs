use super::*;
use crate::{BarKind, IrqLine};
use std::os::unix::net::UnixListener;

struct Irq;
impl IrqLine for Irq {
    fn set_level(&self, _: bool) {}
}
const MAC: [u8; 6] = [0x52, 0x54, 0, 1, 2, 3];

#[test]
fn p2_nic_functions_match_template_pci_captures() {
    let memory = GuestMemory::new();
    let cases: Vec<(Box<dyn PciDevice>, &str, u8)> = vec![
        (
            Box::new(e1000::E1000::new(
                "intel",
                MAC,
                memory.clone(),
                Arc::new(Irq),
            )),
            include_str!("../../../../docs/hwref/pc-windows-7/query-pci.json"),
            3,
        ),
        (
            Box::new(pcnet::Pcnet::new("amd", MAC, memory, Arc::new(Irq))),
            include_str!("../../../../docs/hwref/pc-freedos-1.3/query-pci.json"),
            0x10,
        ),
    ];
    for (mut nic, source, revision) in cases {
        let reference: serde_json::Value = serde_json::from_str(source).unwrap();
        let device = reference[0]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["class_info"]["class"] == 512)
            .unwrap();
        let ids = &device["id"];
        assert_eq!(
            nic.config_read(0),
            ids["vendor"].as_u64().unwrap() as u32 | (ids["device"].as_u64().unwrap() as u32) << 16
        );
        assert_eq!(nic.config_read(2), 0x0200_0000 | u32::from(revision));
        assert_eq!(
            nic.config_read(11),
            ids["subsystem-vendor"].as_u64().unwrap() as u32
                | (ids["subsystem"].as_u64().unwrap() as u32) << 16
        );
        assert_eq!(
            (nic.config_read(15) >> 8) & 0xff,
            device["irq_pin"].as_u64().unwrap() as u32
        );
        let bars = nic.bars();
        assert_eq!(bars.len(), 2);
        for region in device["regions"].as_array().unwrap() {
            let index = region["bar"].as_u64().unwrap() as u8;
            if index == 6 {
                // The captured iPXE ROM is not redistributed. No ROM was
                // supplied, so firmware network boot is intentionally absent.
                assert!(!bars.iter().any(|b| b.index == 6));
                continue;
            }
            let bar = bars.iter().find(|b| b.index == index).unwrap();
            assert_eq!(bar.size, region["size"].as_u64().unwrap());
            assert_eq!(
                bar.kind,
                if region["type"] == "io" {
                    BarKind::Io
                } else {
                    BarKind::Mmio32
                }
            );
            if bar.kind != BarKind::Io {
                assert_eq!(region["prefetch"], false);
                assert_eq!(region["mem_type_64"], false);
            }
        }
    }
}

fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "NIC worker timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn stream_framing_quiesce_restore_and_reset_keep_the_connection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("switch.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
    let mut nic = NetworkCard::new(
        e1000::E1000::new("intel", MAC, mem.clone(), Arc::new(Irq)),
        path,
    )
    .unwrap();
    let mut peer = None;
    wait(|| match listener.accept() {
        Ok((stream, _)) => {
            peer = Some(stream);
            true
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
        Err(e) => panic!("{e}"),
    });
    let mut peer = peer.unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let packet = {
        let mut packet = vec![0x45; 80];
        packet[..6].copy_from_slice(&MAC);
        packet
    };
    let mut bytes = (packet.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(&packet);
    peer.write_all(&bytes[..2]).unwrap();
    wait(|| nic.shared.lock().wire.input.len() == 2);
    nic.reset().unwrap();
    peer.write_all(&bytes[2..]).unwrap();
    wait(|| nic.shared.lock().wire.input.is_empty());
    // Reset did not turn the old partial prefix into a new frame length.
    assert!(listener.accept().is_err());
    nic.quiesce();
    nic.config_write(1, 0, &7u16.to_le_bytes());
    let mut desc = [0; 16];
    desc[..8].copy_from_slice(&0x8000u64.to_le_bytes());
    mem.write_slice(&desc, GuestAddress(0x1000)).unwrap();
    for (reg, value) in [
        (0x2800, 0x1000u32),
        (0x2808, 128),
        (0x2818, 7),
        (0x100, 2 | 1 << 26),
    ] {
        nic.bar_write(0, reg, &value.to_le_bytes());
    }
    peer.write_all(&bytes).unwrap();
    assert_eq!(read::<1>(&mem, 0x100c), Some([0]));
    let function = nic.save_function_state().unwrap();
    let wire = nic.save_wrapped_state().unwrap();
    nic.bar_write(0, 0x100, &0u32.to_le_bytes());
    nic.restore_function_state(&function).unwrap();
    nic.restore_wrapped_state(&wire).unwrap();
    nic.resume_snapshot();
    wait(|| read::<1>(&mem, 0x100c).is_some_and(|s| s[0] & 3 == 3));
    assert_eq!(read::<80>(&mem, 0x8000).unwrap().as_slice(), packet);
    nic.quiesce();
    desc[8..10].copy_from_slice(&80u16.to_le_bytes());
    desc[11] = 0xb;
    mem.write_slice(&desc, GuestAddress(0x2000)).unwrap();
    for (reg, value) in [
        (0x3800, 0x2000u32),
        (0x3808, 128),
        (0x3818, 1),
        (0x400, 0xa),
    ] {
        nic.bar_write(0, reg, &value.to_le_bytes());
    }
    nic.resume_snapshot();
    let mut output = vec![0; bytes.len()];
    peer.read_exact(&mut output).unwrap();
    assert_eq!(output, bytes);
}

#[test]
fn malformed_stream_is_disconnected_and_worker_reconnects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("switch.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let nic = NetworkCard::new(
        pcnet::Pcnet::new("amd", MAC, GuestMemory::new(), Arc::new(Irq)),
        path,
    )
    .unwrap();
    let mut peer = None;
    wait(|| {
        peer = listener.accept().ok().map(|p| p.0);
        peer.is_some()
    });
    peer.unwrap().write_all(&u32::MAX.to_be_bytes()).unwrap();
    wait(|| listener.accept().is_ok());
    assert!(nic.shared.lock().wire.input.len() <= MAX_STAGED);
}
