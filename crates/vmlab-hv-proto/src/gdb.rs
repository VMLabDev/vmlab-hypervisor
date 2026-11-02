//! GDB Remote Serial Protocol bridge shared by the CLI and socket tests.
//!
//! Each vCPU is a debugger thread. Breakpoint packets, including ordinary
//! `break`, consume the same four hardware slots. No guest instruction is patched.

use std::collections::BTreeSet;
use std::net::TcpStream;
use std::time::Duration;

use gdbstub::arch::{Arch, Registers};
use gdbstub::common::{Signal, Tid};
use gdbstub::conn::ConnectionExt;
use gdbstub::stub::{DisconnectReason, GdbStub, MultiThreadStopReason, run_blocking};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::multithread::{
    MultiThreadBase, MultiThreadResume, MultiThreadResumeOps, MultiThreadSchedulerLocking,
    MultiThreadSchedulerLockingOps, MultiThreadSingleStep, MultiThreadSingleStepOps,
};
use gdbstub::target::ext::base::single_register_access::{
    SingleRegisterAccess, SingleRegisterAccessOps,
};
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, HwBreakpoint, HwBreakpointOps, HwWatchpoint, HwWatchpointOps,
    SwBreakpoint, SwBreakpointOps, WatchKind,
};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::x86::reg::id::{X86_64CoreRegId, X86SegmentRegId, X87FpuInternalRegId};
use gdbstub_arch::x86::reg::{X86_64CoreRegs, X86SegmentRegs, X87FpuInternalRegs};

use crate::control::{Command, Outcome, RunState};
use crate::debug::{
    AddressSpace, Breakpoint, BreakpointKind, DebugRequest, DebugResponse, DebugStatus, StopReason,
    X86Registers,
};

/// A transport through the typed debugger vocabulary, independent of vmlab.
pub trait DebugTarget {
    fn request(&mut self, request: DebugRequest) -> Result<DebugResponse, String>;
}

impl DebugTarget for crate::client::Client {
    fn request(&mut self, request: DebugRequest) -> Result<DebugResponse, String> {
        if !self.has_capability(crate::control::Capability::Debug) {
            return Err("the server does not advertise the debug feature".into());
        }
        let response = self
            .call(Command::Debug { request })
            .map_err(|e| e.to_string());
        self.take_events();
        match response? {
            Outcome::Debug { response } => Ok(response),
            Outcome::Error { message, .. } => Err(message),
            other => Err(format!("unexpected debugger reply: {other:?}")),
        }
    }
}

/// Serve one already-accepted connection. Clean detach resumes the guest.
/// On connection failure or kill, remove breakpoints and leave the VM paused.
pub fn serve(stream: TcpStream, backend: impl DebugTarget) -> Result<(), String> {
    let mut target = Remote {
        backend,
        step: None,
        software: BTreeSet::new(),
        hardware: BTreeSet::new(),
        sequence: 0,
        continued: BTreeSet::new(),
        locked: false,
    };
    target.backend.request(DebugRequest::Pause)?;
    let result = GdbStub::new(stream).run_blocking::<EventLoop<_>>(&mut target);
    let resume = matches!(result, Ok(DisconnectReason::Disconnect));
    // Pause also cancels an in-flight step after an abrupt disconnect.
    let cleanup = target
        .backend
        .request(DebugRequest::Pause)
        .and_then(|_| target.backend.request(DebugRequest::Detach { resume }));
    match result {
        Ok(_) => cleanup.map(|_| ()),
        Err(error) => Err(format!(
            "GDB session: {error}; cleanup: {}",
            match cleanup {
                Ok(_) => "VM paused".into(),
                Err(error) => error,
            }
        )),
    }
}

struct Remote<B> {
    continued: BTreeSet<u8>,
    locked: bool,
    backend: B,
    step: Option<u8>,
    software: BTreeSet<u64>,
    hardware: BTreeSet<u64>,
    sequence: u64,
}

enum X86 {}

impl Arch for X86 {
    type Usize = u64;
    type Registers = X86_64CoreRegs;
    type RegId = gdbstub_arch::x86::reg::id::X86_64CoreRegId;
    type BreakpointKind = usize;

