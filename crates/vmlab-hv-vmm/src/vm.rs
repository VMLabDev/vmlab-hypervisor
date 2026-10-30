//! The assembled machine: shared state, the run-state machine, the exit
//! path, and the handle the control server drives it with.

use std::sync::Arc;

use kvm_ioctls::VcpuFd;
use parking_lot::{Condvar, Mutex};
use vmlab_hv_proto::control::{Event, RunState, ShutdownReason};

use crate::bus::{IoBus, MmioBus};
use crate::error::VmmError;
use crate::kvm::KvmVm;
use crate::layout::{IO_ALLOC_BASE, MMIO32_BASE, MMIO32_SIZE};
use crate::memory::GuestRam;
use crate::pci::PciRoot;
use crate::pm::{PmDevice, SleepSignal, SleepState};
use crate::vcpu::VcpuThread;

/// Why the machine stopped for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Shutdown(ShutdownReason),
    Error(String),
}

/// Broadcasts machine events to the control connections.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Everything the vCPU threads and the control threads share.
pub struct VmShared {
    pub(crate) kvm: Arc<KvmVm>,
    io_bus: Mutex<IoBus>,
    mmio_bus: Mutex<MmioBus>,
    pci_root: Arc<Mutex<PciRoot>>,
    pm: Arc<Mutex<PmDevice>>,
    sleep_signal: Arc<SleepSignal>,
    /// Window checks for PCI BAR dispatch: (mmio32, mmio64) as (base, end).
    pci_windows: ((u64, u64), (u64, u64)),
    state: Mutex<RunState>,
    state_cv: Condvar,
    exit: Mutex<Option<ExitReason>>,
    exit_cv: Condvar,
    kicks: Mutex<Vec<libc::pthread_t>>,
    event_sink: Mutex<Option<EventSink>>,
}

impl VmShared {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kvm: KvmVm,
        io_bus: IoBus,
        mmio_bus: MmioBus,
        pci_root: Arc<Mutex<PciRoot>>,
        pm: Arc<Mutex<PmDevice>>,
        sleep_signal: Arc<SleepSignal>,
        mmio64_base: u64,
    ) -> Arc<Self> {
        Arc::new(VmShared {
            kvm: Arc::new(kvm),
            io_bus: Mutex::new(io_bus),
            mmio_bus: Mutex::new(mmio_bus),
            pci_root,
            pm,
            sleep_signal,
            pci_windows: (
                (MMIO32_BASE, MMIO32_BASE + MMIO32_SIZE),
                (mmio64_base, mmio64_base + crate::layout::MMIO64_SIZE),
            ),
            state: Mutex::new(RunState::Paused),
            state_cv: Condvar::new(),
            exit: Mutex::new(None),
            exit_cv: Condvar::new(),
            kicks: Mutex::new(Vec::new()),
            event_sink: Mutex::new(None),
        })
    }

    /// Set the sink machine events are broadcast through (the control
    /// server). Must be set before vCPUs start.
    pub fn set_event_sink(&self, sink: EventSink) {
        *self.event_sink.lock() = Some(sink);
    }

    fn emit(&self, event: Event) {
        if let Some(sink) = self.event_sink.lock().as_ref() {
            sink(event);
        }
    }

    // ---- vCPU registry -------------------------------------------------

    pub(crate) fn register_vcpu(&self, tid: libc::pthread_t) {
        self.kicks.lock().push(tid);
    }

    pub(crate) fn unregister_vcpu(&self, tid: libc::pthread_t) {
        self.kicks.lock().retain(|t| *t != tid);
    }

    fn kick_all(&self) {
        let tids = self.kicks.lock().clone();
        for tid in tids {
            // SAFETY: pthread_kill(3) with a registered signal number on a
            // thread id from the registry; stale ids cannot exist because
            // unregister runs before the thread exits.
            unsafe {
                libc::pthread_kill(tid, libc::SIGUSR1);
            }
        }
    }

    // ---- run state -----------------------------------------------------

    pub fn status(&self) -> RunState {
        if self.exit.lock().is_some() {
            return RunState::ShuttingDown;
        }
        *self.state.lock()
    }

    /// Whether a vCPU thread may enter KVM_RUN.
    pub(crate) fn should_run(&self) -> bool {
        if self.exit.lock().is_some() {
            return false;
        }
        *self.state.lock() == RunState::Running
    }

    /// Block until the machine runs again or exits; called by parked vCPU
    /// threads.
    pub(crate) fn park(&self) {
        let mut state = self.state.lock();
        while *state != RunState::Running && self.exit.lock().is_none() {
            self.state_cv.wait(&mut state);
        }
    }

    /// Whether the machine has an exit reason.
    pub(crate) fn exited(&self) -> bool {
        self.exit.lock().is_some()
    }

    /// Block until the machine exits; returns the reason.
    pub(crate) fn wait_exit(&self) -> ExitReason {
        let mut exit = self.exit.lock();
        loop {
            if let Some(reason) = exit.clone() {
                return reason;
            }
            self.exit_cv.wait(&mut exit);
        }
    }

    /// Stop the machine for good, from any thread, exactly once.
    pub(crate) fn request_exit(&self, reason: ExitReason) {
        {
            let mut exit = self.exit.lock();
            if exit.is_some() {
                return;
            }
            *exit = Some(reason.clone());
        }
        *self.state.lock() = RunState::ShuttingDown;
        self.state_cv.notify_all();
        self.exit_cv.notify_all();
        self.kick_all();
        if let ExitReason::Shutdown(shutdown) = reason {
            self.emit(Event::Shutdown { reason: shutdown });
        }
    }

    pub(crate) fn take_guest_sleep(&self) -> Option<SleepState> {
        let sleep = self.sleep_signal.take()?;
        match sleep {
            // The PM device folds S4 into S5 already; treat it the same if
            // it ever surfaces.
            SleepState::S4 | SleepState::S5 => {
                self.request_exit(ExitReason::Shutdown(ShutdownReason::GuestShutdown))
            }
            SleepState::S3 => {
                let mut state = self.state.lock();
                if *state == RunState::Running {
                    *state = RunState::Suspended;
                    drop(state);
                    self.kick_all();
                    self.state_cv.notify_all();
                    self.emit(Event::Suspend);
                }
            }
        }
        Some(sleep)
    }

    // ---- dispatch (called from vCPU threads) ---------------------------

    pub(crate) fn io_read(&self, port: u16, data: &mut [u8]) {
        if port >= IO_ALLOC_BASE {
            let mut root = self.pci_root.lock();
            if root.dispatch_io_read(port, data) {
                return;
            }
        }
        self.io_bus.lock().read(port, data);
    }

    pub(crate) fn io_write(&self, port: u16, data: &[u8]) {
        if port >= IO_ALLOC_BASE {
            let mut root = self.pci_root.lock();
            if root.dispatch_io_write(port, data) {
                self.take_guest_sleep();
                return;
            }
        }
        self.io_bus.lock().write(port, data);
        self.take_guest_sleep();
    }

    pub(crate) fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        if self.in_pci_window(addr) {
            let mut root = self.pci_root.lock();
            if root.dispatch_mmio_read(addr, data) {
                return;
            }
        }
        self.mmio_bus.lock().read(addr, data);
    }

    pub(crate) fn mmio_write(&self, addr: u64, data: &[u8]) {
        if self.in_pci_window(addr) {
            let mut root = self.pci_root.lock();
            if root.dispatch_mmio_write(addr, data) {
                return;
            }
        }
        self.mmio_bus.lock().write(addr, data);
    }

    fn in_pci_window(&self, addr: u64) -> bool {
        let ((b32, e32), (b64, e64)) = self.pci_windows;
        (b32 <= addr && addr < e32) || (b64 <= addr && addr < e64)
    }
}

