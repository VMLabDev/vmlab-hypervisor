//! Host debugger state is separate from the guest's architectural debug registers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use kvm_bindings::{kvm_debug_exit_arch, kvm_guest_debug, kvm_regs, kvm_sregs};
use kvm_ioctls::VcpuFd;
use parking_lot::Mutex;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmlab_hv_devices::BusDevice;
use vmlab_hv_proto::control::{ErrorCode, Event, Outcome, RunState};
use vmlab_hv_proto::debug::{
    AddressSpace, Breakpoint, BreakpointKind, DebugRequest, DebugResponse, DebugStatus, DebugStop,
    GENERAL_REGISTERS, MATCH_LIMIT, MEMORY_LIMIT, SEARCH_LIMIT, SEGMENT_REGISTERS, StopReason,
    X86Registers,
};

use crate::memory::GuestRam;
use crate::vm::{VmHandle, VmShared};

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const NO_STEP: u16 = 256;
type Result<T> = std::result::Result<T, (ErrorCode, String)>;

pub(crate) struct Debugger {
    step: AtomicU16,
    selected: std::sync::atomic::AtomicBool,
    runnable: [std::sync::atomic::AtomicBool; 256],
    state: Mutex<State>,
}

#[derive(Clone, Default)]
struct State {
    enabled: bool,
    breakpoints: Vec<Breakpoint>,
    stop: Option<DebugStop>,
    sequence: u64,
}

impl Default for Debugger {
    fn default() -> Self {
        Self {
            step: AtomicU16::new(NO_STEP),
            selected: std::sync::atomic::AtomicBool::new(false),
            runnable: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(true)),
            state: Mutex::new(State::default()),
        }
    }
}

impl Debugger {
    pub(crate) fn allowed(&self, id: u8) -> bool {
        !self.selected.load(Ordering::Acquire) || self.runnable[id as usize].load(Ordering::Acquire)
    }
    pub(crate) fn stepping(&self, id: u8) -> bool {
        self.step.load(Ordering::Acquire) == u16::from(id)
    }

    pub(crate) fn cancel_step(&self) {
        self.step.store(NO_STEP, Ordering::Release);
    }
}

fn invalid(message: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::InvalidArgument, message.into())
}

fn internal(error: impl std::fmt::Display) -> (ErrorCode, String) {
    (ErrorCode::Internal, error.to_string())
}

pub(crate) fn handle(shared: &Arc<VmShared>, request: DebugRequest) -> Outcome {
    if !shared
        .kvm
        .kvm
        .check_extension(kvm_ioctls::Cap::SetGuestDebug)
    {
        return Outcome::Error {
            code: ErrorCode::Unsupported,
            message: "KVM guest debugging is unavailable".into(),
        };
    }
    match execute(shared, request) {
        Ok(response) => Outcome::Debug { response },
        Err((code, message)) => Outcome::Error { code, message },
    }
}

fn status(shared: &VmShared) -> DebugResponse {
    let state = shared.debugger.state.lock().clone();
    DebugResponse::Status {
        status: DebugStatus {
            state: shared.status(),
            stepping: shared.debugger.step.load(Ordering::Acquire) != NO_STEP,
            vcpus: shared.vcpus.lock().iter().map(|(id, _)| *id).collect(),
            breakpoints: state.breakpoints,
            stop: state.stop,
        },
    }
}

fn paused(shared: &VmShared) -> Result<()> {
    if shared.status() != RunState::Paused
        || shared.debugger.step.load(Ordering::Acquire) != NO_STEP
    {
        return Err((
            ErrorCode::InvalidState,
            "pause the VM before accessing debugger state".into(),
        ));
    }
    shared.wait_quiesced(STOP_TIMEOUT).map_err(internal)?;
    if shared.exited() {
        return Err((
            ErrorCode::InvalidState,
            "the machine is shutting down".into(),
        ));
    }
    Ok(())
}

fn cpu(shared: &VmShared, id: u8) -> Result<Arc<Mutex<VcpuFd>>> {
    shared
        .vcpus
        .lock()
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .map(|(_, cpu)| cpu.clone())
        .ok_or_else(|| invalid(format!("no vCPU {id}")))
}

