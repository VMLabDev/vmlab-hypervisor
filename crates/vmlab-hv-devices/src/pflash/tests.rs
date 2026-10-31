use super::*;

fn flash(secure: bool) -> (tempfile::TempDir, Pflash) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("code"), vec![0x5a; CODE_SIZE]).unwrap();
    std::fs::write(dir.path().join("vars"), vec![0xff; VARS_SIZE]).unwrap();
    let flash = Pflash::open(&dir.path().join("code"), &dir.path().join("vars"), secure).unwrap();
    (dir, flash)
}

#[test]
fn system_reset_flushes_vars_and_discards_pending_commands() {
    use crate::Resettable;
    let (dir, mut f) = flash(false);
    for command in [0x10, 0x42] {
        f.write_access(123, &[command], false).unwrap();
    }
    assert!(f.dirty);
    f.write(0x1000, &[0x20]);
    f.reset().unwrap();
    assert!(!f.dirty);
    assert_eq!(read(&mut f, 123, 1), [0x42]);
    f.write(0x1000, &[0xd0]);
    assert_eq!(f.bytes[0x1000], 0xff);
    let mut reopened =
        Pflash::open(&dir.path().join("code"), &dir.path().join("vars"), false).unwrap();
    assert_eq!(read(&mut reopened, 123, 1), [0x42]);
}

#[test]
fn word_programming_batches_durability_and_reports_flush_failures() {
    let (_dir, mut f) = flash(false);
    f.last_sync = Instant::now() + Duration::from_secs(60);
    for offset in 0..128 {
        for byte in [0x10, 0x42] {
            f.write_access(offset, &[byte], false).unwrap();
        }
        assert!(
            f.dirty,
            "each word must not issue its own durability barrier"
        );
    }
    assert!(f.take_due_sync().unwrap().is_none(), "the batch is not due");
    f.last_sync = Instant::now() - Duration::from_millis(16);
    let handle = f.take_due_sync().unwrap().expect("a due batch");
    assert!(!f.dirty, "writes during the host sync start the next batch");
    handle.sync_data().unwrap();
    f.sync_failed();
    assert!(f.dirty);
    assert_ne!(f.status & PROGRAM_ERROR, 0);
    f.status = READY;
    f.flush().unwrap();
    assert!(!f.dirty);
    f.dirty = true;
    f.vars = File::open("/dev/null").unwrap();
    assert!(f.flush().is_err());
    assert!(f.dirty);
    assert_ne!(f.status & PROGRAM_ERROR, 0);
}

fn read(flash: &mut Pflash, offset: u64, n: usize) -> Vec<u8> {
    let mut data = vec![0; n];
    flash.read(offset, &mut data);
    data
}

#[test]
fn ovmf_detection_reads_zero_cleared_status_then_program_completion() {
    let (_dir, mut f) = flash(false);
    f.write(0x10, &[0x50]);
    assert_eq!(read(&mut f, 0x10, 1), [0xff]);
    f.write(0x10, &[0x70]);
    assert_eq!(read(&mut f, 0x10, 1), [0]);
    f.write(0x10, &[0x10]);
    f.write(0x10, &[0x42]);
    assert_eq!(read(&mut f, 0x10, 1), [READY]);
    f.write(0x10, &[0xff]);
    assert_eq!(read(&mut f, 0x10, 1), [0x42]);
}

#[test]
fn ovmf_program_erase_and_reopen() {
    let (dir, mut f) = flash(false);
    for (command, address, data) in [(0x10, 123, vec![0x42]), (0x40, 124, vec![0x12, 0x34])] {
        f.write(address, &[command]);
        f.write(address, &data);
        assert_eq!(read(&mut f, address, 1), [READY]);
        f.write(0, &[0xff]);
        assert_eq!(read(&mut f, address, data.len()), data);
    }
    drop(f);
    let mut f = Pflash::open(&dir.path().join("code"), &dir.path().join("vars"), false).unwrap();
    assert_eq!(read(&mut f, 123, 3), [0x42, 0x12, 0x34]);
    f.write(123, &[0x20]);
    f.write(123, &[0xd0]);
    f.write(0, &[0xff]);
    assert_eq!(read(&mut f, 0, BLOCK_SIZE), vec![0xff; BLOCK_SIZE]);
    assert_eq!(
        std::fs::read(dir.path().join("vars")).unwrap(),
        vec![0xff; VARS_SIZE]
    );
}