/// A running machine: the shared state plus the vCPU threads.
pub struct Vm {
    shared: Arc<VmShared>,
    vcpus: Vec<VcpuThread>,
    /// Guest RAM, kept for snapshot orchestration and later vhost-user.
    pub ram: GuestRam,
}

impl Vm {
    pub(crate) fn from_parts(
        shared: Arc<VmShared>,
        vcpu_fds: Vec<(u8, VcpuFd)>,
        ram: GuestRam,
    ) -> Result<Self, VmmError> {
        let vcpus = vcpu_fds
            .into_iter()
            .map(|(id, fd)| VcpuThread::spawn(id, fd, shared.clone()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| VmmError::Other(format!("spawning vCPU threads: {e}")))?;
        Ok(Vm { shared, vcpus, ram })
    }

    /// A handle for the control server and tests.
    pub fn handle(&self) -> VmHandle {
        VmHandle(self.shared.clone())
    }

    /// Start or resume the vCPUs (the `resume` command, or first start).
    pub fn start(&mut self) {
        let _ = self.handle().resume();
    }

    /// Block until the machine exits, joining the vCPU threads; returns why.
    pub fn wait(mut self) -> ExitReason {
        let reason = self.shared.wait_exit();
        for vcpu in self.vcpus.drain(..) {
            vcpu.join();
        }
        reason
    }
}

/// The control-server-facing view of a machine. Cheap to clone.
#[derive(Clone)]
pub struct VmHandle(Arc<VmShared>);

impl VmHandle {
    pub fn status(&self) -> RunState {
        self.0.status()
    }

    /// `pause`: stop issuing guest execution; emits `stopped`.
    pub fn pause(&self) -> Result<RunState, String> {
        let mut state = self.0.state.lock();
        match *state {
            RunState::Running => {
                *state = RunState::Paused;
                drop(state);
                self.0.kick_all();
                self.0.state_cv.notify_all();
                self.0.emit(Event::Stopped);
                Ok(RunState::Paused)
            }
            RunState::Paused | RunState::Suspended => Ok(*state),
            RunState::ShuttingDown => Err("the machine is shutting down".to_string()),
        }
    }

    /// `resume`: start execution; emits `resumed` (or `wakeup` from S3).
    pub fn resume(&self) -> Result<RunState, String> {
        let mut state = self.0.state.lock();
        if self.0.exit.lock().is_some() {
            return Err("the machine is shutting down".to_string());
        }
        let was = *state;
        match was {
            RunState::Paused | RunState::Suspended => {
                *state = RunState::Running;
                drop(state);
                self.0.state_cv.notify_all();
                self.0.emit(if was == RunState::Suspended {
                    Event::Wakeup
                } else {
                    Event::Resumed
                });
                Ok(RunState::Running)
            }
            RunState::Running => Ok(RunState::Running),
            RunState::ShuttingDown => Err("the machine is shutting down".to_string()),
        }
    }

    /// `powerdown`: press the ACPI power button.
    pub fn powerdown(&self) -> Result<RunState, String> {
        if self.0.exit.lock().is_some() {
            return Err("the machine is shutting down".to_string());
        }
        self.0.pm.lock().press_power_button();
        Ok(self.status())
    }

    /// `quit`: stop the machine immediately as a host request.
    pub fn quit(&self) {
        self.0
            .request_exit(ExitReason::Shutdown(ShutdownReason::HostRequest));
    }

    /// Block until the machine exits.
    pub fn wait_exit(&self) -> ExitReason {
        self.0.wait_exit()
    }

    /// Wire the event sink (used by the launcher).
    pub fn set_event_sink(&self, sink: EventSink) {
        self.0.set_event_sink(sink);
    }
}