fn execute(shared: &Arc<VmShared>, request: DebugRequest) -> Result<DebugResponse> {
    // The public resume path takes this lock itself.
    if matches!(request, DebugRequest::Continue) {
        VmHandle::from_shared(shared.clone())
            .resume()
            .map_err(|e| (ErrorCode::InvalidState, e))?;
        return Ok(status(shared));
    }
    let _operation = shared.begin_snapshot();
    match request {
        DebugRequest::Status => return Ok(status(shared)),
        DebugRequest::Pause => {
            if !matches!(shared.status(), RunState::Paused | RunState::Running) {
                return Err((
                    ErrorCode::InvalidState,
                    "debugging requires a running or paused VM".into(),
                ));
            }
            shared.debugger.cancel_step();
            shared.stop_for_snapshot(STOP_TIMEOUT).map_err(internal)?;
            paused(shared)?;
            let old = shared.debugger.state.lock().clone();
            let mut new = old.clone();
            new.enabled = true;
            configure(shared, &old, &new, None)?;
            *shared.debugger.state.lock() = new;
            let id = shared
                .vcpus
                .lock()
                .first()
                .map(|(id, _)| *id)
                .ok_or_else(|| internal("no vCPUs"))?;
            let fd = cpu(shared, id)?;
            let fd = fd.lock();
            let regs = fd.get_regs().map_err(internal)?;
            let sregs = fd.get_sregs().map_err(internal)?;
            publish_stop(shared, id, linear_pc(&regs, &sregs), StopReason::Pause);
            return Ok(status(shared));
        }
        _ => {}
    }
    paused(shared)?;
    match request {
        DebugRequest::ContinueVcpus { vcpus } => {
            if vcpus.is_empty() {
                return Err(invalid("select at least one vCPU to continue"));
            }
            for id in &vcpus {
                cpu(shared, *id)?;
            }
            prepare_continue(shared).map_err(internal)?;
            for (id, runnable) in shared.debugger.runnable.iter().enumerate() {
                runnable.store(vcpus.contains(&(id as u8)), Ordering::Release);
            }
            shared.debugger.selected.store(true, Ordering::Release);
            shared.finish_snapshot(crate::snapshot::format::RUN_RUNNING);
            Ok(status(shared))
        }
        DebugRequest::Step { vcpu } => {
            cpu(shared, vcpu)?;
            let old = shared.debugger.state.lock().clone();
            let mut new = old.clone();
            new.enabled = true;
            set_resume_flag(shared, &old)?;
            configure(shared, &old, &new, Some(vcpu))?;
            *shared.debugger.state.lock() = new;
            shared
                .debugger
                .step
                .store(u16::from(vcpu), Ordering::Release);
            shared.debug_wake();
            Ok(status(shared))
        }
        DebugRequest::Detach { resume } => {
            let old = shared.debugger.state.lock().clone();
            let new = State {
                sequence: old.sequence,
                ..State::default()
            };
            configure(shared, &old, &new, None)?;
            *shared.debugger.state.lock() = new;
            if resume {
                shared.finish_snapshot(crate::snapshot::format::RUN_RUNNING);
            }
            Ok(status(shared))
        }
        DebugRequest::ReadMemory {
            space,
            address,
            length,
        } => {
            check_length(address, length, MEMORY_LIMIT)?;
            Ok(DebugResponse::Memory {
                data: read_memory(shared, space, address, length)?,
            })
        }
        DebugRequest::WriteMemory {
            space,
            address,
            data,
        } => {
            check_length(address, data.len(), MEMORY_LIMIT)?;
            let ram = shared.ram().ok_or_else(|| internal("no guest RAM"))?;
            let spans = spans(shared, &ram, space, address, data.len(), true)?;
            let mut offset = 0;
            for (physical, length) in spans {
                ram.mem
                    .write_slice(&data[offset..offset + length], GuestAddress(physical))
                    .map_err(internal)?;
                offset += length;
            }
            Ok(DebugResponse::Done)
        }
        DebugRequest::SearchMemory {
            space,
            address,
            length,
            pattern,
        } => {
            check_length(address, length, SEARCH_LIMIT)?;
            if pattern.is_empty() || pattern.len() > MEMORY_LIMIT {
                return Err(invalid("search pattern must contain 1..=65536 bytes"));
            }
            let ram = shared.ram().ok_or_else(|| internal("no guest RAM"))?;
            // Preflight the entire range, including holes beyond the result limit.
            spans(shared, &ram, space, address, length, false)?;
            let mut addresses = Vec::new();
            let mut prefixes = vec![0; pattern.len()];
            for index in 1..pattern.len() {
                let mut matched = prefixes[index - 1];
                while matched > 0 && pattern[index] != pattern[matched] {
                    matched = prefixes[matched - 1];
                }
                if pattern[index] == pattern[matched] {
                    matched += 1;
                }
                prefixes[index] = matched;
            }
            let mut matched = 0;
            let mut offset = 0;
            while offset < length {
                let count = MEMORY_LIMIT.min(length - offset);
                let data = read_memory(shared, space, address + offset as u64, count)?;
                for (index, byte) in data.into_iter().enumerate() {
                    while matched > 0 && byte != pattern[matched] {
                        matched = prefixes[matched - 1];
                    }
                    if byte == pattern[matched] {
                        matched += 1;
                    }
                    if matched == pattern.len() {
                        if addresses.len() == MATCH_LIMIT {
                            return Ok(DebugResponse::Matches {
                                addresses,
                                truncated: true,
                            });
                        }
                        addresses.push(address + (offset + index + 1 - pattern.len()) as u64);
                        matched = prefixes[matched - 1];
                    }
                }
                offset += count;
            }
            Ok(DebugResponse::Matches {
                addresses,
                truncated: false,
            })
        }
        DebugRequest::ReadRegisters { vcpu } => {
            let fd = cpu(shared, vcpu)?;
            let registers = read_registers(&fd.lock())?;
            Ok(DebugResponse::Registers {
                vcpu,
                registers: Box::new(registers),
            })
        }
        DebugRequest::WriteRegisters { vcpu, registers } => {
            let fd = cpu(shared, vcpu)?;
            write_registers(shared, &fd.lock(), &registers)?;
            Ok(DebugResponse::Done)
        }
        DebugRequest::SetRegisters { vcpu, values } => {
            let fd = cpu(shared, vcpu)?;
            let fd = fd.lock();
            let mut registers = read_registers(&fd)?;
            for (name, value) in values {
                if let Some(index) = GENERAL_REGISTERS.iter().position(|r| *r == name) {
                    registers.general[index] = value;
                } else if let Some(index) = SEGMENT_REGISTERS.iter().position(|r| *r == name) {
                    registers.segments[index] = u16::try_from(value)
                        .map_err(|_| invalid("segment selector exceeds 16 bits"))?
                        as u32;
                } else if let Some(register) = registers.system.get_mut(&name) {
                    *register = value;
                } else {
                    return Err(invalid(format!("unknown register {name:?}")));
                }
            }
            write_registers(shared, &fd, &registers)?;
            Ok(DebugResponse::Done)
        }
        DebugRequest::AddBreakpoint { breakpoint }
        | DebugRequest::RemoveBreakpoint { breakpoint } => {
            validate_breakpoint(breakpoint)?;
            let old = shared.debugger.state.lock().clone();
            let mut new = old.clone();
            if matches!(request, DebugRequest::AddBreakpoint { .. }) {
                if !new.breakpoints.contains(&breakpoint) {
                    if new.breakpoints.len() == 4 {
                        return Err(invalid(
                            "all four x86 hardware breakpoint slots are occupied",
                        ));
                    }
                    new.breakpoints.push(breakpoint);
                }
                new.enabled = true;
            } else {
                new.breakpoints.retain(|entry| *entry != breakpoint);
            }
            configure(shared, &old, &new, None)?;
            *shared.debugger.state.lock() = new;
            Ok(status(shared))
        }
        DebugRequest::Status | DebugRequest::Pause | DebugRequest::Continue => unreachable!(),
    }
}