    fn target_description_xml() -> Option<&'static str> {
        Some(include_str!("gdb-target.xml"))
    }
}

fn vcpu(tid: Tid) -> Result<u8, String> {
    u8::try_from(tid.get() - 1).map_err(|_| "invalid vCPU thread id".into())
}

fn tid(vcpu: u8) -> Tid {
    Tid::new(usize::from(vcpu) + 1).unwrap()
}

impl<B: DebugTarget> Remote<B> {
    fn status(&mut self) -> Result<DebugStatus, String> {
        match self.backend.request(DebugRequest::Status)? {
            DebugResponse::Status { status } => Ok(status),
            _ => Err("expected debugger status".into()),
        }
    }

    fn registers(&mut self, id: u8) -> TargetResult<X86Registers, Self> {
        match self
            .backend
            .request(DebugRequest::ReadRegisters { vcpu: id })
            .map_err(|_| TargetError::NonFatal)?
        {
            DebugResponse::Registers { registers, .. } => Ok(*registers),
            _ => Err(TargetError::NonFatal),
        }
    }

    fn breakpoint(&mut self, breakpoint: Breakpoint, add: bool) -> TargetResult<bool, Self> {
        let request = if add {
            DebugRequest::AddBreakpoint { breakpoint }
        } else {
            DebugRequest::RemoveBreakpoint { breakpoint }
        };
        self.backend
            .request(request)
            .map(|_| true)
            .map_err(|_| TargetError::NonFatal)
    }

    fn execution_breakpoint(
        &mut self,
        address: u64,
        software: bool,
        add: bool,
    ) -> TargetResult<bool, Self> {
        let other = if software {
            &self.hardware
        } else {
            &self.software
        };
        if !other.contains(&address) {
            self.breakpoint(
                Breakpoint {
                    address,
                    length: 1,
                    kind: BreakpointKind::Execute,
                },
                add,
            )?;
        }
        let own = if software {
            &mut self.software
        } else {
            &mut self.hardware
        };
        if add {
            own.insert(address);
        } else {
            own.remove(&address);
        }
        Ok(true)
    }
}

impl<B: DebugTarget> Target for Remote<B> {
    type Arch = X86;
    type Error = String;

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::MultiThread(self)
    }
    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }
}

