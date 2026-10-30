//! Proves the oracle library links and executes: a hand-assembled RV64
//! program runs through the Sail model with host-served memory, and the
//! architectural state lands where the RISC-V unprivileged spec says it
//! must.
//!
//! One test function by design: the oracle C ABI allows a single instance
//! per process (the Sail memory seam is process-global), and cargo runs
//! tests on parallel threads.
//!
//! Run with `just tcg-oracle-test` (which runs `tcg-oracle-build` first).
use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;

use vmlab_hv_oracle_test::*;
/// The pinned model config maps RAM at 0x80000000 (see riscv64-config.json,
/// memory.regions); the program must live inside a configured region or
/// every fetch fails the model's PMA checks before reaching the host.
const RAM_BASE: u64 = 0x8000_0000;
const RESET_PC: u64 = RAM_BASE;

/// A tiny flat 64 KiB RAM image at RAM_BASE, plus a log of every store the
/// model made through the host callback.
struct HostRam {
    mem: Vec<u8>,
    stores: Vec<(u64, Vec<u8>)>,
}

// Host memory callbacks: the model routes fetches, loads and stores here.
unsafe extern "C" fn host_read(ctx: *mut c_void, addr: u64, buf: *mut u8, len: usize, _kind: i32) -> i32 {
    let ram = &*(ctx as *const HostRam);
    let Some(offset) = offset_in_ram(addr, len, ram.mem.len()) else {
        // Reads outside RAM fail; the oracle fills architecturally-safe
        // 0xff garbage for data reads and aborts for fetches.
        return -1;
    };
    std::ptr::copy_nonoverlapping(ram.mem.as_ptr().add(offset), buf, len);
    0
}

unsafe extern "C" fn host_write(ctx: *mut c_void, addr: u64, buf: *const u8, len: usize, _kind: i32) -> i32 {
    let ram = &mut *(ctx as *mut HostRam);
    let Some(offset) = offset_in_ram(addr, len, ram.mem.len()) else {
        return -1;
    };
    std::ptr::copy_nonoverlapping(buf, ram.mem.as_mut_ptr().add(offset), len);
    ram.stores.push((addr, std::slice::from_raw_parts(buf, len).to_vec()));
    0
}

/// The host RAM image is mapped at RAM_BASE; accesses outside it fail.
fn offset_in_ram(addr: u64, len: usize, size: usize) -> Option<usize> {
    let offset = addr.checked_sub(RAM_BASE)?;
    let end = offset.checked_add(len as u64)?;
    if end > size as u64 {
        return None;
    }
    Some(offset as usize)
}

fn oracle_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::var("VMLAB_HV_ORACLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../../../target/tcg/oracle"));
    dir.canonicalize().unwrap_or(dir)
}

/// The word `lw` loads: 0xdeadbeef as a negative i32, sign-extended.
const LOADED: u64 = 0xFFFF_FFFF_DEAD_BEEF;

