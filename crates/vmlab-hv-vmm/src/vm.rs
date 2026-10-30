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
    /// vCPU file descriptors, shared with the snapshot path: the owning
    /// thread holds each mutex across KVM_RUN, snapshots take it while the
    /// thread is parked.
    pub(crate) vcpus: Mutex<Vec<(u8, Arc<Mutex<VcpuFd>>)>>,
    /// How many vCPU threads are parked (out of KVM_RUN and waiting on
    /// `state_cv`); a snapshot waits for this to reach the vCPU count.
    parked: Mutex<usize>,
    parked_cv: Condvar,
    /// Guest RAM, shared with the snapshot path and later vhost-user.
    ram: Mutex<Option<Arc<GuestRam>>>,
    /// What snapshot orchestration needs: machine digest, writable disks,
    /// and every device with guest-visible state.
    snapshot: Mutex<Option<Arc<crate::snapshot::SnapshotRegistry>>>,
    /// Serialises snapshot operations against run-state changes and each
    /// other: save/load hold it across the whole operation, `pause` and
    /// `resume` take it briefly.
    snapshot_op: Mutex<()>,
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
            vcpus: Mutex::new(Vec::new()),
            parked: Mutex::new(0),
            parked_cv: Condvar::new(),
            ram: Mutex::new(None),
            snapshot: Mutex::new(None),
            snapshot_op: Mutex::new(()),
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
    /// threads. Parking is counted in `parked` so a snapshot can wait for
    /// every vCPU to leave KVM_RUN before it reads machine state.
    pub(crate) fn park(&self) {
        {
            let mut parked = self.parked.lock();
            *parked += 1;
            self.parked_cv.notify_all();
        }
        let mut state = self.state.lock();
        while *state != RunState::Running && self.exit.lock().is_none() {
            self.state_cv.wait(&mut state);
        }
        drop(state);
        let mut parked = self.parked.lock();
        *parked -= 1;
        self.parked_cv.notify_all();
    }

    /// Wait until every vCPU thread has parked, or the machine exits.
    /// Errors after `timeout` so a wedged guest cannot hang the caller.
    pub(crate) fn wait_quiesced(&self, timeout: std::time::Duration) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut parked = self.parked.lock();
        loop {
            let total = self.vcpus.lock().len();
            if self.exit.lock().is_some() || *parked >= total {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "only {parked} of {total} vCPUs quiesced within {timeout:?}"
                ));
            }
            self.parked_cv.wait_until(&mut parked, deadline);
        }
    }

    /// Register what snapshots need; the builder calls this once.
    pub(crate) fn set_snapshot_registry(&self, registry: Arc<crate::snapshot::SnapshotRegistry>) {
        *self.snapshot.lock() = Some(registry);
    }

    /// The machine's snapshot registry, when the machine is built.
    pub(crate) fn snapshot_registry(&self) -> Option<Arc<crate::snapshot::SnapshotRegistry>> {
        self.snapshot.lock().clone()
    }

    /// Guest RAM, when the machine is assembled.
    pub(crate) fn ram(&self) -> Option<Arc<GuestRam>> {
        self.ram.lock().clone()
    }

    /// Take the snapshot-operation lock; save and load hold it across the
    /// whole operation so run-state changes cannot interleave.
    pub(crate) fn begin_snapshot(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.snapshot_op.lock()
    }

    /// Pause the vCPUs and wait until every one has left KVM_RUN. Returns
    /// the run state the machine had, for the header and for
    /// [`VmShared::finish_snapshot`]. The caller holds the snapshot-op
    /// lock.
    pub(crate) fn stop_for_snapshot(
        &self,
        timeout: std::time::Duration,
    ) -> Result<RunState, String> {
        let was = {
            let mut state = self.state.lock();
            match *state {
                RunState::ShuttingDown => return Err("the machine is shutting down".into()),
                RunState::Running => {
                    *state = RunState::Paused;
                    RunState::Running
                }
                was => was,
            }
        };
        if was == RunState::Running {
            self.kick_all();
            self.state_cv.notify_all();
            self.emit(Event::Stopped);
        }
        self.wait_quiesced(timeout)
            .map_err(|e| format!("pausing for a snapshot: {e}"))?;
        Ok(was)
    }

    /// Leave the machine in the run state a snapshot recorded: paused
    /// snapshots stay paused, running ones resume, suspended ones hang back
    /// in S3. The caller holds the snapshot-op lock.
    pub(crate) fn finish_snapshot(&self, run_code: u8) {
        match run_code {
            crate::snapshot::format::RUN_RUNNING => {
                let mut state = self.state.lock();
                if *state == RunState::Paused {
                    *state = RunState::Running;
                    drop(state);
                    self.state_cv.notify_all();
                    self.emit(Event::Resumed);
                }
            }
            crate::snapshot::format::RUN_SUSPENDED => {
                let mut state = self.state.lock();
                if *state == RunState::Paused {
                    *state = RunState::Suspended;
                    drop(state);
                    self.state_cv.notify_all();
                }
            }
            _ => {}
        }
    }

    /// The PCI root's BAR state: programmed addresses and mid-sizing
    /// flags, per slot, in registration order.
    pub(crate) fn save_pci_bars(&self) -> Vec<(u8, Vec<(u64, bool)>)> {
        self.pci_root.lock().save_bar_states()
    }

    /// Restore the PCI root's BAR state from its snapshot section.
    pub(crate) fn restore_pci_bars(&self, bytes: &[u8]) -> Result<(), String> {
        let states: Vec<(u8, Vec<(u64, bool)>)> =
            serde_json::from_slice(bytes).map_err(|e| format!("pci-root state: {e}"))?;
        self.pci_root.lock().restore_bar_states(&states)
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
    pub ram: Arc<GuestRam>,
}

impl Vm {
    pub(crate) fn from_parts(
        shared: Arc<VmShared>,
        vcpu_fds: Vec<(u8, VcpuFd)>,
        ram: GuestRam,
    ) -> Result<Self, VmmError> {
        let ram = Arc::new(ram);
        *shared.ram.lock() = Some(ram.clone());
        let fds: Vec<(u8, Arc<Mutex<VcpuFd>>)> = vcpu_fds
            .into_iter()
            .map(|(id, fd)| (id, Arc::new(Mutex::new(fd))))
            .collect();
        *shared.vcpus.lock() = fds.clone();
        let vcpus = fds
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
    /// The shared machine state, for snapshot orchestration in this crate.
    pub(crate) fn shared(&self) -> &Arc<VmShared> {
        &self.0
    }

    pub fn status(&self) -> RunState {
        self.0.status()
    }

    /// `pause`: stop issuing guest execution; emits `stopped`.
    pub fn pause(&self) -> Result<RunState, String> {
        let _op = self.0.snapshot_op.lock();
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
        let _op = self.0.snapshot_op.lock();
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
