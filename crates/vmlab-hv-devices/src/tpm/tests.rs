use super::*;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct Swtpm {
    child: Child,
    directory: tempfile::TempDir,
}

impl Drop for Swtpm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Swtpm {
    fn start() -> Option<Self> {
        match Command::new("swtpm").arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("SKIP P4 TPM: swtpm executable not installed");
                return None;
            }
            result => assert!(result.unwrap().status.success()),
        }
        let directory = tempfile::tempdir().unwrap();
        let child = Command::new("swtpm")
            .args(["socket", "--tpm2", "--tpmstate"])
            .arg(format!("dir={}", directory.path().display()))
            .arg("--ctrl")
            .arg(format!(
                "type=unixio,path={}/ctrl",
                directory.path().display()
            ))
            .arg("--terminate")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut emulator = Self { child, directory };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !emulator.path().exists() {
            assert!(
                emulator.child.try_wait().unwrap().is_none(),
                "swtpm exited before binding"
            );
            assert!(
                Instant::now() < deadline,
                "swtpm did not bind its control socket"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        Some(emulator)
    }

    fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("ctrl")
    }

    fn device(&self, irq: Option<Arc<dyn IrqLine>>) -> TpmTis {
        TpmTis::connect(self.path(), irq).unwrap()
    }
}

#[derive(Default)]
struct Irq(AtomicBool);
impl IrqLine for Irq {
    fn set_level(&self, high: bool) {
        self.0.store(high, Ordering::SeqCst);
    }
}

fn read(device: &mut TpmTis, offset: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    device.read(offset, &mut data);
    data
}