impl<B: DebugTarget> MultiThreadBase for Remote<B> {
    fn support_single_register_access(&mut self) -> Option<SingleRegisterAccessOps<'_, Tid, Self>> {
        Some(self)
    }
    fn read_registers(&mut self, regs: &mut X86_64CoreRegs, thread: Tid) -> TargetResult<(), Self> {
        *regs = to_gdb(&self.registers(vcpu(thread).map_err(TargetError::Fatal)?)?);
        Ok(())
    }

    fn write_registers(&mut self, regs: &X86_64CoreRegs, thread: Tid) -> TargetResult<(), Self> {
        let id = vcpu(thread).map_err(TargetError::Fatal)?;
        let mut registers = self.registers(id)?;
        from_gdb(regs, &mut registers);
        self.backend
            .request(DebugRequest::WriteRegisters {
                vcpu: id,
                registers: Box::new(registers),
            })
            .map(|_| ())
            .map_err(|_| TargetError::NonFatal)
    }

    fn read_addrs(
        &mut self,
        start: u64,
        data: &mut [u8],
        thread: Tid,
    ) -> TargetResult<usize, Self> {
        let space = AddressSpace::Virtual {
            vcpu: vcpu(thread).map_err(TargetError::Fatal)?,
        };
        match self
            .backend
            .request(DebugRequest::ReadMemory {
                space,
                address: start,
                length: data.len(),
            })
            .map_err(|_| TargetError::NonFatal)?
        {
            DebugResponse::Memory { data: bytes } if bytes.len() == data.len() => {
                data.copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            _ => Err(TargetError::NonFatal),
        }
    }

    fn write_addrs(&mut self, start: u64, data: &[u8], thread: Tid) -> TargetResult<(), Self> {
        let space = AddressSpace::Virtual {
            vcpu: vcpu(thread).map_err(TargetError::Fatal)?,
        };
        self.backend
            .request(DebugRequest::WriteMemory {
                space,
                address: start,
                data: data.to_vec(),
            })
            .map(|_| ())
            .map_err(|_| TargetError::NonFatal)
    }

    fn list_active_threads(&mut self, report: &mut dyn FnMut(Tid)) -> Result<(), String> {
        for id in self.status()?.vcpus {
            report(tid(id));
        }
        Ok(())
    }

    fn support_resume(&mut self) -> Option<MultiThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

fn register_range(register: X86_64CoreRegId) -> Option<std::ops::Range<usize>> {
    let (offset, size) = match register {
        X86_64CoreRegId::Gpr(index @ 0..16) => (usize::from(index) * 8, 8),
        X86_64CoreRegId::Rip => (128, 8),
        X86_64CoreRegId::Eflags => (136, 4),
        X86_64CoreRegId::Segment(segment) => {
            let index = match segment {
                X86SegmentRegId::CS => 0,
                X86SegmentRegId::SS => 1,
                X86SegmentRegId::DS => 2,
                X86SegmentRegId::ES => 3,
                X86SegmentRegId::FS => 4,
                X86SegmentRegId::GS => 5,
            };
            (140 + index * 4, 4)
        }
        X86_64CoreRegId::St(index @ 0..8) => (164 + usize::from(index) * 10, 10),
        X86_64CoreRegId::Fpu(register) => {
            let index = match register {
                X87FpuInternalRegId::Fctrl => 0,
                X87FpuInternalRegId::Fstat => 1,
                X87FpuInternalRegId::Ftag => 2,
                X87FpuInternalRegId::Fiseg => 3,
                X87FpuInternalRegId::Fioff => 4,
                X87FpuInternalRegId::Foseg => 5,
                X87FpuInternalRegId::Fooff => 6,
                X87FpuInternalRegId::Fop => 7,
            };
            (244 + index * 4, 4)
        }
        X86_64CoreRegId::Xmm(index @ 0..16) => (276 + usize::from(index) * 16, 16),
        X86_64CoreRegId::Mxcsr => (532, 4),
        _ => return None,
    };
    Some(offset..offset + size)
}

impl<B: DebugTarget> SingleRegisterAccess<Tid> for Remote<B> {
    fn read_register(
        &mut self,
        thread: Tid,
        register: X86_64CoreRegId,
        output: &mut [u8],
    ) -> TargetResult<usize, Self> {
        let range = register_range(register).ok_or(TargetError::NonFatal)?;
        let mut regs = X86_64CoreRegs::default();
        self.read_registers(&mut regs, thread)?;
        let mut data = Vec::new();
        regs.gdb_serialize(|byte| data.push(byte.expect("x86 core registers are available")));
        let count = range.len();
        output
            .get_mut(..count)
            .ok_or(TargetError::NonFatal)?
            .copy_from_slice(&data[range]);
        Ok(count)
    }

    fn write_register(
        &mut self,
        thread: Tid,
        register: X86_64CoreRegId,
        value: &[u8],
    ) -> TargetResult<(), Self> {
        let range = register_range(register).ok_or(TargetError::NonFatal)?;
        if value.len() != range.len() {
            return Err(TargetError::NonFatal);
        }
        let mut regs = X86_64CoreRegs::default();
        self.read_registers(&mut regs, thread)?;
        let mut data = Vec::new();
        regs.gdb_serialize(|byte| data.push(byte.expect("x86 core registers are available")));
        data[range].copy_from_slice(value);
        regs.gdb_deserialize(&data)
            .map_err(|_| TargetError::NonFatal)?;
        self.write_registers(&regs, thread)
    }
}

impl<B: DebugTarget> MultiThreadResume for Remote<B> {
    fn resume(&mut self) -> Result<(), String> {
        self.sequence = self.status()?.stop.map_or(0, |stop| stop.sequence);
        let request = match self.step {
            Some(vcpu) => DebugRequest::Step { vcpu },
            None if self.locked => DebugRequest::ContinueVcpus {
                vcpus: self.continued.iter().copied().collect(),
            },
            None => DebugRequest::Continue,
        };
        self.backend.request(request).map(|_| ())
    }

    fn clear_resume_actions(&mut self) -> Result<(), String> {
        self.step = None;
        self.continued.clear();
        self.locked = false;
        Ok(())
    }

    fn set_resume_action_continue(
        &mut self,
        thread: Tid,
        signal: Option<Signal>,
    ) -> Result<(), String> {
        self.continued.insert(vcpu(thread)?);
        if signal.is_some() {
            return Err("guest signal injection is unsupported".into());
        }
        Ok(())
    }

    fn support_single_step(&mut self) -> Option<MultiThreadSingleStepOps<'_, Self>> {
        Some(self)
    }

    fn support_scheduler_locking(&mut self) -> Option<MultiThreadSchedulerLockingOps<'_, Self>> {
        Some(self)
    }
}