fn check_length(address: u64, length: usize, limit: usize) -> Result<()> {
    if length == 0 || length > limit || address.checked_add(length as u64).is_none() {
        return Err(invalid(format!(
            "memory range must contain 1..={limit} bytes without address overflow"
        )));
    }
    Ok(())
}

/// Translate and validate every page before a write can change any byte.
fn spans(
    shared: &VmShared,
    ram: &GuestRam,
    space: AddressSpace,
    address: u64,
    length: usize,
    write: bool,
) -> Result<Vec<(u64, usize)>> {
    let fd = match space {
        AddressSpace::Physical => None,
        AddressSpace::Virtual { vcpu } => Some(cpu(shared, vcpu)?),
    };
    let fd = fd.as_ref().map(|fd| fd.lock());
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < length {
        let linear = address + offset as u64;
        let physical = match fd.as_ref() {
            Some(fd) => {
                let translation = fd.translate_gva(linear).map_err(internal)?;
                if translation.valid == 0 {
                    return Err(invalid(format!("unmapped linear address {linear:#x}")));
                }
                translation.physical_address
            }
            None => linear,
        };
        let count = (4096 - (linear & 4095) as usize).min(length - offset);
        let end = physical
            .checked_add(count as u64)
            .ok_or_else(|| invalid("physical address overflow"))?;
        let ordinary_ram = ram
            .layout()
            .regions
            .iter()
            .any(|(base, size)| physical >= *base && end <= base + size);
        let rom = !write
            && ram.firmware_rom.as_ref().is_some_and(|mem| {
                mem.find_region(GuestAddress(physical))
                    .is_some_and(|region| end <= region.start_addr().0 + region.len())
            });
        let pam = !write && physical >= 0xc0000 && end <= 0x100000 && shared.mch.lock().is_some();
        if !ordinary_ram && !rom && !pam {
            return Err(invalid(format!(
                "{physical:#x} is not {}",
                if write {
                    "writable guest RAM"
                } else {
                    "guest RAM or firmware ROM"
                }
            )));
        }
        result.push((physical, count));
        offset += count;
    }
    Ok(result)
}

