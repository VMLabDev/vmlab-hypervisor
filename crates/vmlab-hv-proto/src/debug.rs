//! Whole-machine x86 debugger requests and bounded replies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::control::RunState;

pub const MEMORY_LIMIT: usize = 64 * 1024;
pub const SEARCH_LIMIT: usize = 16 * 1024 * 1024;
pub const MATCH_LIMIT: usize = 1024;
pub const GENERAL_REGISTERS: [&str; 18] = [
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15", "rip", "rflags",
];
pub const SEGMENT_REGISTERS: [&str; 6] = ["cs", "ss", "ds", "es", "fs", "gs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AddressSpace {
    Physical,
    /// A linear address in this vCPU's current page-table context.
    Virtual {
        vcpu: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakpointKind {
    Execute,
    Write,
    ReadWrite,
}

/// Hardware breakpoints use linear addresses on every vCPU. x86 has four slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakpoint {
    pub address: u64,
    pub length: u8,
    pub kind: BreakpointKind,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct X86Registers {
    /// Ordered by GENERAL_REGISTERS.
    pub general: [u64; 18],
    /// Ordered by SEGMENT_REGISTERS.
    pub segments: [u32; 6],
    pub st: [[u8; 10]; 8],
    pub fcw: u16,
    pub fsw: u16,
    pub ftw: u8,
    pub fop: u16,
    pub fip: u64,
    pub fdp: u64,
    pub xmm: [[u8; 16]; 16],
    pub mxcsr: u32,
    /// System registers and segment bases, for inspection and named edits.
    pub system: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    Pause,
    Step,
    Breakpoint { breakpoint: Breakpoint },
    SoftwareBreakpoint,
    DebugException,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugStop {
    pub sequence: u64,
    pub vcpu: u8,
    pub pc: u64,
    #[serde(flatten)]
    pub reason: StopReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugStatus {
    pub state: RunState,
    pub stepping: bool,
    pub vcpus: Vec<u8>,
    pub breakpoints: Vec<Breakpoint>,
    pub stop: Option<DebugStop>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum DebugRequest {
    Status,
    /// Stop all vCPUs and wait for pending KVM exits to complete.
    Pause,
    Continue,
    ContinueVcpus {
        vcpus: Vec<u8>,
    },
    Step {
        vcpu: u8,
    },
    /// Remove host debugger state. Does not undo intentional guest edits.
    Detach {
        resume: bool,
    },
    ReadMemory {
        space: AddressSpace,
        address: u64,
        length: usize,
    },
    WriteMemory {
        space: AddressSpace,
        address: u64,
        data: Vec<u8>,
    },
    SearchMemory {
        space: AddressSpace,
        address: u64,
        length: usize,
        pattern: Vec<u8>,
    },
    ReadRegisters {
        vcpu: u8,
    },
    WriteRegisters {
        vcpu: u8,
        registers: Box<X86Registers>,
    },
    SetRegisters {
        vcpu: u8,
        values: BTreeMap<String, u64>,
    },
    AddBreakpoint {
        breakpoint: Breakpoint,
    },
    RemoveBreakpoint {
        breakpoint: Breakpoint,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DebugResponse {
    Status {
        status: DebugStatus,
    },
    Memory {
        data: Vec<u8>,
    },
    Matches {
        addresses: Vec<u64>,
        truncated: bool,
    },
    Registers {
        vcpu: u8,
        registers: Box<X86Registers>,
    },
    Done,
}