impl<B: DebugTarget> MultiThreadSchedulerLocking for Remote<B> {
    fn set_resume_action_scheduler_lock(&mut self) -> Result<(), String> {
        self.locked = true;
        Ok(())
    }
}

impl<B: DebugTarget> MultiThreadSingleStep for Remote<B> {
    fn set_resume_action_step(
        &mut self,
        thread: Tid,
        signal: Option<Signal>,
    ) -> Result<(), String> {
        let id = vcpu(thread)?;
        if signal.is_some() || self.step.is_some_and(|previous| previous != id) {
            return Err("single-step accepts one vCPU and no signal".into());
        }
        self.step = Some(id);
        Ok(())
    }
}

impl<B: DebugTarget> Breakpoints for Remote<B> {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
    fn support_hw_breakpoint(&mut self) -> Option<HwBreakpointOps<'_, Self>> {
        Some(self)
    }
    fn support_hw_watchpoint(&mut self) -> Option<HwWatchpointOps<'_, Self>> {
        Some(self)
    }
}

impl<B: DebugTarget> SwBreakpoint for Remote<B> {
    fn add_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        self.execution_breakpoint(address, true, true)
    }
    fn remove_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        self.execution_breakpoint(address, true, false)
    }
}

impl<B: DebugTarget> HwBreakpoint for Remote<B> {
    fn add_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        self.execution_breakpoint(address, false, true)
    }
    fn remove_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        self.execution_breakpoint(address, false, false)
    }
}

fn watchpoint(address: u64, length: u64, kind: WatchKind) -> Option<Breakpoint> {
    Some(Breakpoint {
        address,
        length: length.try_into().ok()?,
        kind: match kind {
            WatchKind::Write => BreakpointKind::Write,
            WatchKind::ReadWrite => BreakpointKind::ReadWrite,
            WatchKind::Read => return None,
        },
    })
}

impl<B: DebugTarget> HwWatchpoint for Remote<B> {
    fn add_hw_watchpoint(
        &mut self,
        address: u64,
        length: u64,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        match watchpoint(address, length, kind) {
            Some(point) => self.breakpoint(point, true),
            None => Ok(false),
        }
    }
    fn remove_hw_watchpoint(
        &mut self,
        address: u64,
        length: u64,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        match watchpoint(address, length, kind) {
            Some(point) => self.breakpoint(point, false),
            None => Ok(false),
        }
    }
}

struct EventLoop<B>(std::marker::PhantomData<B>);

impl<B: DebugTarget> run_blocking::BlockingEventLoop for EventLoop<B> {
    type Target = Remote<B>;
    type Connection = TcpStream;
    type StopReason = MultiThreadStopReason<u64>;