fn read_memory(
    shared: &VmShared,
    space: AddressSpace,
    address: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let ram = shared.ram().ok_or_else(|| internal("no guest RAM"))?;
    let spans = spans(shared, &ram, space, address, length, false)?;
    let mut data = vec![0; length];
    let mut offset = 0;
    for (physical, count) in spans {
        let output = &mut data[offset..offset + count];
        if (0xc0000..0x100000).contains(&physical) && shared.mch.lock().is_some() {
            let pam = shared.mch.lock().as_ref().unwrap().lock().pam.clone();
            pam.lock().read(physical - 0xc0000, output);
        } else if let Some(rom) = ram
            .firmware_rom
            .as_ref()
            .filter(|mem| mem.find_region(GuestAddress(physical)).is_some())
        {
            rom.read_slice(output, GuestAddress(physical))
                .map_err(internal)?;
        } else {
            ram.mem
                .read_slice(output, GuestAddress(physical))
                .map_err(internal)?;
        }
        offset += count;
    }
    Ok(data)
}

fn validate_breakpoint(breakpoint: Breakpoint) -> Result<()> {
    if !matches!(breakpoint.length, 1 | 2 | 4 | 8)
        || !breakpoint
            .address
            .is_multiple_of(u64::from(breakpoint.length))
        || (breakpoint.kind == BreakpointKind::Execute && breakpoint.length != 1)
        || breakpoint
            .address
            .checked_add(u64::from(breakpoint.length))
            .is_none()
    {
        return Err(invalid(
            "hardware breakpoints require aligned lengths 1, 2, 4, or 8; execution breakpoints require length 1",
        ));
    }
    Ok(())
}

fn guest_debug(state: &State, step: bool) -> kvm_guest_debug {
    let mut debug = kvm_guest_debug::default();
    if !state.enabled {
        return debug;
    }
    debug.control = kvm_bindings::KVM_GUESTDBG_ENABLE | kvm_bindings::KVM_GUESTDBG_USE_SW_BP;
    if step {
        debug.control |= kvm_bindings::KVM_GUESTDBG_SINGLESTEP;
    }
    if !state.breakpoints.is_empty() {
        debug.control |= kvm_bindings::KVM_GUESTDBG_USE_HW_BP;
        let mut dr7 = 1 << 10;
        for (slot, breakpoint) in state.breakpoints.iter().enumerate() {
            debug.arch.debugreg[slot] = breakpoint.address;
            let access = match breakpoint.kind {
                BreakpointKind::Execute => 0,
                BreakpointKind::Write => 1,
                BreakpointKind::ReadWrite => 3,
            };
            let length = match breakpoint.length {
                1 => 0,
                2 => 1,
                4 => 3,
                8 => 2,
                _ => unreachable!(),
            };
            dr7 |= (1 << (slot * 2)) | ((access | (length << 2)) << (16 + slot * 4));
        }
        debug.arch.debugreg[7] = dr7;
    }
    debug
}