fn packet(code: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x80, 1];
    bytes.extend_from_slice(&(10u32 + payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&code.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn submit(device: &mut TpmTis, locality: u64, command: &[u8]) -> Vec<u8> {
    let base = locality * 0x1000;
    device.write(base, &[2]);
    assert_ne!(read(device, base, 1)[0] & 0x20, 0);
    device.write(base + 0x18, &[READY]);
    assert_eq!(read(device, base + 0x18, 1), [READY]);
    device.write(base + 0x24, &command[..command.len() - 1]);
    assert_eq!(read(device, base + 0x18, 1)[0] & EXPECT, EXPECT);
    device.write(base + 0x24, &command[command.len() - 1..]);
    assert_eq!(read(device, base + 0x18, 1)[0] & EXPECT, 0);
    device.write(base + 0x18, &[0x20]);
    assert_eq!(device.last_error(), None);
    assert_eq!(read(device, base + 0x18, 1), [VALID | AVAILABLE]);
    let burst = read(device, base + 0x19, 2);
    let len = u16::from_le_bytes(burst.try_into().unwrap()) as usize;
    let response = read(device, base + 0x24, len);
    assert_eq!(read(device, base + 0x18, 1), [VALID]);
    assert_eq!(&response[6..10], &[0; 4], "TPM error: {response:02x?}");
    response
}

fn startup(device: &mut TpmTis) {
    submit(device, 0, &packet(0x144, &[0, 0]));
}

#[test]
fn system_reset_reinitializes_the_same_swtpm_and_clears_pcrs() {
    use crate::Resettable;
    let Some(emulator) = Swtpm::start() else {
        return;
    };
    let mut device = emulator.device(None);
    startup(&mut device);
    let initial = pcr_read(&mut device);
    extend(&mut device, 0x42);
    assert_ne!(pcr_read(&mut device), initial);
    device.reset().unwrap();
    assert_eq!(device.state.active, None);
    assert!(device.state.command.is_empty());
    startup(&mut device);
    assert_eq!(pcr_read(&mut device), initial);
    assert!(emulator.child.id() > 0);
    device.shutdown().unwrap();
}

fn pcr_read(device: &mut TpmTis) -> Vec<u8> {
    let response = submit(
        device,
        0,
        &packet(0x17e, &[0, 0, 0, 1, 0, 0x0b, 3, 0, 0, 1]),
    );
    response[response.len() - 32..].to_vec()
}

fn extend(device: &mut TpmTis, digest: u8) {
    let mut command = packet(0x182, &[]);
    command[1] = 2;
    command.extend_from_slice(&16u32.to_be_bytes());
    command.extend_from_slice(&9u32.to_be_bytes());
    command.extend_from_slice(&[0x40, 0, 0, 9, 0, 0, 0, 0, 0]);
    command.extend_from_slice(&[0, 0, 0, 1, 0, 0x0b]);
    command.extend_from_slice(&[digest; 32]);
    let len = command.len() as u32;
    command[2..6].copy_from_slice(&len.to_be_bytes());
    submit(device, 0, &command);
}

#[test]
fn p4_startup_random_interrupt_and_retry() {
    let Some(emulator) = Swtpm::start() else {
        return;
    };
    let irq = Arc::new(Irq::default());
    let mut device = emulator.device(Some(irq.clone()));
    startup(&mut device);
    device.write(8, &0x8000_0001u32.to_le_bytes());
    device.write(16, &u32::MAX.to_le_bytes());
    assert!(!irq.0.load(Ordering::SeqCst));
    let response = submit(&mut device, 0, &packet(0x17b, &[0, 16]));
    assert_eq!(response.len(), 28);
    assert_eq!(&response[10..12], &[0, 16]);
    assert!(irq.0.load(Ordering::SeqCst));
    device.write(0x18, &[2]);
    assert_eq!(read(&mut device, 0x24, response.len()), response);
    device.write(16, &1u32.to_le_bytes());
    assert!(!irq.0.load(Ordering::SeqCst));
    device.shutdown().unwrap();
}

#[test]
fn p4_locality_arbitration_and_register_protection() {
    let Some(emulator) = Swtpm::start() else {
        return;
    };
    let mut device = emulator.device(None);
    assert_eq!(read(&mut device, 0x18, 4), [0xff; 4]);
    assert_eq!(read(&mut device, 0x14, 4), 0x3000_0697u32.to_le_bytes());
    assert_eq!(read(&mut device, 0x30, 8), 0x2100u64.to_le_bytes());
    device.write(0x32, &[8]);
    device.write(0x32, &[0]);
    assert_eq!(read(&mut device, 0x30, 8), 0x82100u64.to_le_bytes());
    device.write(0, &[2]);
    device.write(0x1000, &[2]);
    assert_ne!(read(&mut device, 0x1000, 1)[0] & 2, 0);
    device.write(0x1000, &[0x20]);
    assert_eq!(read(&mut device, 0x1000, 1)[0] & 2, 0);
    device.write(0x1000, &[2]);
    assert_ne!(read(&mut device, 0, 1)[0] & 4, 0);
    device.write(0x1018, &[READY]);
    assert_eq!(read(&mut device, 0x18, 1), [0]);
    device.write(0x4000, &[8]);
    assert_ne!(read(&mut device, 0, 1)[0] & 0x10, 0);
    assert_ne!(read(&mut device, 0x4000, 1)[0] & 0x20, 0);
    device.write(0x4000, &[0x20]);
    assert_ne!(read(&mut device, 0x1000, 1)[0] & 0x20, 0);
    device.write(0x1000, &[0x20]);
    for locality in 0..5 {
        let response = if locality == 0 {
            packet(0x144, &[0, 0])
        } else {
            packet(0x17b, &[0, 8])
        };
        submit(&mut device, locality, &response);
        assert_eq!(
            read(&mut device, locality * 0x1000 + 0xf00, 4),
            [0x14, 0x10, 1, 0]
        );
        device.write(locality * 0x1000, &[0x20]);
    }
    assert_eq!(read(&mut device, u64::MAX, 4), [0; 4]);
    device.shutdown().unwrap();
}

#[test]
fn p4_locality_four_hash_and_establishment_reset() {
    let Some(emulator) = Swtpm::start() else {
        return;
    };
    let mut device = emulator.device(None);
    startup(&mut device);
    device.write(0, &[0x20]);
    device.write(0x4028, &[0]);
    assert_eq!(device.last_error(), None);
    assert_eq!(read(&mut device, 0x4000, 1)[0] & 0x21, 0x20);
    device.write(0x4024, b"measured startup");
    let saved = device.save_state().unwrap();
    device.write(0x4020, &[0]);
    assert_eq!(device.last_error(), None);
    device.write(0x3000, &[2]);
    device.write(0x301b, &[2]);
    assert_eq!(device.last_error(), None);
    assert_eq!(read(&mut device, 0x3000, 1)[0] & 1, 1);
    device.write(0x3000, &[0x20]);
    let command = packet(0x17e, &[0, 0, 0, 1, 0, 0x0b, 3, 0, 0, 2]);
    let first = submit(&mut device, 0, &command);
    device.restore_state(&saved).unwrap();
    device.write(0x4020, &[0]);
    assert_eq!(device.last_error(), None);
    let second = submit(&mut device, 0, &command);
    assert_eq!(&first[first.len() - 32..], &second[second.len() - 32..]);
    device.shutdown().unwrap();
}

#[test]
fn p4_snapshot_restores_pcr_fifo_interrupts_and_physical_presence() {
    let Some(emulator) = Swtpm::start() else {
        return;
    };
    let irq = Arc::new(Irq::default());
    let mut device = emulator.device(Some(irq.clone()));
    startup(&mut device);
    extend(&mut device, 0x42);
    let before = pcr_read(&mut device);
    device.write(0x5109, &23u32.to_le_bytes());
    device.write(0x515a, &[1]);
    device.write(8, &0x8000_0001u32.to_le_bytes());
    let command = packet(0x17b, &[0, 16]);
    device.write(0x18, &[READY]);
    device.write(0x24, &command[..7]);
    let saved = device.save_state().unwrap();
    extend(&mut device, 0x99);
    assert_ne!(pcr_read(&mut device), before);
    device.write(0x5109, &[0; 4]);
    device.write(8, &0u32.to_le_bytes());
    assert!(!irq.0.load(Ordering::SeqCst));
    device.restore_state(&saved).unwrap();
    assert!(irq.0.load(Ordering::SeqCst));
    assert_eq!(read(&mut device, 0x5109, 4), 23u32.to_le_bytes());
    assert_eq!(read(&mut device, 0x515a, 1), [1]);
    assert_eq!(read(&mut device, 0x18, 1), [VALID | EXPECT]);
    device.write(0x24, &command[7..]);
    device.write(0x18, &[0x20]);
    let response = read(&mut device, 0x24, 28);
    assert_eq!(&response[6..12], &[0, 0, 0, 0, 0, 16]);
    assert_eq!(pcr_read(&mut device), before);
    let response = submit(&mut device, 0, &packet(0x17b, &[0, 16]));
    let first = read(&mut device, 0x24, 5);
    assert_eq!(first, [0xff; 5]);
    device.write(0x18, &[2]);
    read(&mut device, 0x24, 5);
    let saved = device.save_state().unwrap();
    read(&mut device, 0x24, 23);
    device.restore_state(&saved).unwrap();
    assert_eq!(read(&mut device, 0x24, 23), response[5..]);
    let other = Swtpm::start().unwrap();
    let mut restored = other.device(None);
    restored.restore_state(&saved).unwrap();
    assert_eq!(read(&mut restored, 0x24, 23), response[5..]);
    assert_eq!(pcr_read(&mut restored), before);
    restored.shutdown().unwrap();
    assert!(device.restore_state(b"{}").is_err());
    device.shutdown().unwrap();
}
