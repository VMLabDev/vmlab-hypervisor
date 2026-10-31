use super::{RAM_BASE, memory::PhysicalMemory};
use crate::{
    pci::PciRoot,
    snapshot::{DeviceNode, DiskNode, SavedSections, format},
    vm::ExitReason,
};
use parking_lot::{Condvar, Mutex};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    os::unix::net::UnixListener,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::riscv::{Clint, Plic, TIMEBASE_HZ};
use vmlab_hv_proto::{
    control::{Capability, Command, ErrorCode, Event, Outcome, RunState, ShutdownReason},
    machine::MachineDescription,
    server::Server,
};
use vmlab_hv_tcg::{CpuState, Riscv64, Value};

type Cpu = Riscv64<PhysicalMemory>;

struct Failure {
    code: ErrorCode,
    message: String,
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            code: ErrorCode::Internal,
            message,
        }
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl Failure {
    fn disk(error: vmlab_disk::Error) -> Self {
        let code = match &error {
            vmlab_disk::Error::SnapshotNotFound(_) => ErrorCode::SnapshotNotFound,
            vmlab_disk::Error::Unsupported(_) => ErrorCode::Unsupported,
            _ => ErrorCode::Internal,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
    fn format(error: format::FormatError) -> Self {
        let code = match &error {
            format::FormatError::VersionMismatch { .. }
            | format::FormatError::DigestMismatch { .. }
            | format::FormatError::NotVmstate(_) => ErrorCode::SnapshotIncompatible,
            _ => ErrorCode::Internal,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

fn snapshot_outcome(result: Result<RunState, Failure>) -> Outcome {
    match result {
        Ok(state) => Outcome::Ok {
            status: Some(state),
        },
        Err(error) => Outcome::Error {
            code: error.code,
            message: error.message,
        },
    }
}

struct RunControl {
    state: RunState,
    exit: Option<ExitReason>,
    elapsed: Duration,
    started: Instant,
    clock_ticks: u64,
}

pub(super) struct Shared {
    control: Mutex<RunControl>,
    changed: Condvar,
    operation: Mutex<()>,
    cpus: Vec<Mutex<Cpu>>,
    memory: PhysicalMemory,
    clint: Arc<Mutex<Clint>>,
    plic: Arc<Mutex<Plic>>,
    shutdown: Arc<AtomicU32>,
    nodes: Vec<DeviceNode>,
    disks: Vec<DiskNode>,
    pci: Arc<Mutex<PciRoot>>,
    digest: [u8; 32],
    events: Mutex<Option<Server<Handler>>>,
    pub(super) gpu: Option<vmlab_hv_devices::virtio::gpu::Gpu>,
    pub(super) xhci: Option<Arc<Mutex<vmlab_hv_devices::usb::XhciController>>>,
    display: Mutex<Option<Arc<vmlab_hv_display::DisplayServer>>>,
    rtc: Arc<Mutex<vmlab_hv_devices::riscv::rtc::GoldfishRtc>>,
    snapshots_enabled: bool,
}

type Handler = Box<dyn Fn(Command) -> Outcome + Send + Sync>;

#[derive(Clone)]
pub struct VirtHandle(Arc<Shared>);

impl VirtHandle {
    pub fn quit(&self) {
        self.0
            .exit(ExitReason::Shutdown(ShutdownReason::HostRequest));
    }
}

pub struct VirtVm {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
    display_workers: Vec<JoinHandle<()>>,
}

impl VirtVm {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        desc: &MachineDescription,
        cpus: Vec<Mutex<Cpu>>,
        memory: PhysicalMemory,
        clint: Arc<Mutex<Clint>>,
        plic: Arc<Mutex<Plic>>,
        shutdown: Arc<AtomicU32>,
        nodes: Vec<DeviceNode>,
        disks: Vec<DiskNode>,
        pci: Arc<Mutex<PciRoot>>,
        gpu: Option<vmlab_hv_devices::virtio::gpu::Gpu>,
        xhci: Option<Arc<Mutex<vmlab_hv_devices::usb::XhciController>>>,
        rtc: Arc<Mutex<vmlab_hv_devices::riscv::rtc::GoldfishRtc>>,
    ) -> Result<Self, String> {
        let digest = Sha256::digest(serde_json::to_vec(desc).map_err(|e| e.to_string())?).into();
        let shared = Arc::new(Shared {
            control: Mutex::new(RunControl {
                state: RunState::Paused,
                exit: None,
                elapsed: Duration::ZERO,
                started: Instant::now(),
                clock_ticks: 0,
            }),
            changed: Condvar::new(),
            operation: Mutex::new(()),
            cpus,
            memory,
            clint,
            plic,
            shutdown,
            nodes,
            disks,
            pci,
            digest,
            events: Mutex::new(None),
            display: Mutex::new(None),
            gpu,
            xhci,
            rtc,
            snapshots_enabled: desc.disks.iter().filter(|disk| disk.vmstate).count() == 1
                && desc.disks.iter().all(|disk| {
                    disk.read_only || disk.format == vmlab_hv_proto::machine::DiskFormat::Qcow2
                }),
        });
        let mut vm = Self {
            shared,
            workers: Vec::new(),
            server: None,
            display_workers: Vec::new(),
        };
        for hart in 0..vm.shared.cpus.len() {
            let shared = vm.shared.clone();
            vm.workers.push(
                std::thread::Builder::new()
                    .name(format!("riscv-hart-{hart}"))
                    .spawn(move || {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            shared.run(hart)
                        }))
                        .is_err()
                        {
                            shared.exit(ExitReason::Error(format!("RISC-V hart {hart} panicked")));
                        }
                    })
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(vm)
    }

    pub fn handle(&self) -> VirtHandle {
        VirtHandle(self.shared.clone())
    }

    pub fn serve(&mut self, path: &Path) -> Result<(), String> {
        if path.exists() {
            return Err(format!("control socket {} already exists", path.display()));
        }
        let listener = UnixListener::bind(path).map_err(|e| e.to_string())?;
        let weak = Arc::downgrade(&self.shared);
        let handler: Handler = Box::new(move |command| match weak.upgrade() {
            Some(shared) => shared.command(command),
            None => Outcome::Error {
                code: ErrorCode::InvalidState,
                message: "machine exited".into(),
            },
        });
        let mut capabilities = if self.shared.snapshots_enabled {
            vec![Capability::Snapshots]
        } else {
            Vec::new()
        };
        if self.shared.gpu.is_some() {
            capabilities.extend([Capability::Display, Capability::Input]);
        }
        let server = Server::new(env!("CARGO_PKG_VERSION"), capabilities, handler);
        *self.shared.events.lock() = Some(server.clone());
        let weak = Arc::downgrade(&self.shared);
        self.server = Some(
            std::thread::Builder::new()
                .name("riscv-control".into())
                .spawn(move || {
                    if let Err(e) = server.serve_until(listener, || {
                        weak.upgrade()
                            .is_none_or(|s| s.control.lock().exit.is_some())
                    }) {
                        eprintln!("RISC-V control: {e}");
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }

    pub fn start(&self) {
        let _ = self.shared.resume();
    }

    pub fn serve_display(&mut self, native: &Path, rfb: &Path) -> Result<(), String> {
        let (server, workers) = super::display::serve(&self.shared, native, rfb)?;
        *self.shared.display.lock() = Some(server);
        self.display_workers = workers;
        Ok(())
    }

    pub fn wait(&mut self) -> Result<ExitReason, String> {
        let mut control = self.shared.control.lock();
        while control.exit.is_none() {
            self.shared.changed.wait(&mut control);
        }
        let reason = control.exit.clone().unwrap();
        drop(control);
        for worker in self.workers.drain(..) {
            worker.join().map_err(|_| "RISC-V hart panicked")?;
        }
        if let Some(server) = self.server.take() {
            server.join().map_err(|_| "RISC-V control panicked")?;
        }
        for worker in self.display_workers.drain(..) {
            worker.join().map_err(|_| "RISC-V display panicked")?;
        }
        Ok(reason)
    }
}

impl Drop for VirtVm {
    fn drop(&mut self) {
        self.handle().quit();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        for worker in self.display_workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Shared {
    pub(super) fn exited(&self) -> bool {
        self.control.lock().exit.is_some()
    }
    fn emit(&self, event: Event) {
        if let Some(server) = self.events.lock().as_ref() {
            server.broadcast(event);
        }
    }

    pub(super) fn exit(&self, reason: ExitReason) {
        let mut control = self.control.lock();
        if control.exit.is_some() {
            return;
        }
        control.state = RunState::ShuttingDown;
        control.exit = Some(reason.clone());
        self.changed.notify_all();
        drop(control);
        if let ExitReason::Shutdown(reason) = reason {
            self.emit(Event::Shutdown { reason });
        }
    }

    fn pause(&self) -> Result<RunState, String> {
        let mut control = self.control.lock();
        if control.exit.is_some() {
            return Err("machine has exited".into());
        }
        if control.state == RunState::Paused {
            return Ok(RunState::Paused);
        }
        if control.state == RunState::Running {
            let elapsed = control.started.elapsed();
            control.elapsed += elapsed;
            control.state = RunState::Paused;
        }
        drop(control);
        for cpu in &self.cpus {
            drop(cpu.lock());
        }
        self.emit(Event::Stopped);
        Ok(RunState::Paused)
    }

    fn resume(&self) -> Result<RunState, String> {
        let mut control = self.control.lock();
        if control.exit.is_some() {
            return Err("machine has exited".into());
        }
        if control.state == RunState::Running {
            return Ok(RunState::Running);
        }
        control.started = Instant::now();
        control.state = RunState::Running;
        self.changed.notify_all();
        drop(control);
        self.emit(Event::Resumed);
        Ok(RunState::Running)
    }

    fn run(&self, hart: usize) {
        let mut last_progress = Instant::now();
        loop {
            let mut control = self.control.lock();
            while control.state != RunState::Running && control.exit.is_none() {
                self.changed.wait(&mut control);
            }
            if control.exit.is_some() {
                break;
            }
            let mut cpu = self.cpus[hart].lock();
            let ticks = ((control.elapsed + control.started.elapsed()).as_nanos()
                * TIMEBASE_HZ as u128
                / 1_000_000_000) as u64;
            let mut clint = self.clint.lock();
            let delta = ticks.wrapping_sub(control.clock_ticks);
            clint.advance(delta);
            self.rtc.lock().advance(delta.saturating_mul(100));
            control.clock_ticks = ticks;
            let time = clint.time();
            let compare = clint.compare(hart);
            let software = clint.pending(hart) & (1 << 3) != 0;
            drop(clint);
            drop(control);
            let globals = &mut cpu.interpreter.globals;
            globals.insert("zmtime".into(), Value::bits(64, time));
            globals.insert("zmtimecmp".into(), Value::bits(64, compare));
            globals.insert("zmsip".into(), Value::bits(32, u32::from(software)));
            if let Some(Value::Struct(mut fields)) = globals.get("zmip").cloned() {
                let bits = fields["zbits"].u64().unwrap_or(0);
                fields.insert(
                    "zbits".into(),
                    Value::bits(
                        64,
                        (bits & !((1 << 9) | (1 << 11))) | self.plic.lock().pending(hart),
                    ),
                );
                globals.insert("zmip".into(), Value::Struct(fields));
            }
            let result = cpu.run(256);
            let pc = cpu.pc().unwrap_or(0);
            let steps = cpu.steps;
            if std::env::var_os("VMLAB_HV_DIAGNOSTICS").is_some()
                && last_progress.elapsed() >= Duration::from_secs(10)
            {
                eprintln!(
                    "RISC-V hart {hart}: PC={pc:#x}, steps={steps}, native={}, recordings={}, mcause={:?}, mepc={:?}",
                    cpu.specialization_stats.hits,
                    cpu.specialization_stats.recordings,
                    cpu.interpreter.globals.get("zmcause"),
                    cpu.interpreter.globals.get("zmepc")
                );
                last_progress = Instant::now();
            }
            drop(cpu);
            if let Some(xhci) = &self.xhci {
                xhci.lock().poll();
            }
            let shutdown = self.shutdown.load(Ordering::SeqCst);
            if shutdown != 0 {
                self.exit(if shutdown & 0xffff == 0x3333 {
                    ExitReason::Error(format!("SiFive test failed: {:#x}", shutdown >> 16))
                } else {
                    ExitReason::Shutdown(if shutdown == 0x7777 {
                        ShutdownReason::GuestReset
                    } else {
                        ShutdownReason::GuestShutdown
                    })
                });
                break;
            }
            match result {
                Err(e) => {
                    self.exit(ExitReason::Error(format!(
                        "hart {hart}, PC {pc:#x}, steps {steps}: {e}"
                    )));
                    break;
                }
                Ok((_, true)) => {
                    let mut control = self.control.lock();
                    if control.exit.is_none() {
                        self.changed
                            .wait_for(&mut control, Duration::from_millis(1));
                    }
                }
                _ => {}
            }
        }
    }

    fn command(&self, command: Command) -> Outcome {
        let _operation = self.operation.lock();
        let result = match command {
            Command::Status => Ok(self.control.lock().state),
            Command::Pause => self.pause(),
            Command::Resume => self.resume(),
            Command::Quit => {
                self.exit(ExitReason::Shutdown(ShutdownReason::HostRequest));
                Ok(RunState::ShuttingDown)
            }
            Command::SnapshotSave { name } => {
                return snapshot_outcome(
                    self.snapshot(&name, false)
                        .map(|_| self.control.lock().state),
                );
            }
            Command::SnapshotLoad { name } => {
                return snapshot_outcome(
                    self.snapshot(&name, true)
                        .map(|_| self.control.lock().state),
                );
            }
            Command::SnapshotDelete { name } => {
                return snapshot_outcome(
                    self.delete_snapshot(&name)
                        .map(|_| self.control.lock().state),
                );
            }
            _ => {
                return Outcome::Error {
                    code: ErrorCode::Unsupported,
                    message: "command is not provided by the RISC-V virt platform".into(),
                };
            }
        };
        match result {
            Ok(state) => Outcome::Ok {
                status: Some(state),
            },
            Err(message) => Outcome::Error {
                code: ErrorCode::Internal,
                message,
            },
        }
    }

    fn delete_snapshot(&self, name: &str) -> Result<(), Failure> {
        for disk in self.disks.iter().filter(|d| d.writable) {
            disk.image
                .lock()
                .snapshot_delete(name)
                .map_err(Failure::disk)?;
        }
        Ok(())
    }

    fn snapshot(&self, name: &str, load: bool) -> Result<(), Failure> {
        if !self.snapshots_enabled {
            return Err(Failure {
                code: ErrorCode::Unsupported,
                message:
                    "online snapshots require writable qcow2 disks and a designated vmstate disk"
                        .into(),
            });
        }
        let vmstate = self
            .disks
            .iter()
            .filter(|d| d.writable && d.vmstate)
            .collect::<Vec<_>>();
        if vmstate.len() != 1 {
            return Err(Failure {
                code: ErrorCode::Unsupported,
                message: "online snapshots require exactly one writable vmstate disk".into(),
            });
        }
        let was = self.control.lock().state;
        self.pause()?;
        for node in &self.nodes {
            match node {
                DeviceNode::Plain(d) => d.lock().quiesce(),
                DeviceNode::Resettable(d) => d.lock().quiesce(),
                DeviceNode::Virtio(d) => d.lock().quiesce(),
            }
        }
        let result = if load {
            self.load_snapshot(name, vmstate[0])
        } else {
            self.save_snapshot(name, vmstate[0], was).map(|_| was)
        };
        for node in &self.nodes {
            match node {
                DeviceNode::Plain(d) => d.lock().resume_snapshot(),
                DeviceNode::Resettable(d) => d.lock().resume_snapshot(),
                DeviceNode::Virtio(d) => d.lock().resume_snapshot(),
            }
        }
        if result
            .as_ref()
            .is_ok_and(|state| *state == RunState::Running)
            || (!load && was == RunState::Running)
        {
            self.resume()?;
        }
        result.map(|_| ())
    }

    fn save_snapshot(&self, name: &str, vmstate: &DiskNode, was: RunState) -> Result<(), Failure> {
        let mut sections = Vec::new();
        for (hart, cpu) in self.cpus.iter().enumerate() {
            sections.push((
                format!("riscv-cpu/{hart}"),
                serde_json::to_vec(&cpu.lock().save_state()).map_err(|e| e.to_string())?,
            ));
        }
        for node in &self.nodes {
            sections.extend(node.save()?);
        }
        let clock = self.control.lock();
        sections.push((
            "riscv-clock".into(),
            serde_json::to_vec(&(clock.elapsed.as_nanos() as u64, clock.clock_ticks))
                .map_err(|e| e.to_string())?,
        ));
        drop(clock);
        sections.push((
            "pci-root".into(),
            serde_json::to_vec(&self.pci.lock().save_bar_states()).map_err(|e| e.to_string())?,
        ));
        let state = if was == RunState::Running {
            format::RUN_RUNNING
        } else {
            format::RUN_PAUSED
        };
        let mut bytes = format::encode_header_sections(
            state,
            env!("CARGO_PKG_VERSION"),
            &self.digest,
            &sections,
        );
        bytes.extend(format::encode_ram_regions(
            std::iter::once((RAM_BASE, self.memory.size)),
            |address, page| {
                self.memory
                    .ram
                    .read_slice(page, GuestAddress(address))
                    .map_err(|e| e.to_string())
            },
        )?);
        vmstate
            .image
            .lock()
            .write_vmstate(0, &bytes)
            .map_err(Failure::disk)?;
        for disk in self.disks.iter().filter(|d| d.writable) {
            let mut image = disk.image.lock();
            image.flush().map_err(Failure::disk)?;
            image
                .snapshot_create(name, if disk.vmstate { bytes.len() as u64 } else { 0 }, 0)
                .map_err(Failure::disk)?;
        }
        Ok(())
    }

    fn load_snapshot(&self, name: &str, vmstate: &DiskNode) -> Result<RunState, Failure> {
        let mut length = 0;
        for disk in self.disks.iter().filter(|d| d.writable) {
            let info = disk
                .image
                .lock()
                .snapshot_apply(name)
                .map_err(Failure::disk)?;
            if disk.vmstate {
                length = info.vm_state_size;
            }
        }
        let mut bytes = vec![0; usize::try_from(length).map_err(|e| e.to_string())?];
        vmstate
            .image
            .lock()
            .read_vmstate(0, &mut bytes)
            .map_err(Failure::disk)?;
        let (header, mut reader) = format::decode_header(&bytes).map_err(Failure::format)?;
        if header.digest != self.digest {
            return Err(Failure {
                code: ErrorCode::SnapshotIncompatible,
                message: "snapshot machine-description digest differs".into(),
            });
        }
        let mut sections = BTreeMap::new();
        for _ in 0..header.section_count {
            let (key, value) = format::read_section(&mut reader).map_err(|e| e.to_string())?;
            if sections.insert(key, value).is_some() {
                return Err("duplicate snapshot section".into());
            }
        }
        let regions = format::decode_ram_regions(&mut reader, |address, data| {
            self.memory
                .ram
                .write_slice(data, GuestAddress(address))
                .map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())?;
        if regions != [(RAM_BASE, self.memory.size)] || !reader.done() {
            return Err("snapshot RAM layout differs".into());
        }
        for (hart, cpu) in self.cpus.iter().enumerate() {
            let state: CpuState = serde_json::from_slice(
                sections
                    .get(&format!("riscv-cpu/{hart}"))
                    .ok_or("missing CPU state")?,
            )
            .map_err(|e| e.to_string())?;
            cpu.lock()
                .restore_state(&state)
                .map_err(|e| e.to_string())?;
        }
        let (elapsed, ticks): (u64, u64) =
            serde_json::from_slice(sections.get("riscv-clock").ok_or("missing clock state")?)
                .map_err(|e| e.to_string())?;
        let mut clock = self.control.lock();
        clock.elapsed = Duration::from_nanos(elapsed);
        clock.clock_ticks = ticks;
        drop(clock);
        let pci: Vec<(u8, Vec<(u64, bool)>)> =
            serde_json::from_slice(sections.get("pci-root").ok_or("missing PCI root")?)
                .map_err(|e| e.to_string())?;
        self.pci.lock().restore_bar_states(&pci)?;
        let mut saved = SavedSections::default();
        for (key, value) in sections {
            saved.insert(key, value);
        }
        for node in &self.nodes {
            node.restore(&saved)?;
        }
        Ok(if header.run_state == format::RUN_RUNNING {
            RunState::Running
        } else {
            RunState::Paused
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_errors_keep_protocol_codes_and_versions() {
        assert!(matches!(
            snapshot_outcome(Err(Failure::disk(vmlab_disk::Error::SnapshotNotFound(
                "missing".into()
            )))),
            Outcome::Error {
                code: ErrorCode::SnapshotNotFound,
                ..
            }
        ));
        assert!(matches!(
            snapshot_outcome(Err(Failure::disk(vmlab_disk::Error::Unsupported(
                "raw".into()
            )))),
            Outcome::Error {
                code: ErrorCode::Unsupported,
                ..
            }
        ));
        let error = Failure::format(format::FormatError::VersionMismatch {
            saved_major: 99,
            saved_minor: 2,
        });
        assert!(
            matches!(snapshot_outcome(Err(error)), Outcome::Error { code: ErrorCode::SnapshotIncompatible, message } if message.contains("99.2") && message.contains("1.0"))
        );
    }
}