fn configure(shared: &VmShared, old: &State, new: &State, step: Option<u8>) -> Result<()> {
    let cpus = shared.vcpus.lock().clone();
    for (id, fd) in &cpus {
        let result = fd
            .lock()
            .set_guest_debug(&guest_debug(new, step == Some(*id)));
        if let Err(error) = result {
            for (_, fd) in &cpus {
                if let Err(rollback) = fd.lock().set_guest_debug(&guest_debug(old, false)) {
                    shared.request_exit(crate::vm::ExitReason::Error(format!(
                        "debugger configuration rollback: {rollback}"
                    )));
                }
            }
            return Err(internal(format!(
                "KVM_SET_GUEST_DEBUG on vCPU {id}: {error}"
            )));
        }
    }
    Ok(())
}

fn set_resume_flag(shared: &VmShared, state: &State) -> Result<()> {
    if let Some(stop) = state.stop.as_ref()
        && matches!(
            stop.reason,
            StopReason::Breakpoint {
                breakpoint: Breakpoint {
                    kind: BreakpointKind::Execute,
                    ..
                }
            }
        )
    {
        let fd = cpu(shared, stop.vcpu)?;
        let fd = fd.lock();
        let mut regs = fd.get_regs().map_err(internal)?;
        let sregs = fd.get_sregs().map_err(internal)?;
        if linear_pc(&regs, &sregs) == stop.pc {
            regs.rflags |= 1 << 16; // RF suppresses the execution breakpoint for one instruction.
            fd.set_regs(&regs).map_err(internal)?;
        }
    }
    Ok(())
}

/// Called with the snapshot-operation lock held, before normal resume.
pub(crate) fn prepare_continue(shared: &VmShared) -> std::result::Result<(), String> {
    if shared.debugger.selected.swap(false, Ordering::AcqRel) {
        shared.debug_wake();
    }
    if !shared.debugger.state.lock().enabled {
        return Ok(());
    }
    if shared.status() == RunState::Running {
        return Ok(());
    }
    shared.debugger.cancel_step();
    shared.wait_quiesced(STOP_TIMEOUT)?;
    let state = shared.debugger.state.lock().clone();
    set_resume_flag(shared, &state).map_err(|(_, message)| message)?;
    configure(shared, &state, &state, None).map_err(|(_, message)| message)
}

fn publish_stop(shared: &VmShared, vcpu: u8, pc: u64, reason: StopReason) {
    let stop = {
        let mut state = shared.debugger.state.lock();
        state.sequence += 1;
        let stop = DebugStop {
            sequence: state.sequence,
            vcpu,
            pc,
            reason,
        };
        state.stop = Some(stop.clone());
        stop
    };
    shared.emit(Event::DebugStopped { stop });
}

pub(crate) fn stopped(shared: &VmShared, vcpu: u8, exit: kvm_debug_exit_arch) {
    let reason = {
        let state = shared.debugger.state.lock();
        if exit.exception == 3 {
            StopReason::SoftwareBreakpoint
        } else if let Some(breakpoint) = state
            .breakpoints
            .iter()
            .enumerate()
            .find(|(slot, _)| exit.dr6 & (1 << slot) != 0)
            .map(|(_, breakpoint)| *breakpoint)
        {
            StopReason::Breakpoint { breakpoint }
        } else if shared.debugger.stepping(vcpu) {
            StopReason::Step
        } else {
            StopReason::DebugException
        }
    };
    shared.debug_stop();
    publish_stop(shared, vcpu, exit.pc, reason);
}

fn general(regs: &kvm_regs) -> [u64; 18] {
    [
        regs.rax,
        regs.rbx,
        regs.rcx,
        regs.rdx,
        regs.rsi,
        regs.rdi,
        regs.rbp,
        regs.rsp,
        regs.r8,
        regs.r9,
        regs.r10,
        regs.r11,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15,
        regs.rip,
        regs.rflags,
    ]
}