#[test]
fn oracle_steps_hand_assembled_rv64_program() {
    let config = oracle_dir().join("riscv64-config.json");
    assert!(config.exists(), "missing {}", config.display());

    // Hand-assembled RV64IM program, at RAM_BASE:
    //   +0x00: 0x00000297  auipc t0, 0           # t0 = 0x80000000 (lui
    //                                            #  would sign-extend bit 31)
    //   +0x04: 0x00700313  addi  t1, x0, 7       # t1 = 7
    //   +0x08: 0x0402A383  lw    t2, 0x40(t0)    # t2 = mem[+0x40]
    //   +0x0C: 0x00638433  add   t3, t2, t1      # t3 = t2 + 7
    //   +0x10: 0x0482A223  sw    t3, 0x44(t0)    # mem[+0x44] = t3
    //   +0x14: 0x340414F3  csrrw t4, mscratch,t3 # t4 = old mscratch (0)
    //   +0x18: 0x10500073  wfi                   # retires under exit_wait=true
    let program: [u32; 7] = [
        0x0000_0297, 0x0070_0313, 0x0402_A383, 0x0063_8433, 0x0482_A223, 0x3404_14F3, 0x1050_0073,
    ];


    let mut ram = HostRam { mem: vec![0; 0x10000], stores: Vec::new() };
    for (i, word) in program.iter().enumerate() {
        let at = i * 4;
        ram.mem[at..at + 4].copy_from_slice(&word.to_le_bytes());
    }
    ram.mem[0x40..0x44].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());

    let config_path = CString::new(config.as_os_str().as_encoded_bytes())
        .expect("config path contains a NUL byte");

    unsafe {
        let o = oracle_create(config_path.as_ptr(), RESET_PC);
        assert!(!o.is_null(), "oracle_create failed");
        oracle_set_mem_callbacks(o, Some(host_read), Some(host_write), &mut ram as *mut HostRam as *mut c_void);

        // Six instructions retire cleanly. The wfi first reports
        // ORACLE_WAITING (the model entered its wait state and retired
        // nothing); the next step force-exits the wait — the exit_wait=true
        // policy, mirroring sail_riscv_sim — and retires it. A timer
        // interrupt is pending (mtimecmp is 0 at reset) but mstatus.MIE is
        // 0, so no interrupt trap is taken.
        for i in 0..6 {
            assert_eq!(oracle_step(o), ORACLE_OK, "instruction {i} did not retire cleanly");
        }
        assert_eq!(oracle_step(o), ORACLE_WAITING, "wfi enters the wait state");
        assert_eq!(oracle_step(o), ORACLE_OK, "the wait force-exits and retires the wfi");

        // ---- architectural state ----

        let mut x = [0u64; 10];
        for (i, slot) in x.iter_mut().enumerate() {
            assert_eq!(oracle_get_x(o, i as u32, slot), ORACLE_OK);
        }
        assert_eq!(x[0], 0, "x0 stays zero");
        assert_eq!(x[5], RAM_BASE, "lui t0");
        assert_eq!(x[6], 7, "addi t1");
        assert_eq!(x[7], LOADED, "lw t2 sign-extends");
        assert_eq!(x[8], LOADED.wrapping_add(7), "add t3");
        assert_eq!(x[9], 0, "csrrw t4 reads the old mscratch");

        let mut pc = 0u64;
        assert_eq!(oracle_get_pc(o, &mut pc), ORACLE_OK);
        assert_eq!(pc, RAM_BASE + 0x1C, "pc advanced past the retired wfi");

        // The store callback observed the sw, little-endian.
        assert_eq!(ram.stores.len(), 1, "exactly one store");
        assert_eq!(ram.stores[0].0, RAM_BASE + 0x44);
        assert_eq!(ram.stores[0].1, (LOADED.wrapping_add(7) as u32).to_le_bytes());

        // Stepping onto the zeroed word past the program raises an illegal
        // instruction (mcause 2) through the trap path, with PC redirected
        // to mtvec (0 at reset).
        assert_eq!(oracle_step(o), ORACLE_TRAP, "0x0000 must trap");
        let mut cause = 0u64;
        assert_eq!(oracle_trap_cause(o, &mut cause), ORACLE_OK);
        assert_eq!(cause, 2, "illegal instruction mcause");
        assert_eq!(oracle_get_pc(o, &mut pc), ORACLE_OK);
        assert_eq!(pc, 0, "mtvec is 0 at reset");

        // The CSR write landed and reads back through the model's own path.
        let mut mscratch = 1u64;
        assert_eq!(oracle_get_csr(o, 0x340, &mut mscratch), ORACLE_OK);
        assert_eq!(mscratch, LOADED.wrapping_add(7));

        // ---- metadata, enumeration, state injection ----

        assert_eq!(oracle_xlen(o), 64);
        let version = CStr::from_ptr(oracle_sail_version());
        assert!(
            version.to_string_lossy().starts_with("sail 0.20.3, sail-riscv fc074f9"),
            "version string: {}",
            version.to_string_lossy()
        );
        let isa = CStr::from_ptr(oracle_isa_string(o));
        assert!(isa.to_string_lossy().starts_with("rv64"), "isa: {}", isa.to_string_lossy());

        let count = oracle_csr_count(o);
        assert!(count > 100, "expected a full CSR set, got {count}");
        let mut have_mstatus = false;
        for i in 0..count {
            let mut addr = 0u16;
            assert_eq!(oracle_csr_at(o, i as u32, &mut addr), ORACLE_OK);
            have_mstatus |= addr == 0x300;
        }
        assert!(have_mstatus, "mstatus (0x300) not enumerable");

        let mut val = 0u64;
        assert_eq!(oracle_get_csr(o, 0x300, &mut val), ORACLE_OK);
        assert_eq!(oracle_get_csr(o, 0xFFF, &mut val), ORACLE_ERR, "0xfff is not implemented");

        assert_eq!(oracle_set_x(o, 10, 0x1234), ORACLE_OK);
        let mut x10 = 0u64;
        assert_eq!(oracle_get_x(o, 10, &mut x10), ORACLE_OK);
        assert_eq!(x10, 0x1234);
        assert_eq!(oracle_set_x(o, 0, 0xFFFF), ORACLE_OK, "x0 writes are ignored, not errors");
        let mut x0 = 1u64;
        assert_eq!(oracle_get_x(o, 0, &mut x0), ORACLE_OK);
        assert_eq!(x0, 0);

        oracle_destroy(o);
    }
}