    fn wait_for_stop_reason(
        target: &mut Remote<B>,
        connection: &mut TcpStream,
    ) -> Result<
        run_blocking::Event<Self::StopReason>,
        run_blocking::WaitForStopReasonError<String, std::io::Error>,
    > {
        use run_blocking::{Event, WaitForStopReasonError as Error};
        loop {
            if ConnectionExt::peek(connection)
                .map_err(Error::Connection)?
                .is_some()
            {
                return Ok(Event::IncomingData(
                    ConnectionExt::read(connection).map_err(Error::Connection)?,
                ));
            }
            let status = target.status().map_err(Error::Target)?;
            if status.state == RunState::ShuttingDown {
                return Err(Error::Target(
                    "VM is shutting down; its final exit status is unavailable on this connection"
                        .into(),
                ));
            }
            if !status.stepping
                && status.state != RunState::Running
                && let Some(stop) = status.stop.filter(|stop| stop.sequence > target.sequence)
            {
                let thread = tid(stop.vcpu);
                let reason = match stop.reason {
                    StopReason::Breakpoint { breakpoint } => match breakpoint.kind {
                        BreakpointKind::Execute
                            if target.software.contains(&breakpoint.address) =>
                        {
                            MultiThreadStopReason::SwBreak(thread)
                        }
                        BreakpointKind::Execute => MultiThreadStopReason::HwBreak(thread),
                        kind => MultiThreadStopReason::Watch {
                            tid: thread,
                            kind: if kind == BreakpointKind::Write {
                                WatchKind::Write
                            } else {
                                WatchKind::ReadWrite
                            },
                            addr: breakpoint.address,
                        },
                    },
                    StopReason::Pause => MultiThreadStopReason::SignalWithThread {
                        tid: thread,
                        signal: Signal::SIGINT,
                    },
                    _ => MultiThreadStopReason::SignalWithThread {
                        tid: thread,
                        signal: Signal::SIGTRAP,
                    },
                };
                return Ok(Event::TargetStopped(reason));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn on_interrupt(target: &mut Remote<B>) -> Result<Option<Self::StopReason>, String> {
        target.backend.request(DebugRequest::Pause)?;
        Ok(Some(MultiThreadStopReason::Signal(Signal::SIGINT)))
    }
}

fn full_tag(registers: &X86Registers) -> u32 {
    let top = (registers.fsw >> 11) as usize & 7;
    (0..8).fold(0, |tag, physical| {
        let value = &registers.st[(physical + 8 - top) & 7];
        let significand = u64::from_le_bytes(value[..8].try_into().unwrap());
        let exponent = u16::from_le_bytes(value[8..].try_into().unwrap()) & 0x7fff;
        let entry = if registers.ftw & (1 << physical) == 0 {
            3
        } else if significand == 0 && exponent == 0 {
            1
        } else if exponent == 0 || exponent == 0x7fff || significand >> 63 == 0 {
            2
        } else {
            0
        };
        tag | (entry << (physical * 2))
    })
}

fn to_gdb(registers: &X86Registers) -> X86_64CoreRegs {
    let [cs, ss, ds, es, fs, gs] = registers.segments;
    X86_64CoreRegs {
        regs: registers.general[..16].try_into().unwrap(),
        rip: registers.general[16].wrapping_add(code_base(registers)),
        eflags: registers.general[17] as u32,
        segments: X86SegmentRegs {
            cs,
            ss,
            ds,
            es,
            fs,
            gs,
        },
        st: registers.st,
        fpu: X87FpuInternalRegs {
            fctrl: registers.fcw as u32,
            fstat: registers.fsw as u32,
            ftag: full_tag(registers),
            fop: registers.fop as u32,
            fioff: registers.fip as u32,
            fiseg: (registers.fip >> 32) as u32,
            fooff: registers.fdp as u32,
            foseg: (registers.fdp >> 32) as u32,
        },
        xmm: registers.xmm.map(u128::from_le_bytes),
        mxcsr: registers.mxcsr,
    }
}

fn from_gdb(regs: &X86_64CoreRegs, registers: &mut X86Registers) {
    registers.general[..16].copy_from_slice(&regs.regs);
    let base = if registers.system.get("cr0").copied().unwrap_or(1) & 1 == 0
        && registers.segments[0] != regs.segments.cs
    {
        u64::from(regs.segments.cs) << 4
    } else {
        code_base(registers)
    };
    registers.general[16] = regs.rip.wrapping_sub(base);
    registers.general[17] = regs.eflags as u64;
    let X86SegmentRegs {
        cs,
        ss,
        ds,
        es,
        fs,
        gs,
    } = regs.segments;
    registers.segments = [cs, ss, ds, es, fs, gs];
    registers.st = regs.st;
    registers.fcw = regs.fpu.fctrl as u16;
    registers.fsw = regs.fpu.fstat as u16;
    registers.ftw = (0..8).fold(0, |tag, i| {
        tag | (u8::from((regs.fpu.ftag >> (2 * i)) & 3 != 3) << i)
    });
    registers.fop = regs.fpu.fop as u16;
    registers.fip = u64::from(regs.fpu.fioff) | (u64::from(regs.fpu.fiseg) << 32);
    registers.fdp = u64::from(regs.fpu.fooff) | (u64::from(regs.fpu.foseg) << 32);
    registers.xmm = regs.xmm.map(u128::to_le_bytes);
    registers.mxcsr = regs.mxcsr;
}

fn code_base(registers: &X86Registers) -> u64 {
    if registers.system.get("cs_long").copied().unwrap_or(0) == 0 {
        registers.system.get("cs_base").copied().unwrap_or(0)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdbstub::arch::RegId;
    use gdbstub::stub::run_blocking::BlockingEventLoop;

    #[test]
    fn every_register_range_matches_the_architecture_codec() {
        let mut end = 0;
        for raw in 0..57 {
            let (register, size) = X86_64CoreRegId::from_raw_id(raw).unwrap();
            let range = register_range(register).unwrap();
            assert_eq!(range.start, end);
            assert_eq!(range.len(), size.unwrap().get());
            end = range.end;
        }
        let mut bytes = Vec::new();
        X86_64CoreRegs::default().gdb_serialize(|byte| bytes.push(byte.unwrap()));
        assert_eq!(end, bytes.len());
    }

    #[test]
    fn segmented_pc_and_x87_tags_roundtrip_without_changing_architectural_state() {
        let mut registers = X86Registers::default();
        registers.general[16] = 0xfff0;
        registers.general[17] = 2;
        registers.system.insert("cs_base".into(), 0xffff0000);
        registers.system.insert("cr0".into(), 0);
        registers.segments[0] = 0xf000;
        registers.ftw = 0b111;
        registers.st[0][..8].copy_from_slice(&(1u64 << 63).to_le_bytes());
        registers.st[0][8..].copy_from_slice(&0x3fffu16.to_le_bytes());
        registers.st[2][8..].copy_from_slice(&0x7fffu16.to_le_bytes());
        let encoded = to_gdb(&registers);
        assert_eq!(encoded.rip, 0xfffffff0);
        assert_eq!(encoded.fpu.ftag & 0x3f, 0b10_01_00);
        let mut decoded = registers.clone();
        from_gdb(&encoded, &mut decoded);
        assert_eq!(registers, decoded);
        registers.system.insert("cs_long".into(), 1);
        assert_eq!(to_gdb(&registers).rip, 0xfff0);
    }

    #[test]
    fn shutdown_does_not_fabricate_a_successful_target_exit() {
        struct Shutdown;
        impl DebugTarget for Shutdown {
            fn request(&mut self, request: DebugRequest) -> Result<DebugResponse, String> {
                assert_eq!(request, DebugRequest::Status);
                Ok(DebugResponse::Status {
                    status: DebugStatus {
                        state: RunState::ShuttingDown,
                        stepping: false,
                        vcpus: vec![0],
                        breakpoints: Vec::new(),
                        stop: None,
                    },
                })
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut connection, _) = listener.accept().unwrap();
        let mut target = Remote {
            backend: Shutdown,
            step: None,
            software: BTreeSet::new(),
            hardware: BTreeSet::new(),
            sequence: 0,
            continued: BTreeSet::new(),
            locked: false,
        };
        assert!(
            matches!(EventLoop::<Shutdown>::wait_for_stop_reason(&mut target, &mut connection),
            Err(run_blocking::WaitForStopReasonError::Target(message)) if message.contains("final exit status is unavailable"))
        );
    }
}