fn linear_pc(regs: &kvm_regs, sregs: &kvm_sregs) -> u64 {
    regs.rip
        .wrapping_add(if sregs.cs.l == 0 { sregs.cs.base } else { 0 })
}

fn read_registers(fd: &VcpuFd) -> Result<X86Registers> {
    let regs = fd.get_regs().map_err(internal)?;
    let sregs = fd.get_sregs().map_err(internal)?;
    let fpu = fd.get_fpu().map_err(internal)?;
    Ok(X86Registers {
        general: general(&regs),
        segments: [
            sregs.cs.selector as u32,
            sregs.ss.selector as u32,
            sregs.ds.selector as u32,
            sregs.es.selector as u32,
            sregs.fs.selector as u32,
            sregs.gs.selector as u32,
        ],
        st: std::array::from_fn(|i| fpu.fpr[i][..10].try_into().unwrap()),
        fcw: fpu.fcw,
        fsw: fpu.fsw,
        ftw: fpu.ftwx,
        fop: fpu.last_opcode,
        fip: fpu.last_ip,
        fdp: fpu.last_dp,
        xmm: fpu.xmm,
        mxcsr: fpu.mxcsr,
        system: [
            ("cr0", sregs.cr0),
            ("cr2", sregs.cr2),
            ("cr3", sregs.cr3),
            ("cr4", sregs.cr4),
            ("cr8", sregs.cr8),
            ("efer", sregs.efer),
            ("cs_base", sregs.cs.base),
            ("cs_long", sregs.cs.l as u64),
            ("ss_base", sregs.ss.base),
            ("ds_base", sregs.ds.base),
            ("es_base", sregs.es.base),
            ("fs_base", sregs.fs.base),
            ("gs_base", sregs.gs.base),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect(),
    })
}

fn update_sregs(sregs: &mut kvm_sregs, registers: &X86Registers) -> Result<()> {
    let protected = sregs.cr0 & 1 != 0;
    let old_bases = [sregs.cs.base, sregs.ss.base, sregs.ds.base, sregs.es.base];
    let old_fs = (sregs.fs.selector, sregs.fs.base);
    let old_gs = (sregs.gs.selector, sregs.gs.base);
    for (segment, selector) in [
        &mut sregs.cs,
        &mut sregs.ss,
        &mut sregs.ds,
        &mut sregs.es,
        &mut sregs.fs,
        &mut sregs.gs,
    ]
    .into_iter()
    .zip(registers.segments)
    {
        let selector =
            u16::try_from(selector).map_err(|_| invalid("segment selector exceeds 16 bits"))?;
        if segment.selector != selector {
            if protected {
                return Err(invalid(
                    "protected-mode segment selector edits require descriptor loading and are unsupported",
                ));
            }
            segment.selector = selector;
            segment.base = u64::from(selector) << 4;
        }
    }
    for (name, value) in &registers.system {
        match name.as_str() {
            "cr0" => sregs.cr0 = *value,
            "cr2" => sregs.cr2 = *value,
            "cr3" => sregs.cr3 = *value,
            "cr4" => sregs.cr4 = *value,
            "cr8" => sregs.cr8 = *value,
            "efer" => sregs.efer = *value,
            "fs_base" if !protected && sregs.fs.selector != old_fs.0 && *value == old_fs.1 => {}
            "gs_base" if !protected && sregs.gs.selector != old_gs.0 && *value == old_gs.1 => {}
            "fs_base" => sregs.fs.base = *value,
            "gs_base" => sregs.gs.base = *value,
            "cs_base" if *value == old_bases[0] => {}
            "ss_base" if *value == old_bases[1] => {}
            "ds_base" if *value == old_bases[2] => {}
            "es_base" if *value == old_bases[3] => {}
            "cs_long" if *value == sregs.cs.l as u64 => {}
            _ => {
                return Err(invalid(format!(
                    "unsupported system-register edit {name:?}"
                )));
            }
        }
    }
    Ok(())
}

fn write_registers(shared: &VmShared, fd: &VcpuFd, registers: &X86Registers) -> Result<()> {
    let old_regs = fd.get_regs().map_err(internal)?;
    let old_sregs = fd.get_sregs().map_err(internal)?;
    let old_fpu = fd.get_fpu().map_err(internal)?;
    let mut sregs = old_sregs;
    update_sregs(&mut sregs, registers)?;
    let [
        rax,
        rbx,
        rcx,
        rdx,
        rsi,
        rdi,
        rbp,
        rsp,
        r8,
        r9,
        r10,
        r11,
        r12,
        r13,
        r14,
        r15,
        rip,
        rflags,
    ] = registers.general;
    let regs = kvm_regs {
        rax,
        rbx,
        rcx,
        rdx,
        rsi,
        rdi,
        rbp,
        rsp,
        r8,
        r9,
        r10,
        r11,
        r12,
        r13,
        r14,
        r15,
        rip,
        rflags: rflags | 2,
    };
    let mut fpu = old_fpu;
    for (destination, source) in fpu.fpr.iter_mut().zip(registers.st) {
        destination[..10].copy_from_slice(&source);
    }
    fpu.fcw = registers.fcw;
    fpu.fsw = registers.fsw;
    fpu.ftwx = registers.ftw;
    fpu.last_opcode = registers.fop;
    fpu.last_ip = registers.fip;
    fpu.last_dp = registers.fdp;
    fpu.xmm = registers.xmm;
    fpu.mxcsr = registers.mxcsr;
    let result = fd
        .set_sregs(&sregs)
        .and_then(|()| fd.set_regs(&regs))
        .and_then(|()| fd.set_fpu(&fpu));
    if let Err(error) = result {
        let rollback = fd
            .set_sregs(&old_sregs)
            .and_then(|()| fd.set_regs(&old_regs))
            .and_then(|()| fd.set_fpu(&old_fpu));
        return Err(internal(match rollback {
            Ok(()) => format!("register update rejected: {error}; previous values restored"),
            Err(rollback) => {
                let message =
                    format!("register update rejected: {error}; rollback failed: {rollback}");
                shared.request_exit(crate::vm::ExitReason::Error(message.clone()));
                message
            }
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_mode_ignores_the_code_segment_base() {
        let regs = kvm_regs {
            rip: 0x20,
            ..Default::default()
        };
        let mut sregs = kvm_sregs::default();
        sregs.cs.base = 0xf0000;
        assert_eq!(linear_pc(&regs, &sregs), 0xf0020);
        sregs.cs.l = 1;
        assert_eq!(linear_pc(&regs, &sregs), 0x20);
    }

    #[test]
    fn breakpoint_fields_follow_x86_dr7_encoding() {
        for (length, encoding) in [(1, 0), (2, 1), (4, 3), (8, 2)] {
            let breakpoint = Breakpoint {
                address: 0x8000,
                length,
                kind: BreakpointKind::ReadWrite,
            };
            validate_breakpoint(breakpoint).unwrap();
            let state = State {
                enabled: true,
                breakpoints: vec![breakpoint],
                ..State::default()
            };
            let debug = guest_debug(&state, true);
            assert_eq!(debug.arch.debugreg[0], 0x8000);
            assert_eq!(
                debug.arch.debugreg[7],
                0x401 | ((3 | (encoding << 2)) << 16)
            );
            assert_ne!(debug.control & kvm_bindings::KVM_GUESTDBG_SINGLESTEP, 0);
        }
        for (address, length, kind) in [
            (1, 2, BreakpointKind::Write),
            (0, 3, BreakpointKind::Write),
            (0, 2, BreakpointKind::Execute),
        ] {
            assert!(
                validate_breakpoint(Breakpoint {
                    address,
                    length,
                    kind
                })
                .is_err()
            );
        }
        assert_eq!(guest_debug(&State::default(), false).control, 0);
    }

    #[test]
    fn memory_request_limits_reject_overflow_and_empty_ranges() {
        for (address, length) in [(u64::MAX, 1), (0, 0), (0, MEMORY_LIMIT + 1)] {
            assert!(check_length(address, length, MEMORY_LIMIT).is_err());
        }
        check_length(0, MEMORY_LIMIT, MEMORY_LIMIT).unwrap();
    }
}
