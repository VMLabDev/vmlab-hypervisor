//! Hand-written bindings for the oracle C ABI (`toolchain/sail/oracle/oracle.h`).
//! Kept in lockstep with that header; the lockstep crate will grow real
//! bindings on top of the same library.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type oracle = c_void;

pub const ORACLE_OK: i32 = 0;
pub const ORACLE_TRAP: i32 = 1;
pub const ORACLE_WAITING: i32 = 2;
pub const ORACLE_ERR: i32 = -1;

pub const ORACLE_MEM_READ: i32 = 0;
pub const ORACLE_MEM_IFETCH: i32 = 1;
pub const ORACLE_MEM_READ_EXCL: i32 = 2;
pub const ORACLE_MEM_WRITE: i32 = 3;
pub const ORACLE_MEM_WRITE_EXCL: i32 = 4;

pub type oracle_read_mem_fn =
    Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u64, buf: *mut u8, len: usize, kind: i32) -> i32>;
pub type oracle_write_mem_fn =
    Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u64, buf: *const u8, len: usize, kind: i32) -> i32>;
pub type oracle_trap_fn = Option<unsafe extern "C" fn(ctx: *mut c_void, is_interrupt: i32, cause: u64)>;
pub type oracle_term_fn = Option<unsafe extern "C" fn(ctx: *mut c_void, bytes: *const i8, len: usize)>;

extern "C" {
    pub fn oracle_create(config_json_path: *const i8, reset_pc: u64) -> *mut oracle;
    pub fn oracle_destroy(o: *mut oracle);
    pub fn oracle_set_mem_callbacks(
        o: *mut oracle,
        read_mem: oracle_read_mem_fn,
        write_mem: oracle_write_mem_fn,
        ctx: *mut c_void,
    );
    pub fn oracle_set_observers(o: *mut oracle, trap: oracle_trap_fn, term: oracle_term_fn, ctx: *mut c_void);
    pub fn oracle_step(o: *mut oracle) -> i32;
    pub fn oracle_trap_cause(o: *mut oracle, cause: *mut u64) -> i32;
    pub fn oracle_tick_clock(o: *mut oracle);
    pub fn oracle_xlen(o: *const oracle) -> u64;
    pub fn oracle_get_pc(o: *const oracle, pc: *mut u64) -> i32;
    pub fn oracle_set_pc(o: *mut oracle, pc: u64) -> i32;
    pub fn oracle_get_x(o: *const oracle, reg: u32, val: *mut u64) -> i32;
    pub fn oracle_set_x(o: *mut oracle, reg: u32, val: u64) -> i32;
    pub fn oracle_get_f(o: *const oracle, reg: u32, bits: *mut u64) -> i32;
    pub fn oracle_set_f(o: *mut oracle, reg: u32, bits: u64) -> i32;
    pub fn oracle_get_csr(o: *mut oracle, csr_addr: u16, val: *mut u64) -> i32;
    pub fn oracle_set_csr(o: *mut oracle, csr_addr: u16, val: u64) -> i32;
    pub fn oracle_csr_count(o: *const oracle) -> i32;
    pub fn oracle_csr_at(o: *const oracle, index: u32, csr_addr: *mut u16) -> i32;
    pub fn oracle_sail_version() -> *const i8;
    pub fn oracle_isa_string(o: *mut oracle) -> *const i8;
}