#[test]
fn protection_status_cfi_and_snapshot() {
    let (_dir, mut f) = flash(true);
    f.write(0, &[0x10]);
    f.write(0, &[0]);
    assert_eq!(read(&mut f, 0, 1), [0xff]);
    f.write_access(0, &[0x50], true).unwrap();
    for command in [0x10, 0] {
        f.write_access(VARS_SIZE as u64, &[command], true).unwrap();
    }
    assert_eq!(read(&mut f, 0, 1), [READY | PROGRAM_ERROR | PROTECTED]);
    for command in [0x50, 0x10, 0x42] {
        f.write_access(0, &[command], true).unwrap();
    }
    f.write_access(0, &[0xff], true).unwrap();
    let state = f.save_state().unwrap();
    for command in [0x20, 0xd0] {
        f.write_access(0, &[command], true).unwrap();
    }
    f.restore_state(&state).unwrap();
    assert_eq!(read(&mut f, 0, 1), [0x42]);
    f.write_access(0, &[0x98], true).unwrap();
    assert_eq!(read(&mut f, 0x10, 3), b"QRY");
    assert_eq!(read(&mut f, 0x2c, 5), [1, 0xff, 3, 0x10, 0]);
    f.write_access(0, &[0x90], true).unwrap();
    assert_eq!(read(&mut f, 0, 2), [0x89, 0x18]);
    assert!(f.restore_state(&state[..20]).is_err());
}

#[test]
fn invalid_program_and_erase_sequences_do_not_modify_array() {
    let (_dir, mut f) = flash(false);
    for command in [0x10, 0, 0x10, 0xff] {
        f.write(0, &[command]);
    }
    assert_eq!(read(&mut f, 0, 1), [READY | PROGRAM_ERROR]);
    f.write(0, &[0x50]);
    f.write(0, &[0x20]);
    f.write(BLOCK_SIZE as u64, &[0xd0]);
    assert_eq!(read(&mut f, 0, 1), [READY | ERASE_ERROR | PROGRAM_ERROR]);
    f.write(0, &[0xff]);
    assert_eq!(read(&mut f, 0, 1), [0]);
    f.read(u64::MAX, &mut [0; 4]);
    f.write(u64::MAX, &[0; 4]);
}

#[test]
fn host_write_failure_is_reported_and_latched() {
    let (dir, mut f) = flash(false);
    f.vars = File::open(dir.path().join("vars")).unwrap();
    f.write_access(0, &[0x10], false).unwrap();
    assert!(f.write_access(0, &[0], false).is_err());
    assert_eq!(read(&mut f, 0, 1), [READY | PROGRAM_ERROR]);
    f.write(0, &[0xff]);
    assert_eq!(read(&mut f, 0, 1), [0xff]);
}

#[test]
fn snapshot_preserves_pending_commands_and_validates_before_writing() {
    let (dir, mut f) = flash(false);
    f.write(0x1000, &[0x20]);
    let pending = f.save_state().unwrap();
    f.write(0, &[0xff]);
    f.restore_state(&pending).unwrap();
    f.write(0x1000, &[0xd0]);
    assert_eq!(read(&mut f, 0, 1), [READY]);
    let mut invalid = pending;
    invalid[5] = 99;
    let file_before = std::fs::read(dir.path().join("vars")).unwrap();
    assert!(f.restore_state(&invalid).is_err());
    assert_eq!(std::fs::read(dir.path().join("vars")).unwrap(), file_before);
}
